//! 对象保护租约（`object_leases`/`backup_leases` 表）（`docs/design.md` §7.2、§19.1、§19.4）。
//!
//! 边界：本模块只记录“哪些内容对象在什么时刻之前不许被 GC”。它不搬运字节、不写清单、不执行 GC，
//! 也不判定备份是否完整——那分别属对象层（S033）、备份准备（S087）与 GC（S085—S086）。
//!
//! 不变量：
//!
//! - **租约只能保护已登记对象**：`object_leases` 的外键指向 `objects(vault_id, hash)`，所以“给一个
//!   文件系统里还不存在的哈希写租约”直接失败（它保护不了任何字节）；反向的 `ON DELETE RESTRICT`
//!   让“删除仍被租约保护的对象行”也失败。GC 因此只需把未过期租约当作额外引用（design §19.4）。
//! - **有效期是唯一判据**：租约在 `expires_at > now` 时有效，`expires_at <= now` 即失效；本层不读
//!   时钟，`now` 由调用方注入（design §5.4）。已过期的租约不得再写入（[`LeaseError::AlreadyExpired`]）。
//! - **下载与提交用同一种机制、不同用途**：`download` 保护固定版本下载/恢复/释放检查期间的版本内容
//!   （design §11“跨越 30 天历史到期窗口的下载、恢复和释放检查通过短期内容租约或等价机制保护其
//!   版本”）；`commit` 保护“对象已落盘、版本尚未引用”的发布窗口（design §11.2 步骤 3—7）。
//! - **释放是幂等的**：导出结束或提交完成后的 release 可以重试，第二次返回 `false` 而不是报错，
//!   因为“租约已经不在了”与“我刚删掉它”对调用方是同一个结果。写入相反：同一身份重复写租约报
//!   [`LeaseError::DuplicateId`]，不覆盖——覆盖会静默改变保护范围。
//! - **备份租约覆盖多个 Vault**（design §7.2 `backup_leases.snapshot_seq_map`）：表里没有 `vault_id`，
//!   因此读接口不带 owner scope；把租约暴露给管理 API 时的鉴权属 S087。
//! - **快照映射是规范文本**：`snapshot_seq_map` 以 Vault 身份的小写十六进制为键、`VaultSeq` 的规范
//!   十进制文本为值（design §7.1：计数在文本里传，不传浮点可能丢精度的 JSON number）。同一个映射
//!   只有一种写法，因此“备份覆盖了哪些 Vault 的哪个 seq”可以直接比较。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{BackupLeaseId, ID_LEN, ObjectLeaseId, UserId, VaultId};
use vaultsyncd_domain::version::{UtcTimestamp, VaultSeq};

use crate::versions::{self, ObjectHash, VersionError};

/// `object_leases` 表的列顺序，读取点共用。
const OBJECT_LEASE_COLUMNS: &str = "l.id, l.vault_id, l.object_hash, l.purpose, l.expires_at";

/// `backup_leases` 表的列顺序，读取点共用。
const BACKUP_LEASE_COLUMNS: &str = "id, snapshot_seq_map, expires_at, manifest_ref";

/// 对象保护租约的用途（`object_leases.purpose`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectLeasePurpose {
    /// 固定版本下载/恢复/释放检查期间保护其内容不被 GC（design §11、§19.4）。
    Download,
    /// 对象已落盘、版本尚未引用时保护发布窗口（design §11.2 步骤 3—7）。
    Commit,
}

impl ObjectLeasePurpose {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Commit => "commit",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "download" => Some(Self::Download),
            "commit" => Some(Self::Commit),
            _ => None,
        }
    }
}

/// 新建一条对象租约的输入。
///
/// 不变量（违反时 [`lease_object`] 报错且不写入）：对象已由 [`crate::versions::record_object`]
/// 登记在该 Vault；`expires_at` 晚于调用方给出的当前时刻；`id` 未被占用。
#[derive(Clone, Copy, Debug)]
pub struct NewObjectLease {
    /// 租约身份。
    pub id: ObjectLeaseId,
    /// 对象所属 Vault。
    pub vault: VaultId,
    /// 被保护的内容哈希。
    pub object_hash: ObjectHash,
    /// 用途。
    pub purpose: ObjectLeasePurpose,
    /// 失效时刻。
    pub expires_at: UtcTimestamp,
}

