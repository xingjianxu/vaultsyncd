//! 客户端 SQLite 的打开、种类与 schema 版本门禁、迁移执行与短事务接口
//! （`docs/design.md` §7.3、§7.4、§8.4、§19.3）。
//!
//! 边界：本模块只决定“这个文件是不是客户端库、能不能写、迁移到哪一版、事务怎么开”。
//! 业务表定义在 `crates/client-store/migrations/`，业务读写在本 crate 的
//! [`bindings`](crate::bindings)、[`entries`](crate::entries)、[`queue`](crate::queue)、
//! [`events`](crate::events)、[`transfers`](crate::transfers)、[`intents`](crate::intents)、
//! [`notifications`](crate::notifications)。
//!
//! 不变量：
//!
//! - **客户端库与服务端库是两个文件**（design §5.4、§7.3）：种类写 `PRAGMA application_id`，
//!   服务端库、未标记的非空库与任意其它 SQLite 文件一律在打开阶段被拒绝，且在拒绝前不写对方
//!   的文件。反向门禁（服务端拒绝客户端库）由 `crates/server-store` 负责。
//! - **迁移先于写入**：schema 迁移在返回可写 [`Database`] 之前完成；失败时不返回可写句柄，
//!   因此不会出现“半启动”的同步写入（design §7.4、§19.3）。
//! - **迁移前有一致性备份**：非空库在真正有待执行迁移时先用 `VACUUM INTO` 导出（读事务，包含
//!   WAL 内容），再用 `PRAGMA quick_check` 自检；客户端升级不清空数据库（design §19.3），备份
//!   是回退路径。回退采用整套备份恢复，不做自动 down 迁移。
//! - **旧二进制不碰新 schema**：`PRAGMA user_version` 与 `_sqlx_migrations` 中较大者高于本
//!   二进制就拒绝写。
//! - **WAL 与外键是每条连接的默认项**：打开后读回实际 `journal_mode`，不是 `wal` 就拒绝启动。
//! - **事务只包数据库**：文件系统与 SQLite 不是一个事务（design §6）；[`Database::begin_write`]
//!   给出 `BEGIN IMMEDIATE` 短写事务，调用方不得在事务里做工作目录 I/O、哈希或网络等待。

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{ConnectOptions, Connection, Sqlite, SqliteConnection, SqlitePool, Transaction};

/// 客户端数据库的文件种类标识（ASCII `CLNT`），写入 SQLite `PRAGMA application_id`。
///
/// 服务端库用另一个标识（`crates/server-store` 的 `SERVER_APPLICATION_ID`，ASCII `VSN1`）。
/// 两个方向都必须拒绝：把服务端库交给客户端会被本模块拒绝，反之由服务端门禁拒绝。
pub const CLIENT_APPLICATION_ID: i32 = 0x434C_4E54;

/// 二进制内嵌的客户端迁移集合，来源目录 `crates/client-store/migrations/`（design §5.4）。
///
/// 编译期内嵌（而不是运行时读目录）保证客户端二进制不带错版本的 SQL；已发布的迁移被改写会
/// 由 SQLx 的校验和比对拒绝启动。
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// 连接池上限。WAL 下只允许一个写者，写靠 `BEGIN IMMEDIATE` + busy timeout 串行化；
/// 池只用于并发读。
const MAX_CONNECTIONS: u32 = 4;

/// 等锁上限：迁移与写事务拿不到锁时等这么久后明确失败，不无限阻塞启动。
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// 本二进制实现的客户端 schema 版本，即内嵌迁移里的最大版本号。
pub fn target_schema_version() -> i64 {
    target_version_of(&MIGRATOR)
}

fn target_version_of(migrator: &Migrator) -> i64 {
    migrator.iter().map(|m| m.version).max().unwrap_or(0)
}

/// 迁移前一致性备份的路径：与数据库同目录的
/// `<库文件名>.pre-migration-v<from>-to-v<to>.bak`。
pub fn migration_backup_path(database: &Path, from_version: i64, to_version: i64) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(format!(
        ".pre-migration-v{from_version}-to-v{to_version}.bak"
    ));
    PathBuf::from(name)
}

/// 已通过种类/版本门禁并完成迁移的客户端数据库句柄。
#[derive(Debug)]
pub struct Database {
    pool: SqlitePool,
    path: PathBuf,
    schema_version: i64,
}

impl Database {
    /// 打开（或创建）`path` 处的客户端数据库，必要时先做一致性备份并执行迁移。
    ///
    /// 顺序固定为：探测种类与版本 → 拒绝非客户端库 → 确认 WAL → 迁移前备份 → 执行迁移 →
    /// 写回种类与版本标记 → 建立连接池。任一步失败都返回 [`DbError`]，不返回可写句柄。
    ///
    /// 父目录必须已存在（状态目录由应用层准备，本模块只创建数据库文件本身）。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        Self::open_with(path.as_ref(), &MIGRATOR).await
    }

    /// 用给定迁移集合打开数据库。生产路径固定用 [`MIGRATOR`]；测试用它构造“上一版 schema
    /// 的旧库”“多走一版迁移”等场景，验证同一段门禁与备份逻辑。
    async fn open_with(path: &Path, migrator: &Migrator) -> Result<Self, DbError> {
        let path = path.to_path_buf();
        if path.to_str().is_none() {
            // SQLite 只接受 UTF-8 文件名；备份路径也从同一字符串派生，先在这里明确拒绝。
            return Err(DbError::NonUtf8Path { path });
        }

        let target = target_version_of(migrator);
        // 已存在的库先只读探测：分类与版本不对就立刻返回，绝不改动对方的文件（例如把服务端库
        // 的 journal_mode 改成 WAL）。`metadata` 只在库路径上取一次文件状态，属于启动路径。
        let inspection = match std::fs::metadata(&path) {
            Ok(_) => {
                let mut probe = connect_for_inspection(&path)
                    .connect()
                    .await
                    .map_err(DbError::Sqlite)?;
                let inspection = inspect(&mut probe, &path).await?;
                probe.close().await.map_err(DbError::Sqlite)?;
                inspection
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Inspection::new_database()
            }
            Err(source) => return Err(DbError::Io { path, source }),
        };
        inspection.check_kind()?;
        inspection.check_version(target)?;

        let mut conn = connect_options(&path)
            .connect()
            .await
            .map_err(DbError::Sqlite)?;
        ensure_wal(&mut conn).await?;

        let from_version = inspection.migrated_version();
        let backup = if inspection.needs_migration(migrator, target) && !inspection.is_empty() {
            Some(create_migration_backup(&mut conn, &path, from_version, target).await?)
        } else {
            None
        };

        // SQLx 逐个比对已应用迁移的校验和并按版本号续跑；失败时给出备份路径，库停在
        // “最后一个成功迁移”的状态，由调用方决定恢复整套备份还是换上匹配的二进制。
        migrator
            .run(&mut conn)
            .await
            .map_err(|source| DbError::Migration { backup, source })?;

        if inspection.application_id != CLIENT_APPLICATION_ID || inspection.user_version != target {
            write_markers(&mut conn, target).await?;
        }
        conn.close().await.map_err(DbError::Sqlite)?;

        let pool = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect_with(connect_options(&path))
            .await
            .map_err(DbError::Sqlite)?;

        Ok(Self {
            pool,
            path,
            schema_version: target,
        })
    }

    /// 数据库文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 打开时确认的 schema 版本，等于 [`target_schema_version`]。
    pub fn schema_version(&self) -> i64 {
        self.schema_version
    }

    /// 连接池：只读查询直接借用；任何写操作必须走 [`Database::begin_write`]。
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// 短写事务：`BEGIN IMMEDIATE` 先拿写锁（避免“读事务升级写锁”时的 `SQLITE_BUSY`），
    /// 等锁上限是 10 秒（连接选项里的 `busy_timeout`）。提交用 `commit()`，丢弃即回滚。
    ///
    /// 事务里只允许数据库语句：文件系统 I/O、哈希计算、网络请求与等待都会把 SQLite 写锁拖长
    /// （design §6、§7.4）。
    pub async fn begin_write(&self) -> Result<Transaction<'static, Sqlite>, DbError> {
        self.pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(DbError::Sqlite)
    }

    /// 关闭连接池并等待在用连接归还。
    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// 打开/迁移客户端数据库的失败。命中任一分支都表示“不要启动同步写入”。
