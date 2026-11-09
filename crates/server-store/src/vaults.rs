//! 服务端 Vault 表（`vaults`）的事务内读写与 owner scope（`docs/design.md` §7.2、§17.1）。
//!
//! 边界：本模块读写 `vaults` 与其合成根条目，并聚合已提交文件容量；不判挂载、不读路径元数据、不做对象或事件 I/O。
//! 写函数接收调用方已开的事务；`insert_vault` 与 `entries` 的根条目写入在同一个事务里完成，
//! 因此不存在“有 Vault 却没有根条目”的中间状态。
//!
//! 不变量：
//!
//! - **owner scope**：每个 Vault 都属于一个账号。所有读写带 [`UserId`]，不属于该 owner 的 Vault
//!   一律 [`VaultError::OutOfScope`] / `None`——即使首发只有一位 owner 也不省略授权（design §17.1）。
//! - **一个 Vault 一个合成根条目**：注册时同时建立 `parent_id IS NULL` 的根条目（design §7.1：
//!   根目录也有 `EntryId`），唯一性由迁移 0002 的部分唯一索引保证。
//! - **路径唯一**：`absolute_path` 全库唯一。本层只检查形式（非空、以 `/` 起始、不以 `/` 结尾、
//!   无 NUL）；“路径不重叠”（父子目录、别名、bind 挂载）需要挂载与路径元数据，由 S032 的重叠
//!   检查与 S044 的注册流程负责，本层不重新实现一份。
//! - **身份不解释**：`volume_identity`/`vault_file_identity` 是平台层编码的不透明字节（S032 的
//!   快照），本层只保证非空；身份不匹配时停止同步属于平台层判定（design §7.2）。
//!
//! Vault 路径文本的绝对性按“以 `/` 起始”判断，而不用宿主 `Path::is_absolute`：服务端是 Linux，
//! 但本 crate 在 Windows 便携检查里也会编译与跑测试，宿主语义会让同一输入得出不同结论。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{EntryId, ID_LEN, UserId, VaultId};
use vaultsyncd_domain::version::{ServerEpoch, VaultSeq};

use crate::entries::{self, EntryError};

/// Vault 的健康状态（design §7.2 `vaults.health`）。
///
/// 只有两个取值，都能对回设计里的判定：挂载、身份与权限都核对通过是 [`Self::Healthy`]；挂载消失、
/// 身份不匹配或权限失败按“Vault 不可用”处理（design §11.5），绝不据此推导删除。更细的健康原因
/// 属 S044 的健康 API，本卡不提前定义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultHealth {
    /// 挂载、身份与权限检查通过，可以同步。
    Healthy,
    /// Vault 不可用（挂载消失、身份不匹配、权限失败）；不推导删除。
    Unavailable,
}

impl VaultHealth {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Unavailable => "unavailable",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜默认值。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "healthy" => Some(Self::Healthy),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }
}

/// 注册一个 Vault 的输入。
///
/// 不变量（违反时 [`insert_vault`] 报错且不写入任何行）：owner 必须已存在；`absolute_path` 是
/// 服务端绝对路径文本且全库唯一；两个身份字节非空；`id` 与 `entry` 未被占用。
#[derive(Clone, Debug)]
pub struct NewVault {
    /// Vault 身份（由应用层经 `IdSource` 生成）。
    pub id: VaultId,
    /// 该 Vault 的合成根条目身份（design §7.1）。
    pub entry: EntryId,
    /// 所属账号。
    pub owner: UserId,
    /// 注册时的绝对路径文本。
    pub absolute_path: String,
    /// 卷身份字节（平台层编码，本层不解释）。
    pub volume_identity: Vec<u8>,
    /// 根目录身份字节（平台层编码，本层不解释）。
    pub vault_file_identity: Vec<u8>,
    /// 注册时的服务器 epoch；`ServerEpoch` 变化会让旧客户端游标失效。
    pub epoch: ServerEpoch,
}