/// 一条对象租约（`object_leases` 一行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectLease {
    /// 租约身份。
    pub id: ObjectLeaseId,
    /// 对象所属 Vault。
    pub vault: VaultId,
    /// 被保护的内容哈希。
    pub object_hash: ObjectHash,
    /// 用途。
    pub purpose: ObjectLeasePurpose,
    /// 失效时刻。
    pub expires_at: UtcTimestamp,
}

/// 备份租约里的快照映射：每个 Vault 在准备备份时已确认的 seq（design §19.1 步骤 2）。
///
/// 构造时按 Vault 身份升序规范化并拒绝空映射与重复 Vault，因此 `entries()` 的顺序就是编码顺序，
/// 同一个映射只有一种 JSON 文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotSeqMap {
    entries: Vec<(VaultId, VaultSeq)>,
}

impl SnapshotSeqMap {
    /// 由 `(Vault, seq)` 列表构造；空列表或同一 Vault 出现两次都报错。
    pub fn new(entries: Vec<(VaultId, VaultSeq)>) -> Result<Self, LeaseError> {
        if entries.is_empty() {
            return Err(LeaseError::EmptySnapshotSeqMap);
        }
        let mut entries = entries;
        entries.sort_by_key(|(vault, _)| *vault);
        for pair in entries.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(LeaseError::DuplicateSnapshotVault { vault: pair[0].0 });
            }
        }
        Ok(Self { entries })
    }

    /// 规范顺序的映射条目（按 Vault 身份升序）。
    pub fn entries(&self) -> &[(VaultId, VaultSeq)] {
        &self.entries
    }

    /// 规范 JSON object 文本：键是 Vault 身份的小写十六进制，值是规范十进制 seq 文本。
    pub fn to_json(&self) -> String {
        let mut text = String::from("{");
        for (index, (vault, seq)) in self.entries.iter().enumerate() {
            if index > 0 {
                text.push(',');
            }
            text.push('"');
            text.push_str(&vault.to_hex());
            text.push_str("\":\"");
            text.push_str(&seq.to_decimal());
            text.push('"');
        }
        text.push('}');
        text
    }

    /// 解析库里的 `snapshot_seq_map` 文本。
    ///
    /// 用 SQLite 自带的 JSON 函数逐项展开（本 crate 不引入第二套 JSON 解析依赖），并要求键是 32 位
    /// 十六进制、值是文本类型的规范十进制，因此“值被写成 JSON number”这种半规范数据会被拒绝，
    /// 而不是悄悄接受两种写法。
    pub async fn from_json(conn: &mut SqliteConnection, text: &str) -> Result<Self, LeaseError> {
        let is_object: i64 = sqlx::query_scalar(
            "SELECT CASE WHEN json_valid(?1) THEN json_type(?1) = 'object' ELSE 0 END",
        )
        .bind(text)
        .fetch_one(&mut *conn)
        .await
        .map_err(LeaseError::Db)?;
        if is_object != 1 {
            return Err(LeaseError::MalformedRow {
                column: "snapshot_seq_map",
            });
        }
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT CAST(key AS TEXT), CAST(type AS TEXT), CAST(value AS TEXT) \
             FROM json_each(?1) ORDER BY key",
        )
        .bind(text)
        .fetch_all(&mut *conn)
        .await
        .map_err(LeaseError::Db)?;
        let mut entries = Vec::with_capacity(rows.len());
        for (key, kind, value) in rows {
            if kind != "text" {
                return Err(LeaseError::MalformedRow {
                    column: "snapshot_seq_map",
                });
            }
            let vault = VaultId::from_hex(&key).map_err(|_| LeaseError::MalformedRow {
                column: "snapshot_seq_map",
            })?;
            let seq = VaultSeq::from_decimal(&value).map_err(|_| LeaseError::MalformedRow {
                column: "snapshot_seq_map",
            })?;
            entries.push((vault, seq));
        }
        Self::new(entries)
    }
}

/// 新建一条备份租约的输入。
///
/// 不变量（违反时 [`insert_backup_lease`] 报错且不写入）：`snapshot_seq_map` 非空且其中的 Vault
/// 都已注册；`manifest_ref` 非空；`expires_at` 晚于调用方给出的当前时刻；`id` 未被占用。
#[derive(Clone, Debug)]
pub struct NewBackupLease {
    /// 租约身份。
    pub id: BackupLeaseId,
    /// 各 Vault 已确认的 seq。
    pub snapshot_seq_map: SnapshotSeqMap,
    /// 失效时刻。
    pub expires_at: UtcTimestamp,
    /// 备份清单位置。
    pub manifest_ref: String,
}

