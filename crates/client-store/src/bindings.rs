//! 客户端绑定表（`bindings`）的事务内读写（`docs/design.md` §7.1、§7.3）。
//!
//! 边界：本模块只读写“本设备某个目录绑定到某个 Vault”这一行。它不注册 Vault（那是服务端与
//! 绑定卡片的事）、不读文件系统、不判挂载，也不生成身份（身份由应用层经 `IdSource` 生成）。
//!
//! 不变量：
//!
//! - **每个绑定是一行**：`id` 是 [`BindingId`]，独立于 [`VaultId`]——同一 Vault 可以在不同设备
//!   或不同本地目录上各有绑定，且每个绑定有自己的模式与规则版本（design §7.1）。
//! - **模式集合来自 design C34**：只有 `full`（完整双向）与 `on_demand`（按需双向）。不接受的
//!   文本在写入前就被拒绝，不做“未知模式按默认处理”。
//! - **路径只做形式检查**：非空、无 NUL。客户端路径可能是 `C:\dir` 或 `/mnt/dir`，绝对性与
//!   重叠判定需要平台语义，属于绑定流程（S058 起），本层不重复实现、也不改写用户给出的路径。
//! - **`server_epoch` 随绑定保存**：epoch 改变意味着旧游标失效（design §7.1、§9.4），判断由
//!   同步流程做，本层只持久化它。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{BindingId, DeviceId, ID_LEN, ServerId, VaultId};
use vaultsyncd_domain::version::ServerEpoch;

/// 绑定的同步模式（design C34：`full` 与 `on_demand`，不含单向模式）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// 完整双向同步：内容按策略驻留本地。
    Full,
    /// 按需双向同步：默认只有占位符，读取时再取内容。
    OnDemand,
}

impl SyncMode {
    /// 数据库里的规范文本（与 design C34 的拼写一致）。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::OnDemand => "on_demand",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜默认模式。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "full" => Some(Self::Full),
            "on_demand" => Some(Self::OnDemand),
            _ => None,
        }
    }
}

/// 新建绑定的输入。
///
/// 不变量（违反时 [`insert_binding`] 报错且不写入）：`local_path`/`cache_path` 非空且无 NUL；
/// `rules_version` 非空；`mode` 是 [`SyncMode`] 的两个取值之一；`id` 未被占用。
#[derive(Clone, Debug)]
pub struct NewBinding {
    /// 绑定身份（由应用层生成）。
    pub id: BindingId,
    /// 绑定的服务器实例身份。
    pub server: ServerId,
    /// 本设备身份；不承担身份认证（design §7.1）。
    pub device: DeviceId,
    /// 绑定的服务器 Vault。
    pub vault: VaultId,
    /// 设备本地路径或挂载点（保持用户给出的形式，不做规范化）。
    pub local_path: String,
    /// 本绑定的缓存位置（与服务端工作目录无关，design §5.3）。
    pub cache_path: String,
    /// 同步模式。
    pub mode: SyncMode,
    /// 排除规则版本标识（规则内容由规则卡定义）。
    pub rules_version: String,
    /// 绑定时看到的服务器 epoch。
    pub server_epoch: ServerEpoch,
}

/// 从数据库读出的一行 `bindings`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRecord {
    /// 绑定身份。
    pub id: BindingId,
    /// 服务器实例身份。
    pub server: ServerId,
    /// 设备身份。
    pub device: DeviceId,
    /// 服务器 Vault。
    pub vault: VaultId,
    /// 设备本地路径。
    pub local_path: String,
    /// 缓存位置。
    pub cache_path: String,
    /// 同步模式。
    pub mode: SyncMode,
    /// 排除规则版本。
    pub rules_version: String,
    /// 服务器 epoch。
    pub server_epoch: ServerEpoch,
}