#[derive(Debug)]
pub enum DbError {
    /// 数据库文件读写失败（探测前的 `metadata`）。
    Io {
        /// 数据库路径。
        path: PathBuf,
        /// 底层 I/O 错误。
        source: std::io::Error,
    },
    /// 数据库路径不是 UTF-8（SQLite 只接受 UTF-8 文件名，备份路径也由它派生）。
    NonUtf8Path {
        /// 数据库路径。
        path: PathBuf,
    },
    /// SQLite 层面失败（连接、查询、连接池）。
    Sqlite(sqlx::Error),
    /// 给出的文件不是 SQLite 数据库（例如把别的文件误当数据库）。
    NotADatabase {
        /// 数据库路径。
        path: PathBuf,
    },
    /// 数据库种类标识不是客户端库：`0` 是未标记的非空库，其它值属于别的产品（例如服务端库）。
    ForeignDatabase {
        /// 文件里记录的 `PRAGMA application_id`。
        application_id: i32,
    },
    /// 数据库 schema 比本二进制新：旧客户端不写新 schema，只能换二进制或整套恢复备份。
    SchemaTooNew {
        /// 库里记录的版本（`user_version` 与 `_sqlx_migrations` 中较大者）。
        found: i64,
        /// 本二进制实现的版本。
        supported: i64,
    },
    /// 迁移前备份写入失败。
    Backup {
        /// 备份目标路径。
        path: PathBuf,
        /// 底层 SQLite 错误。
        source: sqlx::Error,
    },
    /// 迁移前备份自检未通过（`PRAGMA quick_check`）。
    BackupUnverified {
        /// 备份路径。
        path: PathBuf,
        /// `quick_check` 的原始输出。
        detail: String,
    },
    /// 数据库无法进入 WAL 模式（例如只读存储或网络文件系统）。
    WalUnavailable {
        /// 实际读到的 `journal_mode`。
        journal_mode: String,
    },
    /// 迁移执行或校验失败；`backup` 是迁移前的一致性备份（空库迁移时为 `None`）。
    Migration {
        /// 迁移前备份路径；失败时用它做整套恢复。
        backup: Option<PathBuf>,
        /// SQLx 迁移错误（含失败版本号）。
        source: sqlx::migrate::MigrateError,
    },
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "读取数据库路径 {} 失败：{source}", path.display())
            }
            Self::NonUtf8Path { path } => {
                write!(f, "数据库路径 {} 不是 UTF-8", path.display())
            }
            Self::Sqlite(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::NotADatabase { path } => {
                write!(f, "文件 {} 不是 SQLite 数据库", path.display())
            }
            Self::ForeignDatabase { application_id } => write!(
                f,
                "数据库种类标识 0x{application_id:08X} 不是客户端库（0 表示未标记的非空库）"
            ),
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "数据库 schema 版本 {found} 高于本二进制支持的 {supported}"
            ),
            Self::Backup { path, source } => {
                write!(f, "迁移前备份 {} 失败：{source}", path.display())
            }
            Self::BackupUnverified { path, detail } => write!(
                f,
                "迁移前备份 {} 未通过 quick_check：{detail}",
                path.display()
            ),
            Self::WalUnavailable { journal_mode } => write!(
                f,
                "数据库无法进入 WAL 模式（实际 journal_mode={journal_mode}）"
            ),
            Self::Migration {
                backup: Some(backup),
                source,
            } => write!(f, "迁移失败：{source}；迁移前备份在 {}", backup.display()),
            Self::Migration {
                backup: None,
                source,
            } => write!(f, "迁移失败：{source}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Sqlite(source) | Self::Backup { source, .. } => Some(source),
            Self::Migration { source, .. } => Some(source),
            Self::NonUtf8Path { .. }
            | Self::NotADatabase { .. }
            | Self::ForeignDatabase { .. }
            | Self::SchemaTooNew { .. }
            | Self::BackupUnverified { .. }
            | Self::WalUnavailable { .. } => None,
        }
    }
}

/// 正常打开的连接选项：WAL、外键、有界等锁。
fn connect_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // WAL 是库级持久设置；`Database::open` 之后仍会读取实际模式，切换不成功就拒绝启动。
        .journal_mode(SqliteJournalMode::Wal)
        // 外键是每条连接的开关，必须默认打开，而不是等某个调用点临时打开。
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
}

/// 只读探测用的连接选项：不创建文件、不改 journal_mode、不设外键，因此分类与版本不对时
/// 不会改动对方的文件。
fn connect_for_inspection(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
}

/// 已存在数据库的种类、版本与内容概况。
#[derive(Debug)]
struct Inspection {
    application_id: i32,
    user_version: i64,
    /// `_sqlx_migrations` 已记录的迁移版本（表不存在时为空）。
    applied: Vec<i64>,
    /// `sqlite_master` 里的对象数：`0` 表示还没有任何 schema（空库或刚创建）。
    objects: i64,
}