/// 一条备份租约（`backup_leases` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupLease {
    /// 租约身份。
    pub id: BackupLeaseId,
    /// 各 Vault 已确认的 seq。
    pub snapshot_seq_map: SnapshotSeqMap,
    /// 失效时刻。
    pub expires_at: UtcTimestamp,
    /// 备份清单位置。
    pub manifest_ref: String,
}

/// 租约读写的失败。
#[derive(Debug)]
pub enum LeaseError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Vault 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 被保护的对象尚未登记（它保护不了任何字节）。
    UnknownObject {
        /// 请求的内容哈希。
        hash: ObjectHash,
    },
    /// 快照映射里的 Vault 尚未注册。
    UnknownVault {
        /// 请求的 Vault。
        vault: VaultId,
    },
    /// 租约身份已被占用。
    DuplicateId {
        /// 冲突的身份文本（两族租约共用同一个错误变体）。
        id: String,
    },
    /// 失效时刻不晚于调用方给出的当前时刻：这样的租约一写就失效。
    AlreadyExpired {
        /// 给出的失效时刻。
        expires_at: UtcTimestamp,
        /// 调用方给出的当前时刻。
        now: UtcTimestamp,
    },
    /// 快照映射为空。
    EmptySnapshotSeqMap,
    /// 快照映射里同一个 Vault 出现了两次。
    DuplicateSnapshotVault {
        /// 重复的 Vault。
        vault: VaultId,
    },
    /// 备份清单位置为空。
    EmptyManifestRef,
    /// 对象登记失败（读已登记对象时）。
    Object(VersionError),
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::OutOfScope => f.write_str("Vault 不存在或不在调用方的 owner scope 内"),
            Self::UnknownObject { hash } => {
                write!(f, "内容对象 {} 尚未登记", hex(hash.as_slice()))
            }
            Self::UnknownVault { vault } => write!(f, "Vault {vault} 不存在"),
            Self::DuplicateId { id } => write!(f, "租约身份 {id} 已被占用"),
            Self::AlreadyExpired { expires_at, now } => {
                write!(f, "租约失效时刻 {expires_at} 不晚于当前时刻 {now}")
            }
            Self::EmptySnapshotSeqMap => f.write_str("快照映射至少要有一个 Vault"),
            Self::DuplicateSnapshotVault { vault } => {
                write!(f, "快照映射里 Vault {vault} 出现了两次")
            }
            Self::EmptyManifestRef => f.write_str("备份清单位置不能为空"),
            Self::Object(source) => write!(f, "读内容对象失败：{source}"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for LeaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::Object(source) => Some(source),
            _ => None,
        }
    }
}

/// 写一条对象租约（download/commit 保护）。
pub async fn lease_object(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewObjectLease,
    now: UtcTimestamp,
) -> Result<ObjectLease, LeaseError> {
    require_vault_owner(tx, owner, new.vault).await?;
    if new.expires_at <= now {
        return Err(LeaseError::AlreadyExpired {
            expires_at: new.expires_at,
            now,
        });
    }
    // 先按 owner scope 读一次对象：外键只是兜底，这里要给出“对象还没登记”的可区分错误。
    versions::object_in_scope(tx, owner, new.vault, &new.object_hash)
        .await
        .map_err(LeaseError::Object)?
        .ok_or(LeaseError::UnknownObject {
            hash: new.object_hash,
        })?;

    let inserted = sqlx::query(
        "INSERT INTO object_leases (id, vault_id, object_hash, purpose, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(new.vault.as_bytes().as_slice())
    .bind(new.object_hash.as_slice())
    .bind(new.purpose.as_text())
    .bind(new.expires_at.to_db_integer())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_object_lease_error(error, new));
    }
    object_lease_in_scope(tx, owner, new.id)
        .await?
        .ok_or(LeaseError::OutOfScope)
}

/// 按身份读一条对象租约（含已过期的）；不存在或不在 owner scope 内返回 `None`。
pub async fn object_lease_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: ObjectLeaseId,
) -> Result<Option<ObjectLease>, LeaseError> {
    let row = sqlx::query_as::<_, ObjectLeaseRow>(&format!(
        "SELECT {OBJECT_LEASE_COLUMNS} FROM object_leases l JOIN vaults r ON r.id = l.vault_id \
         WHERE l.id = ?1 AND r.owner_id = ?2"
    ))
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(LeaseError::Db)?;
    row.map(ObjectLeaseRow::into_record).transpose()
}