/// 从数据库读出的一条 `vaults` 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultRecord {
    /// Vault 身份。
    pub id: VaultId,
    /// 所属账号。
    pub owner: UserId,
    /// 注册时的绝对路径文本。
    pub absolute_path: String,
    /// 卷身份字节。
    pub volume_identity: Vec<u8>,
    /// 根目录身份字节。
    pub vault_file_identity: Vec<u8>,
    /// 健康状态。
    pub health: VaultHealth,
    /// 递增到的事务序号；新注册为 [`VaultSeq::ZERO`]。
    pub seq: VaultSeq,
    /// 服务器 epoch。
    pub epoch: ServerEpoch,
}

/// Vault 读写的失败。
#[derive(Debug)]
pub enum VaultError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// 路径不是规范的服务器绝对路径；`reason` 说明是哪一条规则。
    InvalidPath {
        /// 给出的路径。
        path: String,
        /// 不合格的具体原因。
        reason: &'static str,
    },
    /// 身份字节为空。
    EmptyIdentity {
        /// 出问题的字段名。
        field: &'static str,
    },
    /// owner 账号不存在（必须先由初始化流程建出账号）。
    UnknownOwner {
        /// 给出的账号身份。
        id: UserId,
    },
    /// 同一路径已有 Vault。
    DuplicatePath {
        /// 冲突的路径。
        path: String,
    },
    /// Vault 身份已被占用。
    DuplicateId {
        /// 冲突的 Vault 身份。
        id: VaultId,
    },
    /// Vault 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 合成根条目写入失败。
    Entry(EntryError),
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::InvalidPath { path, reason } => write!(f, "Vault 路径 {path:?} {reason}"),
            Self::EmptyIdentity { field } => write!(f, "身份字段 {field} 为空"),
            Self::UnknownOwner { id } => write!(f, "账号 {id} 不存在"),
            Self::DuplicatePath { path } => write!(f, "路径 {path:?} 已有 Vault"),
            Self::DuplicateId { id } => write!(f, "Vault 身份 {id} 已被占用"),
            Self::OutOfScope => f.write_str("Vault 不存在或不在调用方的 owner scope 内"),
            Self::Entry(source) => write!(f, "根条目写入失败：{source}"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for VaultError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::Entry(source) => Some(source),
            _ => None,
        }
    }
}

/// `vaults` 表的列顺序，多处查询共用。
const VAULT_COLUMNS: &str = "id, owner_id, absolute_path, volume_identity, vault_file_identity, \
                            health, seq, epoch";

/// 注册一个 Vault：检查 owner、路径形式与身份字节后写入 `vaults` 一行与合成根条目一行。
///
/// 两行都在调用方的事务里，任一失败都不得提交；返回写入后的记录（重新读出）。
pub async fn insert_vault(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewVault,
) -> Result<VaultRecord, VaultError> {
    validate_path(&new.absolute_path)?;
    if new.volume_identity.is_empty() {
        return Err(VaultError::EmptyIdentity {
            field: "volume_identity",
        });
    }
    if new.vault_file_identity.is_empty() {
        return Err(VaultError::EmptyIdentity {
            field: "vault_file_identity",
        });
    }
    require_owner(tx, new.owner).await?;

    let inserted = sqlx::query(
        "INSERT INTO vaults \
         (id, owner_id, absolute_path, volume_identity, vault_file_identity, health, seq, epoch) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(new.owner.as_bytes().as_slice())
    .bind(&new.absolute_path)
    .bind(&new.volume_identity)
    .bind(&new.vault_file_identity)
    .bind(VaultHealth::Healthy.as_text())
    .bind(VaultSeq::ZERO.to_db_integer())
    .bind(new.epoch.to_db_integer())
    .execute(&mut **tx)
    .await;

    if let Err(error) = inserted {
        return Err(classify_insert_error(error, new));
    }
    entries::insert_vault_entry(tx, new.id, new.entry)
        .await
        .map_err(VaultError::Entry)?;

    vault_in_scope(tx, new.owner, new.id)
        .await?
        .ok_or(VaultError::OutOfScope)
}

/// 按身份读一个 Vault，owner scope 不匹配时返回 `None`。
pub async fn vault_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: VaultId,
) -> Result<Option<VaultRecord>, VaultError> {
    let row = sqlx::query_as::<_, VaultRow>(&format!(
        "SELECT {VAULT_COLUMNS} FROM vaults WHERE id = ?1 AND owner_id = ?2"
    ))
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(VaultError::Db)?;
    row.map(VaultRow::into_record).transpose()
}