impl Inspection {
    /// 文件路径尚不存在时的状态：等同刚创建的空库。
    fn new_database() -> Self {
        Self {
            application_id: 0,
            user_version: 0,
            applied: Vec::new(),
            objects: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.objects == 0
    }

    /// 库实际对应的迁移版本：标记值与已应用迁移里的较大者。
    fn migrated_version(&self) -> i64 {
        self.user_version
            .max(self.applied.iter().copied().max().unwrap_or(0))
    }

    fn needs_migration(&self, migrator: &Migrator, target: i64) -> bool {
        self.user_version != target
            || migrator
                .iter()
                .any(|migration| !self.applied.contains(&migration.version))
    }

    /// 种类门禁：客户端标识放行；空库（没有 schema 对象）允许被初始化为客户端库；
    /// 其它一律拒绝——服务端库、未标记的非空库或用户自己的 SQLite 文件都不能被客户端接管。
    fn check_kind(&self) -> Result<(), DbError> {
        if self.application_id == CLIENT_APPLICATION_ID {
            return Ok(());
        }
        if self.application_id == 0 && self.is_empty() {
            return Ok(());
        }
        Err(DbError::ForeignDatabase {
            application_id: self.application_id,
        })
    }

    /// 版本门禁：库里任一来源（标记或迁移记账）新于本二进制就拒绝写。
    ///
    /// 两个来源都看，是因为中断在“迁移已提交、标记未写”之间时只有 `_sqlx_migrations` 记得
    /// 新版本；只看 `user_version` 会把半完成的库当成旧库继续写。
    fn check_version(&self, target: i64) -> Result<(), DbError> {
        let found = self.migrated_version();
        if found > target {
            return Err(DbError::SchemaTooNew {
                found,
                supported: target,
            });
        }
        Ok(())
    }
}

/// 读取种类、版本与已应用迁移版本。只读：不建表、不改 pragma。
async fn inspect(conn: &mut SqliteConnection, path: &Path) -> Result<Inspection, DbError> {
    let application_id: i32 =
        query_scalar_or_not_a_database(conn, path, "PRAGMA application_id").await?;
    let user_version: i32 =
        query_scalar_or_not_a_database(conn, path, "PRAGMA user_version").await?;
    let objects: i64 =
        query_scalar_or_not_a_database(conn, path, "SELECT count(*) FROM sqlite_master").await?;
    let has_migrations_table: i64 = query_scalar_or_not_a_database(
        conn,
        path,
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .await?;
    let applied = if has_migrations_table > 0 {
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *conn)
            .await
            .map_err(DbError::Sqlite)?
    } else {
        Vec::new()
    };

    Ok(Inspection {
        application_id,
        user_version: i64::from(user_version),
        applied,
        objects,
    })
}

/// 执行一条探测查询；SQLITE_NOTADB（错误码 26）单独映射成“这不是数据库”。
async fn query_scalar_or_not_a_database<T>(
    conn: &mut SqliteConnection,
    path: &Path,
    sql: &str,
) -> Result<T, DbError>
where
    T: for<'r> sqlx::Decode<'r, Sqlite> + sqlx::Type<Sqlite> + Send + Unpin,
{
    sqlx::query_scalar(sql)
        .fetch_one(&mut *conn)
        .await
        .map_err(|error| match &error {
            sqlx::Error::Database(database_error)
                if database_error.code().as_deref() == Some("26") =>
            {
                DbError::NotADatabase {
                    path: path.to_path_buf(),
                }
            }
            _ => DbError::Sqlite(error),
        })
}

/// 确认连接真的处于 WAL，而不是“设了但没生效”（只读存储、网络文件系统等）。
async fn ensure_wal(conn: &mut SqliteConnection) -> Result<(), DbError> {
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::Sqlite)?;
    if journal_mode.eq_ignore_ascii_case("wal") {
        return Ok(());
    }
    Err(DbError::WalUnavailable { journal_mode })
}

/// 用 `VACUUM INTO` 导出一致性备份，并用 `PRAGMA quick_check` 自检。
///
/// 同名备份已存在时保留旧文件、不再覆盖：它更接近迁移前的原始状态。旧库若已有 WAL 内容，
/// `VACUUM INTO` 通过读事务取快照，因此不会漏掉只存在于 `-wal` 里的已提交数据。
async fn create_migration_backup(
    conn: &mut SqliteConnection,
    database: &Path,
    from_version: i64,
    to_version: i64,
) -> Result<PathBuf, DbError> {
    let backup = migration_backup_path(database, from_version, to_version);
    if backup.exists() {
        return Ok(backup);
    }
    let target = backup
        .to_str()
        .ok_or_else(|| DbError::NonUtf8Path {
            path: backup.clone(),
        })?
        .to_owned();
    sqlx::query("VACUUM INTO ?1")
        .bind(&target)
        .execute(&mut *conn)
        .await
        .map_err(|source| DbError::Backup {
            path: backup.clone(),
            source,
        })?;
    verify_backup(&backup).await?;
    Ok(backup)
}

/// 用独立只读连接对备份做 `PRAGMA quick_check`：备份存在但不可读等于没有备份。
async fn verify_backup(backup: &Path) -> Result<(), DbError> {
    let backup_error = |source: sqlx::Error| DbError::Backup {
        path: backup.to_path_buf(),
        source,
    };
    let mut check = connect_for_inspection(backup)
        .read_only(true)
        .connect()
        .await
        .map_err(backup_error)?;
    let detail: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(&mut check)
        .await
        .map_err(backup_error)?;
    check.close().await.map_err(backup_error)?;
    if detail != "ok" {
        return Err(DbError::BackupUnverified {
            path: backup.to_path_buf(),
            detail,
        });
    }
    Ok(())
}