/// 列出某个 Vault 在 `now` 时刻**有效**的对象租约（`expires_at > now`），按失效时刻与身份升序。
///
/// GC 标记阶段把这里的结果当作额外引用（design §19.4）；“有效”只看时间，不看用途。
pub async fn active_object_leases(
    conn: &mut SqliteConnection,
    owner: UserId,
    vault: VaultId,
    now: UtcTimestamp,
) -> Result<Vec<ObjectLease>, LeaseError> {
    require_vault_owner(conn, owner, vault).await?;
    let rows = sqlx::query_as::<_, ObjectLeaseRow>(&format!(
        "SELECT {OBJECT_LEASE_COLUMNS} FROM object_leases l WHERE l.vault_id = ?1 \
         AND l.expires_at > ?2 ORDER BY l.expires_at, l.id"
    ))
    .bind(vault.as_bytes().as_slice())
    .bind(now.to_db_integer())
    .fetch_all(conn)
    .await
    .map_err(LeaseError::Db)?;
    rows.into_iter().map(ObjectLeaseRow::into_record).collect()
}

/// 释放一条对象租约；返回是否删掉了一行。租约已不存在（含不属于调用方）返回 `false`，可安全重试。
pub async fn release_object_lease(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    id: ObjectLeaseId,
) -> Result<bool, LeaseError> {
    if object_lease_in_scope(tx, owner, id).await?.is_none() {
        return Ok(false);
    }
    let deleted = sqlx::query("DELETE FROM object_leases WHERE id = ?1")
        .bind(id.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(LeaseError::Db)?;
    Ok(deleted.rows_affected() == 1)
}

/// 删除某 Vault 下已失效（`expires_at <= now`）的对象租约，返回行数。
///
/// 失效租约已经保护不了任何对象，清理它不改变保护范围；仍在有效期内的租约一行都不动。
pub async fn prune_expired_object_leases(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    vault: VaultId,
    now: UtcTimestamp,
) -> Result<u64, LeaseError> {
    require_vault_owner(tx, owner, vault).await?;
    let deleted = sqlx::query("DELETE FROM object_leases WHERE vault_id = ?1 AND expires_at <= ?2")
        .bind(vault.as_bytes().as_slice())
        .bind(now.to_db_integer())
        .execute(&mut **tx)
        .await
        .map_err(LeaseError::Db)?;
    Ok(deleted.rows_affected())
}

/// 写一条备份租约（design §19.1 步骤 4：清单对象在导出结束或租约失效前不许被 GC）。
pub async fn insert_backup_lease(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewBackupLease,
    now: UtcTimestamp,
) -> Result<BackupLease, LeaseError> {
    if new.manifest_ref.is_empty() {
        return Err(LeaseError::EmptyManifestRef);
    }
    if new.expires_at <= now {
        return Err(LeaseError::AlreadyExpired {
            expires_at: new.expires_at,
            now,
        });
    }
    for (vault, _) in new.snapshot_seq_map.entries() {
        require_registered_vault(tx, *vault).await?;
    }
    let map_json = new.snapshot_seq_map.to_json();
    let inserted = sqlx::query(
        "INSERT INTO backup_leases (id, snapshot_seq_map, expires_at, manifest_ref) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(&map_json)
    .bind(new.expires_at.to_db_integer())
    .bind(&new.manifest_ref)
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_backup_lease_error(error, new.id));
    }
    backup_lease(tx, new.id)
        .await?
        .ok_or(LeaseError::DuplicateId {
            id: new.id.to_hex(),
        })
}