/// 列出某个账号名下的全部 Vault，按路径文本排序。
pub async fn vaults_of_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
) -> Result<Vec<VaultRecord>, VaultError> {
    let rows = sqlx::query_as::<_, VaultRow>(&format!(
        "SELECT {VAULT_COLUMNS} FROM vaults WHERE owner_id = ?1 ORDER BY absolute_path"
    ))
    .bind(owner.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(VaultError::Db)?;
    rows.into_iter().map(VaultRow::into_record).collect()
}

/// 本机管理的有界 Vault 页：按身份排序，最多 101 条（100 条与一条 next-page 探针）。
///
/// 普通管理清单的文件容量见 [`vaults_with_file_bytes_of_owner`]。
pub async fn vaults_of_owner_page(
    conn: &mut SqliteConnection,
    owner: UserId,
    page: u32,
) -> Result<Vec<VaultRecord>, VaultError> {
    let rows = sqlx::query_as::<_, VaultRow>(&format!(
        "SELECT {VAULT_COLUMNS} FROM vaults WHERE owner_id = ?1 ORDER BY id LIMIT 101 OFFSET ?2"
    ))
    .bind(owner.as_bytes().as_slice())
    .bind(i64::from(page) * 100)
    .fetch_all(conn)
    .await
    .map_err(VaultError::Db)?;
    rows.into_iter().map(VaultRow::into_record).collect()
}

/// 在同一 SQLite 读快照中返回账号的 Vault、序号与当前文件逻辑容量。
/// 每个活跃文件只计其已提交内容版本一次；同摘要文件分别计数，不含历史与回收站。
/// 空 Vault 为零，SQL 求和溢出及其他读取错误明确失败，不返回伪造的零。
pub async fn vaults_with_file_bytes_of_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
) -> Result<Vec<(VaultRecord, u64)>, VaultError> {
    #[derive(sqlx::FromRow)]
    struct UsageRow {
        #[sqlx(flatten)]
        vault: VaultRow,
        file_bytes: i64,
    }
    let rows = sqlx::query_as::<_, UsageRow>(&format!(
        "SELECT {VAULT_COLUMNS}, \
         (SELECT COALESCE(SUM(v.size), 0) FROM entries e \
          JOIN versions v ON v.id = e.content_version_id \
            AND v.vault_id = e.vault_id AND v.entry_id = e.id \
          WHERE e.vault_id = vaults.id AND e.kind = 'file' AND e.deleted_at IS NULL) AS file_bytes \
         FROM vaults WHERE owner_id = ?1 ORDER BY absolute_path"
    ))
    .bind(owner.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(VaultError::Db)?;
    rows.into_iter()
        .map(|row| {
            let bytes = u64::try_from(row.file_bytes).map_err(|_| VaultError::MalformedRow {
                column: "file_bytes",
            })?;
            Ok((row.vault.into_record()?, bytes))
        })
        .collect()
}

/// 列出全部已注册 Vault，按路径文本排序（启动恢复用）。
///
/// 启动恢复是系统级操作，不属于某个 owner scope：进程必须恢复每一个已注册 Vault，不能因为
/// “还没有请求上下文”就漏掉某个 Vault 的未完成意图（design §11.3）。
pub async fn all_vaults(conn: &mut SqliteConnection) -> Result<Vec<VaultRecord>, VaultError> {
    let rows = sqlx::query_as::<_, VaultRow>(&format!(
        "SELECT {VAULT_COLUMNS} FROM vaults ORDER BY absolute_path"
    ))
    .fetch_all(conn)
    .await
    .map_err(VaultError::Db)?;
    rows.into_iter().map(VaultRow::into_record).collect()
}

/// 更新 Vault 的健康状态；不属于调用方的 Vault（或不存在）返回 [`VaultError::OutOfScope`]。
pub async fn set_health(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    id: VaultId,
    health: VaultHealth,
) -> Result<(), VaultError> {
    let updated = sqlx::query("UPDATE vaults SET health = ?1 WHERE id = ?2 AND owner_id = ?3")
        .bind(health.as_text())
        .bind(id.as_bytes().as_slice())
        .bind(owner.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(VaultError::Db)?;
    if updated.rows_affected() == 0 {
        return Err(VaultError::OutOfScope);
    }
    Ok(())
}

/// 数据库行到 [`VaultRecord`] 的中间形态：身份列先读成字节，再按类型检查长度。
#[derive(sqlx::FromRow)]
struct VaultRow {
    id: Vec<u8>,
    owner_id: Vec<u8>,
    absolute_path: String,
    volume_identity: Vec<u8>,
    vault_file_identity: Vec<u8>,
    health: String,
    seq: i64,
    epoch: i64,
}

impl VaultRow {
    fn into_record(self) -> Result<VaultRecord, VaultError> {
        Ok(VaultRecord {
            id: identity(self.id, "id", VaultId::from_bytes)?,
            owner: identity(self.owner_id, "owner_id", UserId::from_bytes)?,
            absolute_path: self.absolute_path,
            volume_identity: self.volume_identity,
            vault_file_identity: self.vault_file_identity,
            health: VaultHealth::from_text(&self.health)
                .ok_or(VaultError::MalformedRow { column: "health" })?,
            seq: VaultSeq::from_db_integer(self.seq)
                .map_err(|_| VaultError::MalformedRow { column: "seq" })?,
            epoch: ServerEpoch::from_db_integer(self.epoch)
                .map_err(|_| VaultError::MalformedRow { column: "epoch" })?,
        })
    }
}

/// 确认账号存在；`vaults.owner_id` 的外键也会挡住不存在的账号，这里给出更精确的错误。
async fn require_owner(conn: &mut SqliteConnection, owner: UserId) -> Result<(), VaultError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?1")
        .bind(owner.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(VaultError::Db)?;
    match found {
        Some(_) => Ok(()),
        None => Err(VaultError::UnknownOwner { id: owner }),
    }
}

/// 服务端绝对路径文本的形式检查：非空、无 NUL、以 `/` 起始、不以 `/` 结尾（根目录 `/` 除外）。
///
/// 不做规范化（不折叠 `..`、不合并分隔符）：注册方给什么就存什么，重叠与别名由平台检查负责。
fn validate_path(path: &str) -> Result<(), VaultError> {
    let invalid = |reason| {
        Err(VaultError::InvalidPath {
            path: path.to_owned(),
            reason,
        })
    };
    if path.is_empty() {
        return invalid("为空");
    }
    if path.contains('\0') {
        return invalid("含 NUL 字节");
    }
    if !path.starts_with('/') {
        return invalid("不是以 / 起始的服务器绝对路径");
    }
    if path.len() > 1 && path.ends_with('/') {
        return invalid("以 / 结尾，不是规范形式");
    }
    Ok(())
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, VaultError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| VaultError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 把 `vaults` 插入语句的约束错误翻译成带上下文的类型化错误。
fn classify_insert_error(error: sqlx::Error, new: &NewVault) -> VaultError {
    let Some(database_error) = error.as_database_error() else {
        return VaultError::Db(error);
    };
    let message = database_error.message();
    if !message.starts_with("UNIQUE constraint failed") {
        return VaultError::Db(error);
    }
    if message.contains("vaults.absolute_path") {
        return VaultError::DuplicatePath {
            path: new.absolute_path.clone(),
        };
    }
    VaultError::DuplicateId { id: new.id }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::Database;
    use crate::test_support::{
        TestDir, entry, insert_user, open_database, register_vault, seed_owner, user, vault,
    };

    async fn vaults_snapshot(database: &Database) -> Vec<(String, String, String)> {
        let mut conn = database.pool().acquire().await.expect("取连接");
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT r.absolute_path, r.health, u.login FROM vaults r \
             JOIN users u ON u.id = r.owner_id ORDER BY r.absolute_path",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("读 Vault")
    }

    #[tokio::test]
    async fn insert_vault_writes_the_vault_and_its_synthetic_vault_entry() {
        let dir = TestDir::new("insert-vault");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let vault_id = vault(0x21);
        let vault_entry_id = entry(0x21);

        let mut tx = database.begin_write().await.expect("开始写事务");
        let record = insert_vault(
            &mut tx,
            &NewVault {
                id: vault_id,
                entry: vault_entry_id,
                owner,
                absolute_path: "/srv/vaultsyncd".to_owned(),
                volume_identity: vec![0xAA, 0xBB],
                vault_file_identity: vec![0xCC, 0xDD],
                epoch: ServerEpoch::new(7).expect("epoch"),
            },
        )
        .await
        .expect("注册 Vault");
        tx.commit().await.expect("提交");

        assert_eq!(record.id, vault_id);
        assert_eq!(record.owner, owner);
        assert_eq!(record.absolute_path, "/srv/vaultsyncd");
        assert_eq!(record.volume_identity, vec![0xAA, 0xBB]);
        assert_eq!(record.vault_file_identity, vec![0xCC, 0xDD]);
        assert_eq!(record.health, VaultHealth::Healthy);
        assert_eq!(record.seq, VaultSeq::ZERO);
        assert_eq!(record.epoch, ServerEpoch::new(7).expect("epoch"));

        // 合成根条目在同一次提交里落盘。
        let mut conn = database.pool().acquire().await.expect("取连接");
        let vault_entry = entries::vault_entry(&mut conn, owner, vault_id)
            .await
            .expect("读根条目")
            .expect("根条目必须存在");
        assert_eq!(vault_entry.id, vault_entry_id);
        assert_eq!(vault_entry.parent, None);
        assert_eq!(vault_entry.name, "");
        assert_eq!(vault_entry.kind, entries::EntryKind::Directory);
        assert_eq!(vault_entry.deleted_at, None);
        assert_eq!(
            vault_in_scope(&mut conn, owner, vault_id)
                .await
                .expect("读 Vault")
                .expect("Vault 必须存在"),
            record
        );
        assert_eq!(
            vaults_of_owner(&mut conn, owner).await.expect("列 Vault"),
            vec![record]
        );
        // 别的账号看不到这个 Vault。
        assert_eq!(
            vault_in_scope(&mut conn, user(0x12), vault_id)
                .await
                .expect("读 Vault"),
            None
        );
        assert!(
            vaults_of_owner(&mut conn, user(0x12))
                .await
                .expect("列 Vault")
                .is_empty()
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn local_admin_vault_pages_are_bounded_ordered_and_owner_scoped() {
        let dir = TestDir::new("admin-vault-pages");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        for seed in 1..=102 {
            register_vault(&database, owner, seed).await;
        }
        let mut conn = database.pool().acquire().await.unwrap();
        let first = vaults_of_owner_page(&mut conn, owner, 0).await.unwrap();
        let second = vaults_of_owner_page(&mut conn, owner, 1).await.unwrap();
        assert_eq!(first.len(), 101);
        assert_eq!(second.len(), 2);
        assert_eq!(first[100].id, second[0].id, "额外一行只是下一页探针");
        assert!(first.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert!(
            vaults_of_owner_page(&mut conn, user(0x12), 0)
                .await
                .unwrap()
                .is_empty()
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn duplicate_path_duplicate_id_and_unknown_owner_are_rejected() {
        let dir = TestDir::new("vault-reject");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        register_vault(&database, owner, 0x21).await;
        let before = vaults_snapshot(&database).await;

        // 同一路径、不同身份。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = insert_vault(
            &mut tx,
            &NewVault {
                id: vault(0x22),
                entry: entry(0x22),
                owner,
                absolute_path: "/srv/vaultsyncd-21".to_owned(),
                volume_identity: vec![0x01],
                vault_file_identity: vec![0x02],
                epoch: ServerEpoch::ZERO,
            },
        )
        .await
        .expect_err("重复路径必须失败");
        assert!(
            matches!(error, VaultError::DuplicatePath { ref path } if path == "/srv/vaultsyncd-21"),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 同一身份、不同路径。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = insert_vault(
            &mut tx,
            &NewVault {
                id: vault(0x21),
                entry: entry(0x23),
                owner,
                absolute_path: "/srv/other".to_owned(),
                volume_identity: vec![0x01],
                vault_file_identity: vec![0x02],
                epoch: ServerEpoch::ZERO,
            },
        )
        .await
        .expect_err("重复身份必须失败");
        assert!(
            matches!(error, VaultError::DuplicateId { id } if id == vault(0x21)),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        // 不存在的账号。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = insert_vault(
            &mut tx,
            &NewVault {
                id: vault(0x24),
                entry: entry(0x24),
                owner: user(0x99),
                absolute_path: "/srv/unknown-owner".to_owned(),
                volume_identity: vec![0x01],
                vault_file_identity: vec![0x02],
                epoch: ServerEpoch::ZERO,
            },
        )
        .await
        .expect_err("不存在的账号必须失败");
        assert!(
            matches!(error, VaultError::UnknownOwner { id } if id == user(0x99)),
            "实际错误 {error:?}"
        );
        tx.rollback().await.expect("回滚");

        assert_eq!(vaults_snapshot(&database).await, before, "失败不得改数据");
        // 也没有留下半截的根条目。
        let mut conn = database.pool().acquire().await.expect("取连接");
        let entry_count: i64 = sqlx::query_scalar("SELECT count(*) FROM entries")
            .fetch_one(&mut *conn)
            .await
            .expect("数条目");
        assert_eq!(entry_count, 1, "只应有第一次注册的根条目");
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn invalid_paths_and_empty_identities_are_rejected() {
        let dir = TestDir::new("vault-invalid");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;

        let cases = [
            ("", "为空"),
            ("srv/relative", "不是以 / 起始"),
            ("/srv/trailing/", "以 / 结尾"),
            ("/srv/nul\0byte", "含 NUL"),
        ];
        for (index, (path, reason)) in cases.into_iter().enumerate() {
            let mut tx = database.begin_write().await.expect("开始写事务");
            let marker = 0x30 + index as u8;
            let error = insert_vault(
                &mut tx,
                &NewVault {
                    id: vault(marker),
                    entry: entry(marker),
                    owner,
                    absolute_path: path.to_owned(),
                    volume_identity: vec![0x01],
                    vault_file_identity: vec![0x02],
                    epoch: ServerEpoch::ZERO,
                },
            )
            .await
            .expect_err("非法路径必须失败");
            assert!(
                matches!(error, VaultError::InvalidPath { reason: actual, .. } if actual.contains(reason)),
                "路径 {path:?} 的实际错误 {error:?}"
            );
            tx.rollback().await.expect("回滚");
        }

        for (field, volume, file) in [
            ("volume_identity", Vec::new(), vec![0x02]),
            ("vault_file_identity", vec![0x01], Vec::new()),
        ] {
            let mut tx = database.begin_write().await.expect("开始写事务");
            let error = insert_vault(
                &mut tx,
                &NewVault {
                    id: vault(0x40),
                    entry: entry(0x40),
                    owner,
                    absolute_path: "/srv/empty-identity".to_owned(),
                    volume_identity: volume,
                    vault_file_identity: file,
                    epoch: ServerEpoch::ZERO,
                },
            )
            .await
            .expect_err("空身份字节必须失败");
            assert!(
                matches!(error, VaultError::EmptyIdentity { field: actual } if actual == field),
                "实际错误 {error:?}"
            );
            tx.rollback().await.expect("回滚");
        }
        assert!(
            vaults_snapshot(&database).await.is_empty(),
            "不得写入任何 Vault"
        );
        database.close().await;
    }

    #[tokio::test]
    async fn health_updates_are_scoped_to_the_owner() {
        let dir = TestDir::new("vault-health");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (vault_id, _vault_entry) = register_vault(&database, owner, 0x21).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        insert_user(&mut tx, user(0x12)).await;
        tx.commit().await.expect("提交另一个账号");

        let mut tx = database.begin_write().await.expect("开始写事务");
        set_health(&mut tx, owner, vault_id, VaultHealth::Unavailable)
            .await
            .expect("更新健康状态");
        let error = set_health(&mut tx, user(0x12), vault_id, VaultHealth::Healthy)
            .await
            .expect_err("别人的 owner 不能改健康状态");
        assert!(
            matches!(error, VaultError::OutOfScope),
            "实际错误 {error:?}"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let record = vault_in_scope(&mut conn, owner, vault_id)
            .await
            .expect("读 Vault")
            .expect("Vault 存在");
        assert_eq!(record.health, VaultHealth::Unavailable, "健康状态已更新");
        drop(conn);
        database.close().await;
    }
}