/// 绑定读写的失败。
#[derive(Debug)]
pub enum BindingError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// 路径或规则版本的形式不合格；`reason` 说明是哪一条规则。
    InvalidField {
        /// 出问题的字段名。
        field: &'static str,
        /// 不合格的具体原因。
        reason: &'static str,
    },
    /// 绑定身份已被占用。
    DuplicateId {
        /// 冲突的绑定身份。
        id: BindingId,
    },
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::InvalidField { field, reason } => write!(f, "字段 {field} {reason}"),
            Self::DuplicateId { id } => write!(f, "绑定身份 {id} 已被占用"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for BindingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::InvalidField { .. } | Self::DuplicateId { .. } | Self::MalformedRow { .. } => {
                None
            }
        }
    }
}

/// `bindings` 表的列顺序，多处查询共用。
const BINDING_COLUMNS: &str = "id, server_id, device_id, vault_id, local_path, cache_path, mode, \
                               rules_version, server_epoch";

/// 写入一个绑定，返回重新读出的记录。
pub async fn insert_binding(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewBinding,
) -> Result<BindingRecord, BindingError> {
    validate_path("local_path", &new.local_path)?;
    validate_path("cache_path", &new.cache_path)?;
    if new.rules_version.is_empty() {
        return Err(BindingError::InvalidField {
            field: "rules_version",
            reason: "为空",
        });
    }

    let inserted = sqlx::query(
        "INSERT INTO bindings \
         (id, server_id, device_id, vault_id, local_path, cache_path, mode, rules_version, \
          server_epoch) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(new.id.as_bytes().as_slice())
    .bind(new.server.as_bytes().as_slice())
    .bind(new.device.as_bytes().as_slice())
    .bind(new.vault.as_bytes().as_slice())
    .bind(&new.local_path)
    .bind(&new.cache_path)
    .bind(new.mode.as_text())
    .bind(&new.rules_version)
    .bind(new.server_epoch.to_db_integer())
    .execute(&mut **tx)
    .await;

    if let Err(error) = inserted {
        if is_unique_violation(&error) {
            return Err(BindingError::DuplicateId { id: new.id });
        }
        return Err(BindingError::Db(error));
    }

    binding(tx, new.id)
        .await?
        .ok_or(BindingError::MalformedRow { column: "id" })
}

/// 按身份读一个绑定；不存在返回 `None`。
pub async fn binding(
    conn: &mut SqliteConnection,
    id: BindingId,
) -> Result<Option<BindingRecord>, BindingError> {
    let row = sqlx::query_as::<_, BindingRow>(&format!(
        "SELECT {BINDING_COLUMNS} FROM bindings WHERE id = ?1"
    ))
    .bind(id.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(BindingError::Db)?;
    row.map(BindingRow::into_record).transpose()
}

/// 数据库行到 [`BindingRecord`] 的中间形态：身份列先读成字节，再按类型检查长度。
#[derive(sqlx::FromRow)]
struct BindingRow {
    id: Vec<u8>,
    server_id: Vec<u8>,
    device_id: Vec<u8>,
    vault_id: Vec<u8>,
    local_path: String,
    cache_path: String,
    mode: String,
    rules_version: String,
    server_epoch: i64,
}

impl BindingRow {
    fn into_record(self) -> Result<BindingRecord, BindingError> {
        Ok(BindingRecord {
            id: identity(self.id, "id", BindingId::from_bytes)?,
            server: identity(self.server_id, "server_id", ServerId::from_bytes)?,
            device: identity(self.device_id, "device_id", DeviceId::from_bytes)?,
            vault: identity(self.vault_id, "vault_id", VaultId::from_bytes)?,
            local_path: self.local_path,
            cache_path: self.cache_path,
            mode: SyncMode::from_text(&self.mode)
                .ok_or(BindingError::MalformedRow { column: "mode" })?,
            rules_version: self.rules_version,
            server_epoch: ServerEpoch::from_db_integer(self.server_epoch).map_err(|_| {
                BindingError::MalformedRow {
                    column: "server_epoch",
                }
            })?,
        })
    }
}

/// 路径字段的形式检查：非空、无 NUL。绝对性与重叠属平台绑定流程，本层不判断。
fn validate_path(field: &'static str, path: &str) -> Result<(), BindingError> {
    if path.is_empty() {
        return Err(BindingError::InvalidField {
            field,
            reason: "为空",
        });
    }
    if path.contains('\0') {
        return Err(BindingError::InvalidField {
            field,
            reason: "含 NUL 字节",
        });
    }
    Ok(())
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, BindingError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| BindingError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 是否为唯一约束冲突（主键重复）。只看错误消息里的约束名，不把其它约束错误当成重复。
fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .map(|database_error| database_error.is_unique_violation())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{TestDir, binding_id, device_id, open_database, server_id, vault_id};

    fn new_binding(marker: u8) -> NewBinding {
        NewBinding {
            id: binding_id(marker),
            server: server_id(0x01),
            device: device_id(0x02),
            vault: vault_id(0x03),
            local_path: r"C:\Users\test\Vault".to_owned(),
            cache_path: r"C:\Users\test\AppData\Local\vaultsyncd\cache".to_owned(),
            mode: SyncMode::Full,
            rules_version: "rules-v1".to_owned(),
            server_epoch: ServerEpoch::new(9).expect("epoch"),
        }
    }

    #[tokio::test]
    async fn insert_binding_round_trips_all_fields() {
        let dir = TestDir::new("insert-binding");
        let database = open_database(&dir).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let record = insert_binding(&mut tx, &new_binding(0x11))
            .await
            .expect("写绑定");
        tx.commit().await.expect("提交");

        assert_eq!(record.id, binding_id(0x11));
        assert_eq!(record.server, server_id(0x01));
        assert_eq!(record.device, device_id(0x02));
        assert_eq!(record.vault, vault_id(0x03));
        assert_eq!(record.local_path, r"C:\Users\test\Vault");
        assert_eq!(
            record.cache_path,
            r"C:\Users\test\AppData\Local\vaultsyncd\cache"
        );
        assert_eq!(record.mode, SyncMode::Full);
        assert_eq!(record.rules_version, "rules-v1");
        assert_eq!(record.server_epoch, ServerEpoch::new(9).expect("epoch"));

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            binding(&mut conn, binding_id(0x11)).await.expect("读绑定"),
            Some(record)
        );
        assert_eq!(
            binding(&mut conn, binding_id(0x12)).await.expect("读绑定"),
            None
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn invalid_fields_duplicate_id_and_bad_mode_are_rejected() {
        let dir = TestDir::new("binding-reject");
        let database = open_database(&dir).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let mut empty_path = new_binding(0x21);
        empty_path.local_path = String::new();
        assert!(matches!(
            insert_binding(&mut tx, &empty_path)
                .await
                .expect_err("空路径必须被拒绝"),
            BindingError::InvalidField {
                field: "local_path",
                ..
            }
        ));

        let mut nul_path = new_binding(0x22);
        nul_path.cache_path = "cache\0dir".to_owned();
        assert!(matches!(
            insert_binding(&mut tx, &nul_path)
                .await
                .expect_err("含 NUL 的路径必须被拒绝"),
            BindingError::InvalidField {
                field: "cache_path",
                ..
            }
        ));

        let mut no_rules = new_binding(0x23);
        no_rules.rules_version = String::new();
        assert!(matches!(
            insert_binding(&mut tx, &no_rules)
                .await
                .expect_err("空规则版本必须被拒绝"),
            BindingError::InvalidField {
                field: "rules_version",
                ..
            }
        ));

        insert_binding(&mut tx, &new_binding(0x24))
            .await
            .expect("写绑定");
        assert!(matches!(
            insert_binding(&mut tx, &new_binding(0x24))
                .await
                .expect_err("重复绑定身份必须被拒绝"),
            BindingError::DuplicateId { id } if id == binding_id(0x24)
        ));

        // 绕过 Rust 直接写不受支持的模式：`CHECK` 让写入失败，坏值不会落库。
        let bad_mode = sqlx::query(
            "INSERT INTO bindings \
             (id, server_id, device_id, vault_id, local_path, cache_path, mode, rules_version, \
              server_epoch) \
             VALUES (x'01', x'02', x'03', x'04', 'p', 'c', 'one_way', 'r', 0)",
        )
        .execute(&mut *tx)
        .await;
        assert!(bad_mode.is_err(), "未知同步模式必须被 CHECK 拒绝");
        tx.commit().await.expect("提交");

        database.close().await;
    }
}