/// 按身份读一条备份租约（含已过期的）；不存在返回 `None`。
pub async fn backup_lease(
    conn: &mut SqliteConnection,
    id: BackupLeaseId,
) -> Result<Option<BackupLease>, LeaseError> {
    let row = sqlx::query_as::<_, BackupLeaseRow>(&format!(
        "SELECT {BACKUP_LEASE_COLUMNS} FROM backup_leases WHERE id = ?1"
    ))
    .bind(id.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await
    .map_err(LeaseError::Db)?;
    match row {
        Some(row) => Ok(Some(row.into_record(conn).await?)),
        None => Ok(None),
    }
}

/// 列出 `now` 时刻**有效**的备份租约（`expires_at > now`），按失效时刻与身份升序。
pub async fn active_backup_leases(
    conn: &mut SqliteConnection,
    now: UtcTimestamp,
) -> Result<Vec<BackupLease>, LeaseError> {
    let rows = sqlx::query_as::<_, BackupLeaseRow>(&format!(
        "SELECT {BACKUP_LEASE_COLUMNS} FROM backup_leases \
         WHERE expires_at > ?1 ORDER BY expires_at, id"
    ))
    .bind(now.to_db_integer())
    .fetch_all(&mut *conn)
    .await
    .map_err(LeaseError::Db)?;
    let mut leases = Vec::with_capacity(rows.len());
    for row in rows {
        leases.push(row.into_record(conn).await?);
    }
    Ok(leases)
}

/// 释放一条备份租约；返回是否删掉了一行。租约已不存在时返回 `false`，可安全重试。
pub async fn release_backup_lease(
    tx: &mut Transaction<'_, Sqlite>,
    id: BackupLeaseId,
) -> Result<bool, LeaseError> {
    let deleted = sqlx::query("DELETE FROM backup_leases WHERE id = ?1")
        .bind(id.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(LeaseError::Db)?;
    Ok(deleted.rows_affected() == 1)
}

/// 删除已失效（`expires_at <= now`）的备份租约，返回行数。
pub async fn prune_expired_backup_leases(
    tx: &mut Transaction<'_, Sqlite>,
    now: UtcTimestamp,
) -> Result<u64, LeaseError> {
    let deleted = sqlx::query("DELETE FROM backup_leases WHERE expires_at <= ?1")
        .bind(now.to_db_integer())
        .execute(&mut **tx)
        .await
        .map_err(LeaseError::Db)?;
    Ok(deleted.rows_affected())
}

/// 确认 Vault 属于调用方的 owner scope。
async fn require_vault_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    vault: VaultId,
) -> Result<(), LeaseError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT owner_id FROM vaults WHERE id = ?1")
        .bind(vault.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(LeaseError::Db)?;
    match found {
        Some(bytes) if bytes.as_slice() == owner.as_bytes() => Ok(()),
        _ => Err(LeaseError::OutOfScope),
    }
}

/// 确认 Vault 已注册（备份租约跨 Vault，因此不做 owner scope，只核对存在性）。
async fn require_registered_vault(
    conn: &mut SqliteConnection,
    vault: VaultId,
) -> Result<(), LeaseError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT id FROM vaults WHERE id = ?1")
        .bind(vault.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(LeaseError::Db)?;
    match found {
        Some(_) => Ok(()),
        None => Err(LeaseError::UnknownVault { vault }),
    }
}

/// 数据库行到 [`ObjectLease`] 的中间形态。
#[derive(sqlx::FromRow)]
struct ObjectLeaseRow {
    id: Vec<u8>,
    vault_id: Vec<u8>,
    object_hash: Vec<u8>,
    purpose: String,
    expires_at: i64,
}

impl ObjectLeaseRow {
    fn into_record(self) -> Result<ObjectLease, LeaseError> {
        Ok(ObjectLease {
            id: identity(self.id, "id", ObjectLeaseId::from_bytes)?,
            vault: identity(self.vault_id, "vault_id", VaultId::from_bytes)?,
            object_hash: content_hash(self.object_hash)?,
            purpose: ObjectLeasePurpose::from_text(&self.purpose)
                .ok_or(LeaseError::MalformedRow { column: "purpose" })?,
            expires_at: timestamp(self.expires_at, "expires_at")?,
        })
    }
}

/// `backup_leases` 行的中间形态；快照映射要回到连接上解析，因此分两步转换。
#[derive(sqlx::FromRow)]
struct BackupLeaseRow {
    id: Vec<u8>,
    snapshot_seq_map: String,
    expires_at: i64,
    manifest_ref: String,
}

impl BackupLeaseRow {
    async fn into_record(self, conn: &mut SqliteConnection) -> Result<BackupLease, LeaseError> {
        let snapshot_seq_map =
            SnapshotSeqMap::from_json(&mut *conn, &self.snapshot_seq_map).await?;
        Ok(BackupLease {
            id: identity(self.id, "id", BackupLeaseId::from_bytes)?,
            snapshot_seq_map,
            expires_at: timestamp(self.expires_at, "expires_at")?,
            manifest_ref: self.manifest_ref,
        })
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, LeaseError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| LeaseError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 内容哈希列长度检查：必须是恰好 32 字节。
fn content_hash(bytes: Vec<u8>) -> Result<ObjectHash, LeaseError> {
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| LeaseError::MalformedRow {
            column: "object_hash",
        })
}

/// 时间列还原；负值按坏行处理。
fn timestamp(value: i64, column: &'static str) -> Result<UtcTimestamp, LeaseError> {
    UtcTimestamp::from_db_integer(value).map_err(|_| LeaseError::MalformedRow { column })
}

/// 哈希的十六进制文本，只用于错误信息。
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from_digit(u32::from(byte >> 4), 16).expect("半字节是十六进制数字"));
        text.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("半字节是十六进制数字"));
    }
    text
}