/// 迁移成功后写回种类与版本标记。`PRAGMA` 不支持绑定参数，值全部来自本文件的常量与内嵌
/// 迁移版本号，不含外部输入。
async fn write_markers(conn: &mut SqliteConnection, target: i64) -> Result<(), DbError> {
    sqlx::query(&format!("PRAGMA application_id = {CLIENT_APPLICATION_ID}"))
        .execute(&mut *conn)
        .await
        .map_err(DbError::Sqlite)?;
    sqlx::query(&format!("PRAGMA user_version = {target}"))
        .execute(&mut *conn)
        .await
        .map_err(DbError::Sqlite)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// `migrations/0001_initial.sql` 的真实内容，作为“上一版 schema”的 fixture。
    const BASELINE_SQL: &str = include_str!("../migrations/0001_initial.sql");
    /// 迁移目录里的文件名（SQLx 以文件名前缀的数字作为版本号）。
    const BASELINE_FILE: &str = "0001_initial.sql";
    /// `migrations/0002_events_transfers_intents.sql` 的真实内容。
    const EVENTS_SQL: &str = include_str!("../migrations/0002_events_transfers_intents.sql");
    const EVENTS_FILE: &str = "0002_events_transfers_intents.sql";
    /// `migrations/0003_upload_resumes.sql` 的真实内容。
    const UPLOAD_RESUMES_SQL: &str = include_str!("../migrations/0003_upload_resumes.sql");
    const UPLOAD_RESUMES_FILE: &str = "0003_upload_resumes.sql";

    /// 服务端库的种类标识（`crates/server-store` 的 `SERVER_APPLICATION_ID`，ASCII `VSN1`）。
    /// 这里写常量而不依赖 server-store：本 crate 不得链接服务端存储，测试只验证门禁方向。
    const SERVER_APPLICATION_ID_FIXTURE: i32 = 0x5653_4E31;

    /// 每个测试独占的临时目录，`Drop` 时删除（含 WAL 与备份文件）。
    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let index = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "vaultsyncd-s030-{}-{label}-{index}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self { path }
        }

        fn database(&self) -> PathBuf {
            self.path.join("client.sqlite")
        }

        /// 按给定文件写出迁移目录，供 `Migrator::new` 解析。
        fn migrations(&self, files: &[(&str, &str)]) -> PathBuf {
            let dir = self.path.join("migrations");
            std::fs::create_dir_all(&dir).expect("创建 migrations 目录");
            for (name, sql) in files {
                std::fs::write(dir.join(name), sql).expect("写入迁移文件");
            }
            dir
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    async fn migrator(dir: &Path) -> Migrator {
        Migrator::new(dir).await.expect("解析迁移目录")
    }

    /// 本二进制之前的集合（只有 0001），用于“旧库由上一版客户端建出”的场景。
    async fn baseline_migrator(dir: &TestDir) -> Migrator {
        migrator(&dir.migrations(&[(BASELINE_FILE, BASELINE_SQL)])).await
    }

    /// 比本二进制多一版迁移的合成集合：验证“schema 比二进制新”的拒绝，而不是提前发布假迁移。
    async fn next_migrator(dir: &TestDir) -> Migrator {
        let migration_dir = dir.path.join("next-migrations");
        std::fs::create_dir_all(&migration_dir).expect("迁移目录");
        for migration in MIGRATOR.iter() {
            let name = format!("{:04}_{}.sql", migration.version, migration.description);
            std::fs::write(migration_dir.join(name), migration.sql.as_bytes()).expect("当前迁移");
        }
        std::fs::write(
            migration_dir.join(format!("{:04}_probe.sql", target_schema_version() + 1)),
            "CREATE TABLE probe (id INTEGER PRIMARY KEY) STRICT;",
        )
        .expect("合成未来迁移");
        migrator(&migration_dir).await
    }

    async fn scalar<T>(database: &Path, sql: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, Sqlite> + sqlx::Type<Sqlite> + Send + Unpin + std::fmt::Debug,
    {
        let mut conn = connect_for_inspection(database)
            .read_only(true)
            .connect()
            .await
            .expect("打开只读连接");
        let value = sqlx::query_scalar(sql)
            .fetch_one(&mut conn)
            .await
            .expect("执行只读查询");
        conn.close().await.expect("关闭只读连接");
        value
    }

    async fn insert_probe(database: &Database, value: i64) {
        let mut tx = database.begin_write().await.expect("开始写事务");
        sqlx::query("INSERT INTO probe (id) VALUES (?1)")
            .bind(value)
            .execute(&mut *tx)
            .await
            .expect("写入 probe");
        tx.commit().await.expect("提交写事务");
    }

    #[tokio::test]
    async fn empty_database_is_initialized_as_client_store() {
        let dir = TestDir::new("empty");
        let path = dir.database();
        let database = Database::open(&path).await.expect("打开新库");

        assert_eq!(database.schema_version(), target_schema_version());
        assert!(target_schema_version() >= 9, "必须包含受控释放迁移");
        assert_eq!(database.path(), path.as_path());
        assert_eq!(
            scalar::<i32>(&path, "PRAGMA application_id").await,
            CLIENT_APPLICATION_ID
        );
        assert_eq!(
            scalar::<i32>(&path, "PRAGMA user_version").await,
            target_schema_version() as i32
        );
        assert_eq!(
            scalar::<i64>(
                &path,
                "SELECT count(*) FROM _sqlx_migrations WHERE success = 1"
            )
            .await,
            MIGRATOR.iter().count() as i64
        );
        // 客户端状态表包含上传续传及首次绑定日志，不含服务端表。
        let tables: Vec<(String,)> = {
            let mut conn = connect_for_inspection(&path)
                .read_only(true)
                .connect()
                .await
                .expect("打开只读连接");
            let rows = sqlx::query_as(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations' ORDER BY name",
            )
            .fetch_all(&mut conn)
            .await
            .expect("列出表");
            conn.close().await.expect("关闭只读连接");
            rows
        };
        assert_eq!(
            tables.into_iter().map(|(name,)| name).collect::<Vec<_>>(),
            vec![
                "agent_actions".to_owned(),
                "agent_connection".to_owned(),
                "agent_evidence".to_owned(),
                "agent_versions".to_owned(),
                "baseline_entries".to_owned(),
                "binding_controls".to_owned(),
                "binding_full_proofs".to_owned(),
                "binding_imports".to_owned(),
                "binding_participation".to_owned(),
                "binding_resyncs".to_owned(),
                "binding_rules_history".to_owned(),
                "bindings".to_owned(),
                "controlled_applies".to_owned(),
                "controlled_releases".to_owned(),
                "import_id_map".to_owned(),
                "local_entries".to_owned(),
                "local_intents".to_owned(),
                "notifications".to_owned(),
                "operation_schedule".to_owned(),
                "pending_ops".to_owned(),
                "received_changes".to_owned(),
                "remote_entries".to_owned(),
                "resync_quarantined_ops".to_owned(),
                "transfers".to_owned(),
                "upload_resumes".to_owned(),
            ]
        );
        // 空库没有可丢的数据，因此不产生迁移前备份。
        assert!(
            !migration_backup_path(&path, 0, target_schema_version()).exists(),
            "空库迁移不应产生备份"
        );

        // 再次打开是幂等的：不迁移、不新增备份。
        database.close().await;
        let reopened = Database::open(&path).await.expect("重新打开");
        assert_eq!(reopened.schema_version(), target_schema_version());
        assert!(
            !migration_backup_path(&path, target_schema_version(), target_schema_version())
                .exists()
        );
        reopened.close().await;
    }

    #[tokio::test]
    async fn wal_foreign_keys_and_busy_timeout_apply_to_every_pooled_connection() {
        let dir = TestDir::new("pragmas");
        let path = dir.database();
        let database = Database::open(&path).await.expect("打开新库");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *conn)
            .await
            .expect("读 journal_mode");
        let foreign_keys: i32 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *conn)
            .await
            .expect("读 foreign_keys");
        let busy_timeout: i32 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&mut *conn)
            .await
            .expect("读 busy_timeout");
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(busy_timeout, BUSY_TIMEOUT.as_millis() as i32);

        // 外键真的生效：local_entries 指向不存在的 binding 必须失败。
        let violation = sqlx::query(
            "INSERT INTO local_entries (binding_id, entry_id, generation, dirty, residency) \
             VALUES (x'00', x'00', 0, 'clean', 'materialized')",
        )
        .execute(&mut *conn)
        .await;
        assert!(
            violation.is_err(),
            "外键违反应失败，实际 {:?}",
            violation.map(|result| result.rows_affected())
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn write_transaction_holds_the_lock_at_begin() {
        let dir = TestDir::new("write-tx");
        let path = dir.database();
        let database = Database::open(&path).await.expect("打开新库");

        let mut tx = database.begin_write().await.expect("开始写事务");
        sqlx::query("CREATE TABLE probe (id INTEGER PRIMARY KEY) STRICT")
            .execute(&mut *tx)
            .await
            .expect("事务内写入");

        // `BEGIN IMMEDIATE` 在开始时就持有写锁：另一个连接此时连“立刻开始的写事务”都拿不到锁。
        // 若是延迟 `BEGIN`，这句会成功（延迟事务不取锁），因此这里正好验证短写事务的语义。
        let mut other = connect_options(&path)
            .busy_timeout(Duration::from_millis(100))
            .connect()
            .await
            .expect("打开旁路连接");
        let blocked = sqlx::query("BEGIN IMMEDIATE").execute(&mut other).await;
        let error = blocked.expect_err("写事务期间另一连接不应拿到写锁");
        let code = error
            .as_database_error()
            .and_then(|database_error| database_error.code())
            .map(|code| code.to_string())
            .unwrap_or_default();
        assert!(
            code == "5" || code == "6",
            "期望 SQLITE_BUSY/LOCKED（5/6），实际代码 {code}：{error}"
        );
        other.close().await.expect("关闭旁路连接");
        tx.commit().await.expect("提交写事务");
        database.close().await;
    }

    #[tokio::test]
    async fn server_database_is_rejected_without_being_modified() {
        let dir = TestDir::new("server-db");
        let path = dir.database();
        // 服务端库：种类标识 + 服务端表 + 非零版本标记，模拟被误指过来的真实服务端状态。
        let mut conn = connect_options(&path).connect().await.expect("建库");
        sqlx::query(&format!(
            "PRAGMA application_id = {SERVER_APPLICATION_ID_FIXTURE}"
        ))
        .execute(&mut conn)
        .await
        .expect("写种类标识");
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut conn)
            .await
            .expect("写版本标记");
        sqlx::query("CREATE TABLE entries (id INTEGER PRIMARY KEY) STRICT")
            .execute(&mut conn)
            .await
            .expect("建服务端表");
        conn.close().await.expect("关闭连接");

        let error = Database::open(&path).await.expect_err("服务端库必须被拒绝");
        assert!(
            matches!(
                error,
                DbError::ForeignDatabase { application_id }
                    if application_id == SERVER_APPLICATION_ID_FIXTURE
            ),
            "实际错误 {error:?}"
        );

        // 拒绝时不得改动对方文件：种类、版本、表结构与 journal_mode 都原样保留，
        // 也没有任何客户端表被建出来。
        assert_eq!(
            scalar::<i32>(&path, "PRAGMA application_id").await,
            SERVER_APPLICATION_ID_FIXTURE
        );
        assert_eq!(scalar::<i32>(&path, "PRAGMA user_version").await, 2);
        assert_eq!(
            scalar::<i64>(
                &path,
                "SELECT count(*) FROM sqlite_master WHERE name = 'entries'"
            )
            .await,
            1
        );
        for table in [
            "bindings",
            "baseline_entries",
            "remote_entries",
            "local_entries",
            "pending_ops",
        ] {
            assert_eq!(
                scalar::<i64>(
                    &path,
                    &format!("SELECT count(*) FROM sqlite_master WHERE name = '{table}'")
                )
                .await,
                0,
                "拒绝时不应建出 {table}"
            );
        }
    }

    #[tokio::test]
    async fn unmarked_non_empty_database_is_not_adopted() {
        let dir = TestDir::new("unmarked");
        let path = dir.database();
        let mut conn = connect_options(&path).connect().await.expect("建库");
        sqlx::query("CREATE TABLE someone_else (id INTEGER PRIMARY KEY) STRICT")
            .execute(&mut conn)
            .await
            .expect("建表");
        conn.close().await.expect("关闭连接");

        let error = Database::open(&path)
            .await
            .expect_err("未标记的非空库必须被拒绝");
        assert!(
            matches!(error, DbError::ForeignDatabase { application_id: 0 }),
            "实际错误 {error:?}"
        );
        assert_eq!(
            scalar::<i64>(
                &path,
                "SELECT count(*) FROM sqlite_master WHERE name = 'someone_else'"
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn non_database_file_is_rejected() {
        let dir = TestDir::new("not-a-database");
        let path = dir.database();
        std::fs::write(&path, b"this is not a sqlite database").expect("写非数据库文件");
        let error = Database::open(&path)
            .await
            .expect_err("非数据库文件必须被拒绝");
        assert!(
            matches!(error, DbError::NotADatabase { .. }),
            "实际错误 {error:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("读回文件"),
            b"this is not a sqlite database"
        );
    }

    #[tokio::test]
    async fn fixture_from_previous_version_upgrades_with_backup() {
        let dir = TestDir::new("upgrade");
        let path = dir.database();

        // 旧 fixture：上一版客户端（只有 0001），已经带着绑定数据与队列数据。
        let previous = baseline_migrator(&dir).await;
        let old = Database::open_with(&path, &previous).await.expect("建旧库");
        let binding = crate::test_support::seed_binding(&old, 0x21).await;
        old.close().await;

        // 本二进制打开旧库：真实执行 0002—0006，保留旧数据与恢复上下文。
        let upgraded = Database::open(&path).await.expect("升级旧库");
        assert_eq!(upgraded.schema_version(), target_schema_version());
        let mut conn = upgraded.pool().acquire().await.expect("取连接");
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN \
             ('received_changes', 'transfers', 'local_intents', 'notifications', 'upload_resumes', 'binding_imports', 'import_id_map') \
             ORDER BY name",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("列新表");
        assert_eq!(rows.len(), 7, "0002—0004 应建出七张新表");
        assert_eq!(
            crate::events::cursors(&mut conn, binding)
                .await
                .expect("读游标"),
            crate::events::Cursors {
                received: vaultsyncd_domain::version::RootSeq::ZERO,
                applied: vaultsyncd_domain::version::RootSeq::ZERO,
            },
            "升级后游标从 0 开始"
        );
        assert!(
            crate::bindings::binding(&mut conn, binding)
                .await
                .expect("读绑定")
                .is_some(),
            "升级不清空旧数据"
        );
        drop(conn);
        upgraded.close().await;

        assert_eq!(
            scalar::<i32>(&path, "PRAGMA user_version").await,
            target_schema_version() as i32
        );
        // 迁移前备份存在，且内容是**迁移前**的状态：没有新表。
        let backup = migration_backup_path(&path, 1, target_schema_version());
        assert!(backup.exists(), "升级前应有一致性备份");
        for table in [
            "received_changes",
            "transfers",
            "local_intents",
            "notifications",
            "upload_resumes",
        ] {
            assert_eq!(
                scalar::<i64>(
                    &backup,
                    &format!("SELECT count(*) FROM sqlite_master WHERE name = '{table}'")
                )
                .await,
                0,
                "迁移前备份不应含 {table}"
            );
        }

        // 再打开一次：版本已到目标，不重复迁移、不覆盖已有备份。
        let again = Database::open(&path).await.expect("重新打开");
        assert_eq!(again.schema_version(), target_schema_version());
        again.close().await;
    }

    #[tokio::test]
    async fn version_three_upgrade_keeps_upload_resume_and_creates_empty_import_journal() {
        let dir = TestDir::new("upgrade-v3");
        let path = dir.database();
        let previous = migrator(&dir.migrations(&[
            (BASELINE_FILE, BASELINE_SQL),
            (EVENTS_FILE, EVENTS_SQL),
            (UPLOAD_RESUMES_FILE, UPLOAD_RESUMES_SQL),
        ]))
        .await;
        let old = Database::open_with(&path, &previous).await.expect("旧版库");
        let binding = crate::test_support::seed_binding(&old, 0x58).await;
        let op = vaultsyncd_domain::ids::OpId::from_bytes([0x58; 16]);
        let mut tx = old.begin_write().await.expect("事务");
        crate::queue::enqueue(
            &mut tx,
            &crate::queue::NewPendingOp {
                op,
                binding,
                kind: "upload_new_entry".to_owned(),
                entry: None,
                baseline_revision: None,
                baseline_version: None,
                depends_on: None,
                pinned_version: None,
            },
        )
        .await
        .expect("旧版队列");
        crate::upload_resumes::save(
            &mut tx,
            &crate::upload_resumes::NewUploadResume {
                op,
                binding,
                snapshot_identity: "generation-1".to_owned(),
                total_size: 42,
                digest: [0x58; 32],
                session: "old-session".to_owned(),
            },
        )
        .await
        .expect("旧版续传");
        tx.commit().await.expect("提交");
        old.close().await;
        let upgraded = Database::open(&path).await.expect("升级 0006");
        assert_eq!(upgraded.schema_version(), target_schema_version());
        let mut conn = upgraded.pool().acquire().await.expect("连接");
        let resume = crate::upload_resumes::load(&mut conn, op)
            .await
            .expect("旧会话")
            .expect("旧会话仍在");
        assert!(resume.matches_snapshot("generation-1", 42, &[0x58; 32]));
        assert!(
            crate::queue::pending_op(&mut conn, op)
                .await
                .expect("旧操作")
                .is_some()
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM binding_imports")
            .fetch_one(&mut *conn)
            .await
            .expect("空日志");
        assert_eq!(count, 0, "迁移不能把曾经同步的绑定标为首次导入");
        drop(conn);
        upgraded.close().await;
        assert!(
            migration_backup_path(&path, 3, target_schema_version()).exists(),
            "升级前一致性备份"
        );
    }

    #[tokio::test]
    async fn version_four_upgrade_preserves_open_intent_import_and_ack_mapping() {
        let dir = TestDir::new("upgrade-v4");
        let path = dir.database();
        let previous = migrator(&dir.migrations(&[
            (BASELINE_FILE, BASELINE_SQL),
            (EVENTS_FILE, EVENTS_SQL),
            (UPLOAD_RESUMES_FILE, UPLOAD_RESUMES_SQL),
            (
                "0004_first_import.sql",
                include_str!("../migrations/0004_first_import.sql"),
            ),
        ]))
        .await;
        let old = Database::open_with(&path, &previous)
            .await
            .expect("0004 库");
        let binding = crate::test_support::seed_binding(&old, 0x59).await;
        let mut tx = old.begin_write().await.expect("旧库事务");
        let intent = crate::intents::begin_intent(
            &mut tx,
            &crate::intents::NewLocalIntent {
                binding,
                kind: crate::intents::LocalIntentKind::Replace,
                entry: None,
                target_path: "protected.txt".to_owned(),
                source_path: Some("staged.part".to_owned()),
                content_hash: Some(vec![0x59; 32]),
                generation: Some(vaultsyncd_domain::version::Generation::new(7).unwrap()),
            },
        )
        .await
        .expect("旧意图");
        sqlx::query("INSERT INTO binding_imports(binding_id, journal, revision) VALUES (?, '{\"fixed_snapshot\":\"old\"}', 3)")
            .bind(binding.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO import_id_map VALUES (?, ?, ?, ?, '{\"state\":\"committed\"}')")
            .bind(binding.as_bytes().as_slice())
            .bind(vec![1u8; 16])
            .bind(vec![2u8; 16])
            .bind(vec![3u8; 16])
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        old.close().await;
        let upgraded = Database::open(&path).await.expect("升级 0006");
        assert_eq!(upgraded.schema_version(), target_schema_version());
        let mut conn = upgraded.pool().acquire().await.unwrap();
        assert_eq!(
            crate::intents::intent(&mut conn, intent.id).await.unwrap(),
            Some(intent)
        );
        let imports: (String, i64) =
            sqlx::query_as("SELECT journal, revision FROM binding_imports")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(imports, ("{\"fixed_snapshot\":\"old\"}".to_owned(), 3));
        let ack: String = sqlx::query_scalar("SELECT result_json FROM import_id_map")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(ack, "{\"state\":\"committed\"}");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM controlled_applies")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(count, 0, "旧意图不能被猜成受控应用完成或自动重放");
        drop(conn);
        upgraded.close().await;
        let backup = migration_backup_path(&path, 4, target_schema_version());
        assert!(backup.exists());
        assert_eq!(
            scalar::<i64>(&backup, "SELECT count(*) FROM local_intents").await,
            1
        );
        assert_eq!(
            scalar::<i64>(
                &backup,
                "SELECT count(*) FROM sqlite_master WHERE name = 'controlled_applies'"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn version_five_upgrade_preserves_apply_queue_and_backup_and_rejects_old_binary() {
        let dir = TestDir::new("upgrade-v5-resync");
        let path = dir.database();
        let previous = migrator(&dir.migrations(&[
            (BASELINE_FILE, BASELINE_SQL),
            (EVENTS_FILE, EVENTS_SQL),
            (UPLOAD_RESUMES_FILE, UPLOAD_RESUMES_SQL),
            (
                "0004_first_import.sql",
                include_str!("../migrations/0004_first_import.sql"),
            ),
            (
                "0005_controlled_apply.sql",
                include_str!("../migrations/0005_controlled_apply.sql"),
            ),
        ]))
        .await;
        let old = Database::open_with(&path, &previous).await.unwrap();
        let binding = crate::test_support::seed_binding(&old, 0x60).await;
        let op = vaultsyncd_domain::ids::OpId::from_bytes([0x60; 16]);
        let mut tx = old.begin_write().await.unwrap();
        crate::queue::enqueue(
            &mut tx,
            &crate::queue::NewPendingOp {
                op,
                binding,
                kind: "delete".to_owned(),
                entry: None,
                baseline_revision: None,
                baseline_version: None,
                depends_on: None,
                pinned_version: None,
            },
        )
        .await
        .unwrap();
        crate::upload_resumes::save(
            &mut tx,
            &crate::upload_resumes::NewUploadResume {
                op,
                binding,
                snapshot_identity: "uncommitted-snapshot".to_owned(),
                total_size: 8,
                digest: [0x60; 32],
                session: "old-session".to_owned(),
            },
        )
        .await
        .unwrap();
        let intent = crate::intents::begin_intent(
            &mut tx,
            &crate::intents::NewLocalIntent {
                binding,
                kind: crate::intents::LocalIntentKind::Replace,
                entry: None,
                target_path: "protected.txt".to_owned(),
                source_path: Some("staged.part".to_owned()),
                content_hash: Some(vec![0x60; 32]),
                generation: None,
            },
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO controlled_applies(op_id, binding_id, target_path, intent_id, journal) VALUES (?, ?, 'protected.txt', ?, '{\"fixed_version\":\"old\"}')")
            .bind(op.as_bytes().as_slice()).bind(binding.as_bytes().as_slice()).bind(intent.id).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        old.close().await;

        let upgraded = Database::open(&path).await.unwrap();
        assert_eq!(upgraded.schema_version(), target_schema_version());
        let mut conn = upgraded.pool().acquire().await.unwrap();
        assert!(
            crate::queue::pending_op(&mut conn, op)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            crate::upload_resumes::load(&mut conn, op)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            crate::intents::intent(&mut conn, intent.id).await.unwrap(),
            Some(intent)
        );
        let journal: String = sqlx::query_scalar("SELECT journal FROM controlled_applies")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(journal, "{\"fixed_version\":\"old\"}");
        let resyncs: i64 = sqlx::query_scalar("SELECT count(*) FROM binding_resyncs")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(resyncs, 0, "升级不能猜测旧设备重建已完成");
        drop(conn);
        upgraded.close().await;
        let backup = migration_backup_path(&path, 5, target_schema_version());
        assert!(backup.exists());
        assert_eq!(
            scalar::<String>(&backup, "PRAGMA integrity_check").await,
            "ok"
        );
        assert_eq!(
            scalar::<i64>(&backup, "SELECT count(*) FROM controlled_applies").await,
            1
        );
        assert_eq!(
            scalar::<i64>(&backup, "SELECT count(*) FROM pending_ops").await,
            1
        );
        assert_eq!(
            scalar::<i64>(
                &backup,
                "SELECT count(*) FROM sqlite_master WHERE name = 'binding_resyncs'"
            )
            .await,
            0
        );
        let checksums = "SELECT group_concat(hex(checksum), ',') FROM (SELECT checksum FROM _sqlx_migrations WHERE version <= 5 ORDER BY version)";
        assert_eq!(
            scalar::<String>(&path, checksums).await,
            scalar::<String>(&backup, checksums).await
        );
        let rejected = Database::open_with(&path, &previous).await.unwrap_err();
        assert!(matches!(
            rejected,
            DbError::SchemaTooNew {
                found, supported: 5
            } if found == target_schema_version()
        ));
        assert_eq!(
            scalar::<String>(&path, "PRAGMA integrity_check").await,
            "ok"
        );
        assert_eq!(
            scalar::<i64>(&path, "SELECT count(*) FROM upload_resumes").await,
            1
        );
    }

    #[tokio::test]
    async fn version_six_upgrade_keeps_dirty_queue_and_adds_binding_controls_with_backup() {
        let dir = TestDir::new("s061-upgrade");
        let path = dir.database();
        let migration_dir = dir.path.join("version-six");
        std::fs::create_dir_all(&migration_dir).unwrap();
        for migration in MIGRATOR.iter().filter(|m| m.version <= 6) {
            std::fs::write(
                migration_dir.join(format!(
                    "{:04}_{}.sql",
                    migration.version, migration.description
                )),
                migration.sql.as_bytes(),
            )
            .unwrap();
        }
        let previous = migrator(&migration_dir).await;
        let old = Database::open_with(&path, &previous).await.unwrap();
        let mut tx = old.begin_write().await.unwrap();
        let binding = [0x61_u8; 16];
        // Explicit columns keep this fixture independent of future binding metadata.
        sqlx::query("INSERT INTO bindings(id,server_id,device_id,root_id,local_path,cache_path,mode,rules_version,server_epoch) VALUES (?,?,?,?,?,?,'full','old',1)")
            .bind(binding.as_slice()).bind([1_u8;16].as_slice()).bind([2_u8;16].as_slice()).bind([3_u8;16].as_slice()).bind("C:/source").bind("C:/cache").execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency) VALUES (?,?,9,'awaiting_commit','materialized')")
            .bind(binding.as_slice()).bind([4_u8;16].as_slice()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind) VALUES (?,?,'upload_content')")
            .bind([5_u8; 16].as_slice())
            .bind(binding.as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        old.close().await;
        let upgraded = Database::open(&path).await.unwrap();
        assert_eq!(upgraded.schema_version(), target_schema_version());
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT dirty FROM local_entries")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            "awaiting_commit"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pending_ops")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM binding_controls")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            0
        );
        let backup = migration_backup_path(&path, 6, target_schema_version());
        assert!(backup.exists());
        assert_eq!(
            scalar::<String>(&backup, "PRAGMA integrity_check").await,
            "ok"
        );
        assert_eq!(
            scalar::<i64>(&backup, "SELECT count(*) FROM pending_ops").await,
            1
        );
        let checksums = "SELECT group_concat(hex(checksum), ',') FROM (SELECT checksum FROM _sqlx_migrations WHERE version<=6 ORDER BY version)";
        assert_eq!(
            scalar::<String>(&path, checksums).await,
            scalar::<String>(&backup, checksums).await
        );
        assert!(
            matches!(Database::open_with(&path,&previous).await.unwrap_err(),DbError::SchemaTooNew {found,supported:6} if found==target_schema_version())
        );
    }

    #[tokio::test]
    async fn version_seven_upgrade_preserves_paused_dirty_queue_and_backs_up_before_scheduler() {
        let dir = TestDir::new("s062-upgrade-v7");
        let path = dir.database();
        let migration_dir = dir.path.join("version-seven");
        std::fs::create_dir_all(&migration_dir).unwrap();
        for migration in MIGRATOR.iter().filter(|m| m.version <= 7) {
            std::fs::write(
                migration_dir.join(format!(
                    "{:04}_{}.sql",
                    migration.version, migration.description
                )),
                migration.sql.as_bytes(),
            )
            .unwrap();
        }
        let previous = migrator(&migration_dir).await;
        let old = Database::open_with(&path, &previous).await.unwrap();
        let binding = crate::test_support::seed_binding(&old, 0x62).await;
        let mut tx = old.begin_write().await.unwrap();
        sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency) VALUES (?, ?,9,'awaiting_commit','materialized')")
            .bind(binding.as_bytes().as_slice()).bind([4_u8;16].as_slice()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind,attempts,last_error_code) VALUES (?,?,'apply_remote',2,'PLACEHOLDER_OFFLINE')")
            .bind([5_u8;16].as_slice()).bind(binding.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO binding_controls(binding_id,paused) VALUES (?,1)")
            .bind(binding.as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        old.close().await;
        let upgraded = Database::open(&path).await.unwrap();
        assert_eq!(upgraded.schema_version(), target_schema_version());
        for (sql, expected) in [
            ("SELECT dirty FROM local_entries", "awaiting_commit"),
            (
                "SELECT last_error_code FROM pending_ops",
                "PLACEHOLDER_OFFLINE",
            ),
        ] {
            assert_eq!(
                sqlx::query_scalar::<_, String>(sql)
                    .fetch_one(upgraded.pool())
                    .await
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM pending_ops")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT paused FROM binding_controls")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM operation_schedule")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            0
        );
        let backup = migration_backup_path(&path, 7, target_schema_version());
        assert!(backup.exists());
        assert_eq!(
            scalar::<String>(&backup, "PRAGMA integrity_check").await,
            "ok"
        );
        assert_eq!(
            scalar::<i64>(&backup, "SELECT attempts FROM pending_ops").await,
            2
        );
        assert_eq!(
            scalar::<i64>(
                &backup,
                "SELECT count(*) FROM sqlite_master WHERE name='operation_schedule'"
            )
            .await,
            0
        );
        let checksums = "SELECT group_concat(hex(checksum),',') FROM (SELECT checksum FROM _sqlx_migrations WHERE version<=7 ORDER BY version)";
        assert_eq!(
            scalar::<String>(&path, checksums).await,
            scalar::<String>(&backup, checksums).await
        );
        assert!(
            matches!(Database::open_with(&path,&previous).await.unwrap_err(),DbError::SchemaTooNew{found,supported:7} if found==target_schema_version())
        );
    }

    #[tokio::test]
    async fn version_nine_upgrade_keeps_pending_work_backs_up_and_refuses_old_writer() {
        let dir = TestDir::new("s070-upgrade-v9");
        let path = dir.database();
        let prior_dir = dir.path.join("version-nine");
        std::fs::create_dir_all(&prior_dir).unwrap();
        for migration in MIGRATOR.iter().filter(|m| m.version <= 9) {
            std::fs::write(
                prior_dir.join(format!(
                    "{:04}_{}.sql",
                    migration.version, migration.description
                )),
                migration.sql.as_bytes(),
            )
            .unwrap();
        }
        let previous = migrator(&prior_dir).await;
        let old = Database::open_with(&path, &previous).await.unwrap();
        let binding = crate::test_support::seed_binding(&old, 0x70).await;
        sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency) VALUES(?,?,23,'dirty','materialized')").bind(binding.as_bytes().as_slice()).bind([7u8;16].as_slice()).execute(old.pool()).await.unwrap();
        sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind,attempts) VALUES(?,?,'upload_content',1)").bind([8u8;16].as_slice()).bind(binding.as_bytes().as_slice()).execute(old.pool()).await.unwrap();
        old.close().await;
        let upgraded = Database::open(&path).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT dirty FROM local_entries")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            "dirty"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM pending_ops")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            1
        );
        assert!(migration_backup_path(&path, 9, target_schema_version()).is_file());
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('agent_evidence','agent_versions','agent_actions')").fetch_one(upgraded.pool()).await.unwrap(),3);
        upgraded.close().await;
        assert!(
            matches!(Database::open_with(&path,&previous).await.unwrap_err(),DbError::SchemaTooNew{found,supported:9} if found==target_schema_version())
        );
        assert_eq!(
            scalar::<i64>(&path, "SELECT generation FROM local_entries").await,
            23
        );
    }

    #[tokio::test]
    async fn version_ten_upgrade_preserves_unconfirmed_work_and_persists_connection_without_secrets()
     {
        let dir = TestDir::new("s072-upgrade-v10");
        let path = dir.database();
        let prior = dir.path.join("v10");
        std::fs::create_dir(&prior).unwrap();
        for migration in MIGRATOR.iter().filter(|m| m.version <= 10) {
            std::fs::write(
                prior.join(format!(
                    "{:04}_{}.sql",
                    migration.version, migration.description
                )),
                migration.sql.as_bytes(),
            )
            .unwrap();
        }
        let previous = migrator(&prior).await;
        let old = Database::open_with(&path, &previous).await.unwrap();
        let binding = crate::test_support::seed_binding(&old, 0x72).await;
        sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency) VALUES(?,?,72,'awaiting_commit','materialized')").bind(binding.as_bytes().as_slice()).bind([7u8;16].as_slice()).execute(old.pool()).await.unwrap();
        sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind,attempts) VALUES(?,?,'upload_content',2)").bind([8u8;16].as_slice()).bind(binding.as_bytes().as_slice()).execute(old.pool()).await.unwrap();
        old.close().await;
        let db = Database::open(&path).await.unwrap();
        sqlx::query("INSERT INTO agent_connection VALUES(1,?,'https://sync.example',NULL)")
            .bind("72".repeat(16))
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT dirty FROM local_entries")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            "awaiting_commit"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM pending_ops")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            2
        );
        db.close().await;
        let backup = migration_backup_path(&path, 10, target_schema_version());
        assert!(backup.is_file());
        assert!(matches!(
            Database::open_with(&path, &previous).await.unwrap_err(),
            DbError::SchemaTooNew { supported: 10, .. }
        ));
        let reopened = Database::open(&path).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT server_url FROM agent_connection")
                .fetch_one(reopened.pool())
                .await
                .unwrap(),
            "https://sync.example"
        );
        reopened.close().await;
        let checksums = "SELECT group_concat(hex(checksum),',') FROM (SELECT checksum FROM _sqlx_migrations WHERE version<=10 ORDER BY version)";
        assert_eq!(
            scalar::<String>(&path, checksums).await,
            scalar::<String>(&backup, checksums).await
        );
        assert_eq!(
            scalar::<i64>(&backup, "SELECT generation FROM local_entries").await,
            72
        );
    }

    #[tokio::test]
    async fn version_eight_upgrade_preserves_retry_cancellation_and_open_intent_before_release() {
        let dir = TestDir::new("s063-upgrade-v8");
        let path = dir.database();
        let migration_dir = dir.path.join("version-eight");
        std::fs::create_dir_all(&migration_dir).unwrap();
        for migration in MIGRATOR.iter().filter(|m| m.version <= 8) {
            std::fs::write(
                migration_dir.join(format!(
                    "{:04}_{}.sql",
                    migration.version, migration.description
                )),
                migration.sql.as_bytes(),
            )
            .unwrap();
        }
        let previous = migrator(&migration_dir).await;
        let old = Database::open_with(&path, &previous).await.unwrap();
        let binding = crate::test_support::seed_binding(&old, 0x63).await;
        let mut tx = old.begin_write().await.unwrap();
        sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency) VALUES (?, ?,17,'awaiting_commit','materialized')")
            .bind(binding.as_bytes().as_slice()).bind([4_u8;16].as_slice()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind,attempts,last_error_code) VALUES (?,?,'apply_remote',2,'PLACEHOLDER_OFFLINE')")
            .bind([5_u8;16].as_slice()).bind(binding.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO operation_schedule(op_id,priority,due_at,cancelled,offline) VALUES (?,0,1234,1,1)")
            .bind([5_u8;16].as_slice()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO local_intents(binding_id,kind,entry_id,target_path,generation) VALUES (?,'replace',?,'pending.txt',17)")
            .bind(binding.as_bytes().as_slice()).bind([4_u8;16].as_slice()).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        old.close().await;
        let upgraded = Database::open(&path).await.unwrap();
        assert_eq!(upgraded.schema_version(), target_schema_version());
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT dirty FROM local_entries")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            "awaiting_commit"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM pending_ops")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            2
        );
        let schedule: (i64, i64, bool, bool) =
            sqlx::query_as("SELECT priority,due_at,cancelled,offline FROM operation_schedule")
                .fetch_one(upgraded.pool())
                .await
                .unwrap();
        assert_eq!(schedule, (0, 1234, true, true));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM local_intents")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM controlled_releases")
                .fetch_one(upgraded.pool())
                .await
                .unwrap(),
            0
        );
        let backup = migration_backup_path(&path, 8, target_schema_version());
        assert!(backup.exists());
        assert_eq!(
            scalar::<String>(&backup, "PRAGMA integrity_check").await,
            "ok"
        );
        assert_eq!(
            scalar::<i64>(&backup, "SELECT due_at FROM operation_schedule").await,
            1234
        );
        let checksums = "SELECT group_concat(hex(checksum),',') FROM (SELECT checksum FROM _sqlx_migrations WHERE version<=8 ORDER BY version)";
        assert_eq!(
            scalar::<String>(&path, checksums).await,
            scalar::<String>(&backup, checksums).await
        );
        assert!(
            matches!(Database::open_with(&path,&previous).await.unwrap_err(),DbError::SchemaTooNew{found,supported:8} if found==target_schema_version())
        );
    }

    #[tokio::test]
    async fn schema_newer_than_binary_is_refused_before_any_write() {
        let dir = TestDir::new("too-new");
        let path = dir.database();

        let next = next_migrator(&dir).await;
        let newer = Database::open_with(&path, &next).await.expect("建新库");
        insert_probe(&newer, 1).await;
        newer.close().await;

        let error = Database::open(&path)
            .await
            .expect_err("旧客户端必须拒绝更新的 schema");
        assert!(
            matches!(
                error,
                DbError::SchemaTooNew {
                    found, supported
                } if found == target_schema_version() + 1 && supported == target_schema_version()
            ),
            "实际错误 {error:?}"
        );
        assert_eq!(
            scalar::<i32>(&path, "PRAGMA user_version").await,
            (target_schema_version() + 1) as i32
        );
        assert_eq!(
            scalar::<i64>(&path, "SELECT count(*) FROM probe").await,
            1,
            "拒绝写入时不得改动数据"
        );
    }
}