/// 把对象租约写语句的约束错误翻译成带上下文的类型化错误。
fn classify_object_lease_error(error: sqlx::Error, new: &NewObjectLease) -> LeaseError {
    let Some(database_error) = error.as_database_error() else {
        return LeaseError::Db(error);
    };
    let message = database_error.message();
    if message.starts_with("UNIQUE constraint failed") {
        return LeaseError::DuplicateId {
            id: new.id.to_hex(),
        };
    }
    if message.contains("FOREIGN KEY") {
        // 外键兜底：对象在预检查之后被删（写事务串行，正常路径不会到这里）。
        return LeaseError::UnknownObject {
            hash: new.object_hash,
        };
    }
    LeaseError::Db(error)
}

/// 把备份租约写语句的约束错误翻译成带上下文的类型化错误。
fn classify_backup_lease_error(error: sqlx::Error, id: BackupLeaseId) -> LeaseError {
    let Some(database_error) = error.as_database_error() else {
        return LeaseError::Db(error);
    };
    if database_error
        .message()
        .starts_with("UNIQUE constraint failed")
    {
        return LeaseError::DuplicateId { id: id.to_hex() };
    }
    LeaseError::Db(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::Database;
    use crate::test_support::{
        TestDir, at, backup_lease as backup_lease_id, content_hash,
        object_lease as object_lease_id, open_database, register_vault, seed_object, seed_owner,
        user,
    };

    async fn lease(database: &Database, owner: UserId, new: &NewObjectLease) -> ObjectLease {
        let mut tx = database.begin_write().await.expect("开始写事务");
        let record = lease_object(&mut tx, owner, new, at(1_000))
            .await
            .expect("写对象租约");
        tx.commit().await.expect("提交");
        record
    }

    fn download(vault: VaultId, marker: u8, hash: ObjectHash) -> NewObjectLease {
        NewObjectLease {
            id: object_lease_id(marker),
            vault,
            object_hash: hash,
            purpose: ObjectLeasePurpose::Download,
            expires_at: at(1_060),
        }
    }

    #[tokio::test]
    async fn a_restarted_server_lists_only_the_leases_that_still_hold() {
        let dir = TestDir::new("leases-restart");
        let path = dir.database();
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let hash = seed_object(&database, owner, vault, 0x51, 10).await;
        let committing = seed_object(&database, owner, vault, 0x52, 20).await;

        lease(&database, owner, &download(vault, 0x61, hash)).await;
        lease(
            &database,
            owner,
            &NewObjectLease {
                purpose: ObjectLeasePurpose::Commit,
                ..download(vault, 0x62, committing)
            },
        )
        .await;
        database.close().await;

        // 重启后按当前时刻列出有效租约：两个都还在有效期内。
        let database = Database::open(&path).await.expect("重启后打开库");
        let mut conn = database.pool().acquire().await.expect("取连接");
        let active = active_object_leases(&mut conn, owner, vault, at(1_030))
            .await
            .expect("列出有效租约");
        assert_eq!(
            active
                .iter()
                .map(|lease| (lease.id, lease.object_hash, lease.purpose))
                .collect::<Vec<_>>(),
            vec![
                (object_lease_id(0x61), hash, ObjectLeasePurpose::Download),
                (
                    object_lease_id(0x62),
                    committing,
                    ObjectLeasePurpose::Commit
                )
            ]
        );
        // 过了失效时刻（`expires_at <= now`）就不再是有效租约，行本身仍在，直到被清理。
        assert!(
            active_object_leases(&mut conn, owner, vault, at(1_060))
                .await
                .expect("列出有效租约")
                .is_empty()
        );
        assert_eq!(
            object_lease_in_scope(&mut conn, owner, object_lease_id(0x61))
                .await
                .expect("读租约")
                .expect("行仍在")
                .expires_at,
            at(1_060)
        );
        drop(conn);

        let mut tx = database.begin_write().await.expect("开始写事务");
        let pruned = prune_expired_object_leases(&mut tx, owner, vault, at(1_060))
            .await
            .expect("清理失效租约");
        tx.commit().await.expect("提交");
        assert_eq!(pruned, 2);
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert!(
            active_object_leases(&mut conn, owner, vault, at(1_010))
                .await
                .expect("列出有效租约")
                .is_empty()
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn object_leases_need_a_registered_object_and_a_live_deadline() {
        let dir = TestDir::new("leases-object-rules");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let hash = seed_object(&database, owner, vault, 0x51, 10).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        // 未登记的对象保护不了任何字节。
        let error = lease_object(
            &mut tx,
            owner,
            &download(vault, 0x61, content_hash(0x99)),
            at(1_000),
        )
        .await
        .expect_err("未登记对象必须拒绝");
        assert!(
            matches!(error, LeaseError::UnknownObject { hash: actual } if actual == content_hash(0x99)),
            "实际错误 {error:?}"
        );
        // 一写就失效的租约没有意义。
        let error = lease_object(
            &mut tx,
            owner,
            &NewObjectLease {
                expires_at: at(1_000),
                ..download(vault, 0x61, hash)
            },
            at(1_000),
        )
        .await
        .expect_err("失效时刻必须晚于当前时刻");
        assert!(
            matches!(error, LeaseError::AlreadyExpired { .. }),
            "实际错误 {error:?}"
        );
        let error = lease_object(&mut tx, user(0x12), &download(vault, 0x61, hash), at(1_000))
            .await
            .expect_err("别人的 Vault 不能写租约");
        assert!(matches!(error, LeaseError::OutOfScope));
        tx.rollback().await.expect("回滚");

        lease(&database, owner, &download(vault, 0x61, hash)).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = lease_object(&mut tx, owner, &download(vault, 0x61, hash), at(1_000))
            .await
            .expect_err("重复身份必须拒绝");
        assert!(
            matches!(error, LeaseError::DuplicateId { ref id } if id == &object_lease_id(0x61).to_hex()),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 被租约保护的对象行不能删：GC 与租约用的是同一条引用协调（design §19.4）。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let refused = sqlx::query("DELETE FROM objects WHERE vault_id = ?1 AND hash = ?2")
            .bind(vault.as_bytes().as_slice())
            .bind(hash.as_slice())
            .execute(&mut *tx)
            .await
            .expect_err("仍被租约保护的对象行不能删");
        assert!(
            refused.to_string().contains("FOREIGN KEY"),
            "实际错误 {refused}"
        );
        tx.rollback().await.expect("回滚");

        // 释放是幂等的：第二次返回 false 而不是报错。
        let mut tx = database.begin_write().await.expect("开始写事务");
        assert!(
            release_object_lease(&mut tx, owner, object_lease_id(0x61))
                .await
                .expect("释放租约")
        );
        assert!(
            !release_object_lease(&mut tx, owner, object_lease_id(0x61))
                .await
                .expect("重复释放不报错")
        );
        // 越权释放既不删别人的租约，也不泄露它是否存在。
        assert!(
            !release_object_lease(&mut tx, user(0x12), object_lease_id(0x62))
                .await
                .expect("越权释放")
        );
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[tokio::test]
    async fn a_backup_lease_round_trips_its_snapshot_map() {
        let dir = TestDir::new("leases-backup");
        let path = dir.database();
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let (other_vault, _other_entry) = register_vault(&database, owner, 0x22).await;

        // 规范文本：按 Vault 身份升序、值是十进制文本（design §7.1：计数在文本里传）。
        let map = SnapshotSeqMap::new(vec![
            (other_vault, VaultSeq::new(4).expect("seq")),
            (vault, VaultSeq::new(17).expect("seq")),
        ])
        .expect("快照映射");
        assert_eq!(
            map.to_json(),
            format!(
                r#"{{"{}":"17","{}":"4"}}"#,
                vault.to_hex(),
                other_vault.to_hex()
            )
        );
        assert!(
            SnapshotSeqMap::new(vec![]).is_err(),
            "空映射保护不了任何对象"
        );
        assert!(matches!(
            SnapshotSeqMap::new(vec![
                (vault, VaultSeq::new(1).expect("seq")),
                (vault, VaultSeq::new(2).expect("seq")),
            ]),
            Err(LeaseError::DuplicateSnapshotVault { .. })
        ));

        let mut tx = database.begin_write().await.expect("开始写事务");
        let write = |id: BackupLeaseId, map: SnapshotSeqMap, manifest: &str| NewBackupLease {
            id,
            snapshot_seq_map: map,
            expires_at: at(2_000),
            manifest_ref: manifest.to_owned(),
        };
        let lease = insert_backup_lease(
            &mut tx,
            &write(backup_lease_id(0x71), map.clone(), "/srv/backups/m1"),
            at(1_000),
        )
        .await
        .expect("写备份租约");
        assert_eq!(lease.snapshot_seq_map, map);
        assert_eq!(lease.manifest_ref, "/srv/backups/m1");
        // 清单位置是租约的证据，不能为空。
        let error = insert_backup_lease(
            &mut tx,
            &write(backup_lease_id(0x72), map.clone(), ""),
            at(1_000),
        )
        .await
        .expect_err("空清单位置必须拒绝");
        assert!(matches!(error, LeaseError::EmptyManifestRef));
        // 已失效的租约不能建立。
        let error = insert_backup_lease(
            &mut tx,
            &NewBackupLease {
                expires_at: at(999),
                ..write(backup_lease_id(0x72), map.clone(), "/srv/backups/m2")
            },
            at(1_000),
        )
        .await
        .expect_err("失效时刻必须晚于当前时刻");
        assert!(matches!(error, LeaseError::AlreadyExpired { .. }));
        // 映射里的 Vault 必须真实存在。
        let unknown_vault = VaultId::from_bytes([0x99; ID_LEN]);
        let error = insert_backup_lease(
            &mut tx,
            &write(
                backup_lease_id(0x72),
                SnapshotSeqMap::new(vec![(unknown_vault, VaultSeq::new(1).expect("seq"))])
                    .expect("快照映射"),
                "/srv/backups/m2",
            ),
            at(1_000),
        )
        .await
        .expect_err("未注册 Vault 必须拒绝");
        assert!(
            matches!(error, LeaseError::UnknownVault { vault: actual } if actual == unknown_vault)
        );
        let error = insert_backup_lease(
            &mut tx,
            &write(backup_lease_id(0x71), map.clone(), "/srv/backups/m3"),
            at(1_000),
        )
        .await
        .expect_err("重复身份必须拒绝");
        assert!(matches!(error, LeaseError::DuplicateId { .. }));
        tx.commit().await.expect("提交");
        database.close().await;

        // 重启后按当前时刻列出有效备份租约，映射与清单可原样读回。
        let database = Database::open(&path).await.expect("重启后打开库");
        let mut conn = database.pool().acquire().await.expect("取连接");
        let active = active_backup_leases(&mut conn, at(1_500))
            .await
            .expect("列出有效备份租约");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, backup_lease_id(0x71));
        assert_eq!(active[0].snapshot_seq_map, map);
        assert!(
            active_backup_leases(&mut conn, at(2_000))
                .await
                .expect("列出有效备份租约")
                .is_empty()
        );
        // 库里被写成 JSON number 的映射不是规范文本：按坏行报错，不静默接受两种写法。
        let mut tx = database.begin_write().await.expect("开始写事务");
        sqlx::query("UPDATE backup_leases SET snapshot_seq_map = ?1 WHERE id = ?2")
            .bind(format!(r#"{{"{}":17}}"#, vault.to_hex()))
            .bind(backup_lease_id(0x71).as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .expect("改写映射");
        tx.commit().await.expect("提交");
        assert!(matches!(
            backup_lease(&mut conn, backup_lease_id(0x71)).await,
            Err(LeaseError::MalformedRow { .. })
        ));
        drop(conn);

        let mut tx = database.begin_write().await.expect("开始写事务");
        assert!(
            release_backup_lease(&mut tx, backup_lease_id(0x71))
                .await
                .expect("释放备份租约")
        );
        assert!(
            !release_backup_lease(&mut tx, backup_lease_id(0x71))
                .await
                .expect("重复释放")
        );
        assert_eq!(
            prune_expired_backup_leases(&mut tx, at(2_500))
                .await
                .expect("清理失效备份租约"),
            0
        );
        tx.commit().await.expect("提交");
        database.close().await;
    }
}
