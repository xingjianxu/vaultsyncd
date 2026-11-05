//! 工作目录发布意图（`fs_intents` 表）：显式 phase、前后状态与恢复所需路径
//! （`docs/design.md` §7.2、§11.2—11.3）。
//!
//! 边界：本模块只持久化“要对 Root 里的哪个目标做什么、依据什么现场证据、走到哪一步”。它不发布
//! 工作文件、不操作对象、不提交元数据事务，也不决定什么时候该重做还是该停下——那是提交意图状态机
//! （S034）与启动恢复的职责。本模块给它们的是**可查询的持久证据**。
//!
//! 不变量：
//!
//! - **意图先于磁盘改动持久化**（design §11.2 步骤 5）：[`begin_intent`] 写入 `prepared`；只有在工作
//!   文件按 `desired_after` 发布之后才推进到 `published`；元数据事务完成后才 `finalized`。
//!   因此“工作文件已换而数据库未更新”这种中间状态是可查询的事实，而不是靠日志猜的。
//! - **phase 有明确转换**（[`FsIntentPhase::allows_transition_to`]）：
//!   `prepared → published | needs_recovery`；`published → finalized | needs_recovery`；
//!   `needs_recovery → finalized`（人工核对后的收尾）；`finalized` 是终态。其余组合一律
//!   [`IntentError::IllegalTransition`]，不能用重跑一次意图来“跳过”未知现场。
//! - **前后状态都记录、都不为 NULL**：`expected_before` / `desired_after` 是 JSON object；
//!   “当时不存在”必须是一个字段（例如 `{"present":false}`），而不是 NULL——`NULL` 只能表达“未知”，
//!   把“不存在”混进未知里，恢复时就无法区分“新建”和“读不出来”。具体编码属 S034。
//! - **目标路径受约束**：`target_path` 是 Root 内的相对路径，非空、无 NUL、不以 `/` 起始、不含空
//!   分量与 `.`/`..` 分量。恢复会拿它去定位工作文件，把 `../` 放进数据库等于把恢复指向 Root 之外。
//! - **持久化 phase 是唯一判据**（design §11.3「只重放可证明的操作，不依赖最后一条普通日志推测
//!   事实」）：[`unfinished_intents`] 按 Root 列出所有未到 `finalized` 的意图（含 `needs_recovery`），
//!   启动恢复先处理它们再开放写入。
//! - **owner scope**：读写都带 [`UserId`]，经 `roots.owner_id` 判定。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{ID_LEN, IntentId, RootId, UserId};
use vaultsyncd_domain::version::UtcTimestamp;

/// `fs_intents` 表的列顺序，读取点共用。
const INTENT_COLUMNS: &str = "intent_id, root_id, kind, target_path, staging_path, \
                              expected_before, desired_after, phase, created_at, updated_at";

/// 意图对应的文件系统动作种类（`fs_intents.kind`）。
///
/// 意图日志同样覆盖纯移动、创建目录与删除（design §11.2 末段），因此种类在表里显式记录，
/// 恢复时不需要从 `desired_after` 反推动作。本卡只提供种类，具体执行属 S034/S038/S039。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsIntentKind {
    /// 用不可变对象的内容替换（或新建）一个文件。
    PutContent,
    /// 创建目录。
    Create,
    /// 同 Root 内移动。
    Move,
    /// 删除（可恢复删除，design §11.2 末段与回收站一致）。
    Delete,
}

impl FsIntentKind {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::PutContent => "put_content",
            Self::Create => "create",
            Self::Move => "move",
            Self::Delete => "delete",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜种类。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "put_content" => Some(Self::PutContent),
            "create" => Some(Self::Create),
            "move" => Some(Self::Move),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// 意图的持久化阶段（`fs_intents.phase`），对应 design §11.3 恢复矩阵的四行证据。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsIntentPhase {
    /// 意图已持久化，工作文件未发布：满足 `expected_before` 就可以重做（§11.3 第 3 行）。
    Prepared,
    /// 工作文件已按 `desired_after` 发布，元数据事务未完成：核对目标后完成同一操作（第 4 行）。
    Published,
    /// 元数据事务已完成：意图收尾（终态）。
    Finalized,
    /// 现场与意图都无法解释：暂停该 Root 的破坏性写入并给出恢复报告（第 6 行）。
    NeedsRecovery,
}

impl FsIntentPhase {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Published => "published",
            Self::Finalized => "finalized",
            Self::NeedsRecovery => "needs_recovery",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜阶段。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "prepared" => Some(Self::Prepared),
            "published" => Some(Self::Published),
            "finalized" => Some(Self::Finalized),
            "needs_recovery" => Some(Self::NeedsRecovery),
            _ => None,
        }
    }

    /// 是否已经收尾（只有 `finalized`）。
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Finalized)
    }

    /// 是否处于“还可以自动推进”的两个阶段。
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Prepared | Self::Published)
    }

    /// 允许的阶段转换。`needs_recovery → finalized` 是唯一的人工出口：人工核对现场后按已发布内容
    /// 收尾；它不允许退回 `prepared`/`published`，否则会把“无法解释”重新当成“可以重做”。
    pub const fn allows_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Prepared, Self::Published)
                | (Self::Prepared, Self::NeedsRecovery)
                | (Self::Published, Self::Finalized)
                | (Self::Published, Self::NeedsRecovery)
                | (Self::NeedsRecovery, Self::Finalized)
        )
    }
}

/// 新建一条发布意图的输入。
///
/// 不变量（违反时 [`begin_intent`] 报错且不写入）：Root 属于调用方；`target_path` 是 Root 内相对
/// 路径（见模块文档）；`staging_path` 给了就必须非空；`expected_before` 与 `desired_after` 是合法
/// JSON object；`at` 不早于任何约束下限（由调用方注入时钟，本层不读时钟）；`intent_id` 未被占用。
#[derive(Clone, Debug)]
pub struct NewFsIntent {
    /// 意图身份。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 动作种类。
    pub kind: FsIntentKind,
    /// 目标在 Root 内的相对路径。
    pub target_path: String,
    /// 发布用的暂存文件；不需要暂存的动作（删除、移动）为 `None`。
    pub staging_path: Option<String>,
    /// 意图前提（JSON object）：意图开始时观察到的现场，含“当时不存在”。
    pub expected_before: String,
    /// 意图目标（JSON object）：发布完成后现场应有的样子。
    pub desired_after: String,
}

/// 一条发布意图（`fs_intents` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsIntent {
    /// 意图身份。
    pub intent_id: IntentId,
    /// 所属 Root。
    pub root: RootId,
    /// 动作种类。
    pub kind: FsIntentKind,
    /// 目标在 Root 内的相对路径。
    pub target_path: String,
    /// 发布用的暂存文件。
    pub staging_path: Option<String>,
    /// 意图前提（JSON object 文本）。
    pub expected_before: String,
    /// 意图目标（JSON object 文本）。
    pub desired_after: String,
    /// 持久化阶段。
    pub phase: FsIntentPhase,
    /// 意图写入时间。
    pub created_at: UtcTimestamp,
    /// 阶段最后一次推进的时间。
    pub updated_at: UtcTimestamp,
}

/// 意图读写的失败。
#[derive(Debug)]
pub enum IntentError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// Root 不存在或不属于调用方的 owner scope（两种情况不区分）。
    OutOfScope,
    /// 意图不存在或不在调用方的 owner scope 内。
    UnknownIntent {
        /// 请求的意图。
        id: IntentId,
    },
    /// 意图身份已被占用。
    DuplicateId {
        /// 冲突的意图。
        id: IntentId,
    },
    /// 目标路径不是 Root 内相对路径。
    InvalidTargetPath {
        /// 给出的路径。
        path: String,
        /// 不合格的具体原因。
        reason: &'static str,
    },
    /// 暂存路径不合格（空或含 NUL）。
    InvalidStagingPath {
        /// 具体原因。
        reason: &'static str,
    },
    /// `expected_before` / `desired_after` 不是合法 JSON object。
    InvalidStateJson {
        /// 哪个字段。
        field: &'static str,
        /// 具体原因。
        reason: &'static str,
    },
    /// 阶段转换不在允许集合内。
    IllegalTransition {
        /// 当前阶段。
        from: FsIntentPhase,
        /// 请求的目标阶段。
        to: FsIntentPhase,
    },
    /// 读到的阶段与写入时不一致（写事务串行，出现即说明绕过本模块写入）。
    PhaseConflict {
        /// 意图。
        id: IntentId,
    },
    /// 给定的时间早于意图已记录的时间（阶段推进必须单调，否则恢复顺序失去意义）。
    TimeWentBackwards {
        /// 意图。
        id: IntentId,
        /// 已记录的时间。
        recorded: UtcTimestamp,
        /// 给出的时间。
        given: UtcTimestamp,
    },
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for IntentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::OutOfScope => f.write_str("Root 不存在或不在调用方的 owner scope 内"),
            Self::UnknownIntent { id } => write!(f, "意图 {id} 不存在"),
            Self::DuplicateId { id } => write!(f, "意图身份 {id} 已被占用"),
            Self::InvalidTargetPath { path, reason } => {
                write!(f, "目标路径 {path:?} {reason}")
            }
            Self::InvalidStagingPath { reason } => write!(f, "暂存路径{reason}"),
            Self::InvalidStateJson { field, reason } => {
                write!(f, "意图字段 {field} 不合格：{reason}")
            }
            Self::IllegalTransition { from, to } => {
                write!(f, "意图不能从 {} 转到 {}", from.as_text(), to.as_text())
            }
            Self::PhaseConflict { id } => write!(f, "意图 {id} 的阶段在读取后又被改动"),
            Self::TimeWentBackwards {
                id,
                recorded,
                given,
            } => write!(f, "意图 {id} 的时间 {given} 早于已记录的 {recorded}"),
            Self::MalformedRow { column } => write!(f, "数据库列 {column} 的值不符合类型前提"),
        }
    }
}

impl std::error::Error for IntentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            _ => None,
        }
    }
}

/// 写入一条 `prepared` 意图（design §11.2 步骤 5：先持久化意图，再发布工作文件）。
pub async fn begin_intent(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    new: &NewFsIntent,
    at: UtcTimestamp,
) -> Result<FsIntent, IntentError> {
    require_root_owner(tx, owner, new.root).await?;
    validate_target_path(&new.target_path)?;
    if let Some(staging) = &new.staging_path {
        if staging.is_empty() {
            return Err(IntentError::InvalidStagingPath {
                reason: "不能为空"
            });
        }
        if staging.contains('\0') {
            return Err(IntentError::InvalidStagingPath {
                reason: "不能含 NUL",
            });
        }
    }
    require_json_object(tx, "expected_before", &new.expected_before).await?;
    require_json_object(tx, "desired_after", &new.desired_after).await?;

    let inserted = sqlx::query(
        "INSERT INTO fs_intents \
         (intent_id, root_id, kind, target_path, staging_path, expected_before, desired_after, \
          phase, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
    )
    .bind(new.intent_id.as_bytes().as_slice())
    .bind(new.root.as_bytes().as_slice())
    .bind(new.kind.as_text())
    .bind(&new.target_path)
    .bind(new.staging_path.as_deref())
    .bind(&new.expected_before)
    .bind(&new.desired_after)
    .bind(FsIntentPhase::Prepared.as_text())
    .bind(at.to_db_integer())
    .execute(&mut **tx)
    .await;
    if let Err(error) = inserted {
        return Err(classify_insert_error(error, new.intent_id));
    }
    intent_in_scope(tx, owner, new.intent_id)
        .await?
        .ok_or(IntentError::OutOfScope)
}

/// 推进一条意图的持久化阶段；只能按 [`FsIntentPhase::allows_transition_to`] 前进。
pub async fn advance_intent_phase(
    tx: &mut Transaction<'_, Sqlite>,
    owner: UserId,
    id: IntentId,
    next: FsIntentPhase,
    at: UtcTimestamp,
) -> Result<FsIntent, IntentError> {
    let record = require_intent(tx, owner, id).await?;
    if !record.phase.allows_transition_to(next) {
        return Err(IntentError::IllegalTransition {
            from: record.phase,
            to: next,
        });
    }
    if at < record.updated_at {
        return Err(IntentError::TimeWentBackwards {
            id,
            recorded: record.updated_at,
            given: at,
        });
    }
    let updated = sqlx::query(
        "UPDATE fs_intents SET phase = ?1, updated_at = ?2 \
         WHERE intent_id = ?3 AND phase = ?4",
    )
    .bind(next.as_text())
    .bind(at.to_db_integer())
    .bind(id.as_bytes().as_slice())
    .bind(record.phase.as_text())
    .execute(&mut **tx)
    .await
    .map_err(IntentError::Db)?;
    if updated.rows_affected() != 1 {
        return Err(IntentError::PhaseConflict { id });
    }
    require_intent(tx, owner, id).await
}

/// 按身份读一条意图；不存在或不在 owner scope 内返回 `None`。
pub async fn intent_in_scope(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: IntentId,
) -> Result<Option<FsIntent>, IntentError> {
    let row = sqlx::query_as::<_, IntentRow>(&format!(
        "SELECT {INTENT_COLUMNS} FROM fs_intents i JOIN roots r ON r.id = i.root_id \
         WHERE i.intent_id = ?1 AND r.owner_id = ?2"
    ))
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(IntentError::Db)?;
    row.map(IntentRow::into_record).transpose()
}

/// 列出某个 Root 下尚未收尾的意图（`prepared`/`published`/`needs_recovery`），按写入时间升序。
///
/// 这是启动恢复的入口（design §11.3：启动先恢复未完成意图，再开放写入和事件游标）。`needs_recovery`
/// 也在列表里：它需要处理，只是不能自动推进。
pub async fn unfinished_intents(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<Vec<FsIntent>, IntentError> {
    require_root_owner(conn, owner, root).await?;
    let rows = sqlx::query_as::<_, IntentRow>(&format!(
        "SELECT {INTENT_COLUMNS} FROM fs_intents i WHERE i.root_id = ?1 \
         AND i.phase <> 'finalized' ORDER BY i.created_at, i.intent_id"
    ))
    .bind(root.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(IntentError::Db)?;
    rows.into_iter().map(IntentRow::into_record).collect()
}

/// 读一条必须存在的意图。
async fn require_intent(
    conn: &mut SqliteConnection,
    owner: UserId,
    id: IntentId,
) -> Result<FsIntent, IntentError> {
    intent_in_scope(conn, owner, id)
        .await?
        .ok_or(IntentError::UnknownIntent { id })
}

/// 确认 Root 属于调用方的 owner scope。
async fn require_root_owner(
    conn: &mut SqliteConnection,
    owner: UserId,
    root: RootId,
) -> Result<(), IntentError> {
    let found: Option<Vec<u8>> = sqlx::query_scalar("SELECT owner_id FROM roots WHERE id = ?1")
        .bind(root.as_bytes().as_slice())
        .fetch_optional(conn)
        .await
        .map_err(IntentError::Db)?;
    match found {
        Some(bytes) if bytes.as_slice() == owner.as_bytes() => Ok(()),
        _ => Err(IntentError::OutOfScope),
    }
}

/// 目标路径检查：Root 内相对路径，非空、无 NUL、不以 `/` 起始、无空分量与 `.`/`..` 分量。
///
/// 不用宿主 `Path` 判断：规则必须与服务端路径语义一致，而不是与运行本 crate 的主机一致
/// （便携检查会在 Windows 上编译并跑这些测试）。
fn validate_target_path(path: &str) -> Result<(), IntentError> {
    let invalid = |reason: &'static str| IntentError::InvalidTargetPath {
        path: path.to_owned(),
        reason,
    };
    if path.is_empty() {
        return Err(invalid("不能为空"));
    }
    if path.contains('\0') {
        return Err(invalid("不能含 NUL"));
    }
    if path.starts_with('/') {
        return Err(invalid("必须是 Root 内相对路径"));
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(invalid("不能含空路径分量"));
        }
        if component == "." || component == ".." {
            return Err(invalid("不能含 . 或 .. 分量"));
        }
    }
    Ok(())
}

/// 状态 JSON 检查：必须是合法 JSON object（与迁移里的 CHECK 同一套语义）。
///
/// 用 SQLite 自带的 JSON 函数判定，避免在 Rust 侧再引入一套 JSON 解析惯例；`json_type` 对非法 JSON
/// 会报错，先用 `json_valid` 短接，把“不是 JSON”与“是 JSON 但不是 object”归到同一个可区分错误。
async fn require_json_object(
    conn: &mut SqliteConnection,
    field: &'static str,
    text: &str,
) -> Result<(), IntentError> {
    if text.is_empty() {
        return Err(IntentError::InvalidStateJson {
            field,
            reason: "不能为空",
        });
    }
    let is_object: i64 = sqlx::query_scalar(
        "SELECT CASE WHEN json_valid(?1) THEN json_type(?1) = 'object' ELSE 0 END",
    )
    .bind(text)
    .fetch_one(conn)
    .await
    .map_err(IntentError::Db)?;
    if is_object != 1 {
        return Err(IntentError::InvalidStateJson {
            field,
            reason: "必须是 JSON object",
        });
    }
    Ok(())
}

/// 数据库行到 [`FsIntent`] 的中间形态。
#[derive(sqlx::FromRow)]
struct IntentRow {
    intent_id: Vec<u8>,
    root_id: Vec<u8>,
    kind: String,
    target_path: String,
    staging_path: Option<String>,
    expected_before: String,
    desired_after: String,
    phase: String,
    created_at: i64,
    updated_at: i64,
}

impl IntentRow {
    fn into_record(self) -> Result<FsIntent, IntentError> {
        Ok(FsIntent {
            intent_id: identity(self.intent_id, "intent_id", IntentId::from_bytes)?,
            root: identity(self.root_id, "root_id", RootId::from_bytes)?,
            kind: FsIntentKind::from_text(&self.kind)
                .ok_or(IntentError::MalformedRow { column: "kind" })?,
            target_path: self.target_path,
            staging_path: self.staging_path,
            expected_before: self.expected_before,
            desired_after: self.desired_after,
            phase: FsIntentPhase::from_text(&self.phase)
                .ok_or(IntentError::MalformedRow { column: "phase" })?,
            created_at: timestamp(self.created_at, "created_at")?,
            updated_at: timestamp(self.updated_at, "updated_at")?,
        })
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, IntentError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| IntentError::MalformedRow { column })?;
    Ok(wrap(array))
}

/// 时间列还原；负值按坏行处理。
fn timestamp(value: i64, column: &'static str) -> Result<UtcTimestamp, IntentError> {
    UtcTimestamp::from_db_integer(value).map_err(|_| IntentError::MalformedRow { column })
}

/// 把意图写语句的约束错误翻译成带上下文的类型化错误。
fn classify_insert_error(error: sqlx::Error, id: IntentId) -> IntentError {
    let Some(database_error) = error.as_database_error() else {
        return IntentError::Db(error);
    };
    let message = database_error.message();
    if message.starts_with("UNIQUE constraint failed") {
        return IntentError::DuplicateId { id };
    }
    // 迁移里的 JSON CHECK 与本模块的预检查是同一套语义；走到这里只可能是绕过预检查的写入。
    if message.contains("expected_before") || message.contains("desired_after") {
        return IntentError::InvalidStateJson {
            field: "expected_before/desired_after",
            reason: "必须是 JSON object",
        };
    }
    IntentError::Db(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::db::Database;
    use crate::test_support::{
        TestDir, at, intent as intent_id, open_database, register_root, seed_owner,
    };

    const BEFORE_ABSENT: &str = r#"{"present":false}"#;
    const AFTER_PUT: &str = r#"{"present":true,"object":"00112233","size":"10"}"#;

    fn put(root: RootId, marker: u8) -> NewFsIntent {
        NewFsIntent {
            intent_id: intent_id(marker),
            root,
            kind: FsIntentKind::PutContent,
            target_path: format!("docs/note-{marker:02x}.txt"),
            staging_path: Some(format!("/srv/staging/note-{marker:02x}.txt")),
            expected_before: BEFORE_ABSENT.to_owned(),
            desired_after: AFTER_PUT.to_owned(),
        }
    }

    async fn begin(database: &Database, owner: UserId, new: &NewFsIntent) -> FsIntent {
        let mut tx = database.begin_write().await.expect("开始写事务");
        let record = begin_intent(&mut tx, owner, new, at(100))
            .await
            .expect("写意图");
        tx.commit().await.expect("提交");
        record
    }

    #[tokio::test]
    async fn a_restarted_server_lists_unfinished_intents_in_write_order() {
        let dir = TestDir::new("intents-restart");
        let path = dir.database();
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        let first = begin(&database, owner, &put(root, 0x41)).await;
        assert_eq!(first.phase, FsIntentPhase::Prepared);
        assert_eq!(first.created_at, at(100));
        assert_eq!(first.updated_at, at(100));
        begin(&database, owner, &put(root, 0x42)).await;
        let finished = begin(&database, owner, &put(root, 0x43)).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        advance_intent_phase(
            &mut tx,
            owner,
            finished.intent_id,
            FsIntentPhase::Published,
            at(110),
        )
        .await
        .expect("发布");
        advance_intent_phase(
            &mut tx,
            owner,
            finished.intent_id,
            FsIntentPhase::Finalized,
            at(120),
        )
        .await
        .expect("收尾");
        tx.commit().await.expect("提交");
        database.close().await;

        // 重启后：未收尾的两条按写入时间列出，已收尾的不出现在待恢复清单里。
        let database = Database::open(&path).await.expect("重启后打开库");
        let mut conn = database.pool().acquire().await.expect("取连接");
        let unfinished = unfinished_intents(&mut conn, owner, root)
            .await
            .expect("列出未完成意图");
        assert_eq!(
            unfinished
                .iter()
                .map(|intent| intent.intent_id)
                .collect::<Vec<_>>(),
            vec![intent_id(0x41), intent_id(0x42)]
        );
        assert!(
            unfinished
                .iter()
                .all(|intent| intent.phase == FsIntentPhase::Prepared)
        );
        assert_eq!(
            intent_in_scope(&mut conn, owner, intent_id(0x43))
                .await
                .expect("读意图")
                .expect("意图仍在")
                .phase,
            FsIntentPhase::Finalized,
            "收尾的意图仍可作为证据读回，只是不在待恢复清单里"
        );
        // 别人的账号看不到，也不在别人的清单里。
        assert_eq!(
            intent_in_scope(&mut conn, crate::test_support::user(0x12), intent_id(0x41))
                .await
                .expect("越权读不报错"),
            None
        );
        assert!(
            unfinished_intents(&mut conn, crate::test_support::user(0x12), root)
                .await
                .expect_err("越权列清单必须报错，不能返回空列表")
                .to_string()
                .contains("owner scope")
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn raw_sql_cannot_write_an_unknown_phase_or_a_non_object_state() {
        let dir = TestDir::new("intents-constraints");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        for (sql, expected) in [
            (
                "INSERT INTO fs_intents \
                 (intent_id, root_id, kind, target_path, staging_path, expected_before, \
                  desired_after, phase, created_at, updated_at) \
                 VALUES (?1, ?2, 'put_content', 'a.txt', NULL, '{\"present\":false}', \
                         '{\"present\":true}', 'published_ish', 100, 100)",
                "CHECK",
            ),
            (
                "INSERT INTO fs_intents \
                 (intent_id, root_id, kind, target_path, staging_path, expected_before, \
                  desired_after, phase, created_at, updated_at) \
                 VALUES (?1, ?2, 'put_content', 'a.txt', NULL, '[]', \
                         '{\"present\":true}', 'prepared', 100, 100)",
                "CHECK",
            ),
            (
                "INSERT INTO fs_intents \
                 (intent_id, root_id, kind, target_path, staging_path, expected_before, \
                  desired_after, phase, created_at, updated_at) \
                 VALUES (?1, ?2, 'teleport', 'a.txt', NULL, '{\"present\":false}', \
                         '{\"present\":true}', 'prepared', 100, 100)",
                "CHECK",
            ),
        ] {
            let refused = sqlx::query(sql)
                .bind(intent_id(0x41).as_bytes().as_slice())
                .bind(root.as_bytes().as_slice())
                .execute(&mut *tx)
                .await
                .expect_err("绕过 Rust 的坏行必须被迁移约束拒绝");
            assert!(refused.to_string().contains(expected), "实际错误 {refused}");
        }
        tx.rollback().await.expect("回滚");

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM fs_intents")
            .fetch_one(&mut *database.pool().acquire().await.unwrap())
            .await
            .expect("计数意图");
        assert_eq!(rows, 0);
        database.close().await;
    }

    #[tokio::test]
    async fn phases_only_advance_along_the_declared_transitions() {
        let dir = TestDir::new("intents-phases");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;
        begin(&database, owner, &put(root, 0x41)).await;

        // 非法转换：不能跳过 publish 直接 finalize，也不能从终态回头。
        let mut tx = database.begin_write().await.expect("开始写事务");
        for to in [FsIntentPhase::Finalized] {
            let error = advance_intent_phase(&mut tx, owner, intent_id(0x41), to, at(110))
                .await
                .expect_err("跳阶段必须拒绝");
            assert!(
                matches!(
                    error,
                    IntentError::IllegalTransition {
                        from: FsIntentPhase::Prepared,
                        to: FsIntentPhase::Finalized
                    }
                ),
                "实际错误 {error:?}"
            );
        }
        // 时间不能倒退：阶段推进必须留出可排序的证据。
        let error = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x41),
            FsIntentPhase::Published,
            at(99),
        )
        .await
        .expect_err("时间倒退必须拒绝");
        assert!(
            matches!(error, IntentError::TimeWentBackwards { recorded, given, .. } if recorded == at(100) && given == at(99)),
            "实际错误 {error:?}"
        );
        let published = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x41),
            FsIntentPhase::Published,
            at(110),
        )
        .await
        .expect("发布");
        assert_eq!(published.phase, FsIntentPhase::Published);
        assert_eq!(published.updated_at, at(110));
        // published 不能退回 prepared：那会把“工作文件已换”重新当成“可以重做”。
        let error = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x41),
            FsIntentPhase::Prepared,
            at(120),
        )
        .await
        .expect_err("阶段不能倒退");
        assert!(matches!(error, IntentError::IllegalTransition { .. }));
        let finalized = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x41),
            FsIntentPhase::Finalized,
            at(130),
        )
        .await
        .expect("收尾");
        assert_eq!(finalized.phase, FsIntentPhase::Finalized);
        assert!(finalized.phase.is_finished());
        let error = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x41),
            FsIntentPhase::NeedsRecovery,
            at(140),
        )
        .await
        .expect_err("终态不再变化");
        assert!(matches!(error, IntentError::IllegalTransition { .. }));
        tx.rollback().await.expect("回滚");

        // 无法解释的现场走 needs_recovery，人工核对后只能直接收尾。
        begin(&database, owner, &put(root, 0x42)).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        let blocked = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x42),
            FsIntentPhase::NeedsRecovery,
            at(150),
        )
        .await
        .expect("标记需要恢复");
        assert_eq!(blocked.phase, FsIntentPhase::NeedsRecovery);
        let error = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x42),
            FsIntentPhase::Published,
            at(160),
        )
        .await
        .expect_err("needs_recovery 不能自动推进");
        assert!(matches!(error, IntentError::IllegalTransition { .. }));
        advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x42),
            FsIntentPhase::Finalized,
            at(170),
        )
        .await
        .expect("人工核对后收尾");
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[tokio::test]
    async fn intent_fields_are_validated_before_anything_is_written() {
        let dir = TestDir::new("intents-validation");
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        // 目标路径必须留在 Root 内：恢复会拿它定位工作文件。
        for path in [
            "",
            "/etc/passwd",
            "docs/../escape",
            "docs/./note",
            "docs//note",
            "docs/",
        ] {
            let error = begin_intent(
                &mut tx,
                owner,
                &NewFsIntent {
                    target_path: path.to_owned(),
                    ..put(root, 0x41)
                },
                at(100),
            )
            .await
            .expect_err("不合格路径必须拒绝");
            assert!(
                matches!(error, IntentError::InvalidTargetPath { .. }),
                "路径 {path:?} 的错误 {error:?}"
            );
        }
        let error = begin_intent(
            &mut tx,
            owner,
            &NewFsIntent {
                staging_path: Some(String::new()),
                ..put(root, 0x41)
            },
            at(100),
        )
        .await
        .expect_err("空暂存路径必须拒绝");
        assert!(matches!(error, IntentError::InvalidStagingPath { .. }));
        // 前后状态必须是 JSON object：数组、坏 JSON、空串都不是“现场证据”。
        for (before, after, field) in [
            ("[]".to_owned(), AFTER_PUT.to_owned(), "expected_before"),
            (BEFORE_ABSENT.to_owned(), "{".to_owned(), "desired_after"),
            (String::new(), AFTER_PUT.to_owned(), "expected_before"),
        ] {
            let error = begin_intent(
                &mut tx,
                owner,
                &NewFsIntent {
                    expected_before: before,
                    desired_after: after,
                    ..put(root, 0x41)
                },
                at(100),
            )
            .await
            .expect_err("非 object 的状态必须拒绝");
            match error {
                IntentError::InvalidStateJson { field: actual, .. } => {
                    assert_eq!(actual, field)
                }
                other => panic!("期望 InvalidStateJson，实际 {other:?}"),
            }
        }
        let error = begin_intent(
            &mut tx,
            crate::test_support::user(0x12),
            &put(root, 0x41),
            at(100),
        )
        .await
        .expect_err("别人的 Root 不能写意图");
        assert!(matches!(error, IntentError::OutOfScope));
        tx.rollback().await.expect("回滚");

        // 失败不写入任何行。
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM fs_intents")
            .fetch_one(&mut *database.pool().acquire().await.unwrap())
            .await
            .expect("计数意图");
        assert_eq!(rows, 0);

        begin(&database, owner, &put(root, 0x41)).await;
        let mut tx = database.begin_write().await.expect("开始写事务");
        let error = begin_intent(&mut tx, owner, &put(root, 0x41), at(100))
            .await
            .expect_err("重复身份必须拒绝");
        assert!(
            matches!(error, IntentError::DuplicateId { id } if id == intent_id(0x41)),
            "实际错误 {error:?}"
        );
        let error = advance_intent_phase(
            &mut tx,
            owner,
            intent_id(0x99),
            FsIntentPhase::Published,
            at(110),
        )
        .await
        .expect_err("不存在的意图");
        assert!(matches!(error, IntentError::UnknownIntent { .. }));
        tx.rollback().await.expect("回滚");
        database.close().await;
    }

    /// 提交状态机写出的 before/after 证据必须穿过真实 `fs_intents` 表，并在每个阶段按同一编码还原。
    ///
    /// 恢复只读数据库行与工作目录现场（design §11.3），所以“库里那段 JSON 能否还原成 typed 证据”
    /// 是本表与 S034 状态机之间的契约，必须在真实迁移与 CHECK 约束下验证，而不是只测编码函数。
    #[tokio::test]
    async fn put_evidence_written_by_the_commit_state_machine_round_trips_at_every_phase() {
        use vaultsyncd_server_core::commit::intent::{
            NewPutIntent, PutIntent, PutPhase, TargetAfter, TargetBefore, TargetSnapshot,
        };
        use vaultsyncd_server_core::objects::ContentDigest;
        use vaultsyncd_server_core::ports::object_store::FileKey;

        use crate::test_support::entry;

        const HEX_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
        const HEX_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

        /// 按核心的编码还原一条意图：这是恢复路径实际要做的事。
        fn decode(stored: &FsIntent) -> PutIntent {
            PutIntent {
                intent_id: stored.intent_id,
                root: stored.root,
                target_path: stored.target_path.clone(),
                staging_path: stored.staging_path.clone().expect("put 意图必须记暂存名"),
                before: TargetBefore::from_json(&stored.expected_before).expect("before 可还原"),
                after: TargetAfter::from_json(&stored.desired_after).expect("after 可还原"),
                phase: PutPhase::from_text(stored.phase.as_text()).expect("阶段文本与核心一致"),
            }
        }

        let dir = TestDir::new("intents-put-evidence");
        let path = dir.database();
        let database = open_database(&dir).await;
        let owner = seed_owner(&database, 0x11).await;
        let (root, _root_entry) = register_root(&database, owner, 0x21).await;

        let after = TargetAfter {
            digest: ContentDigest::from_hex(HEX_A).expect("测试摘要"),
            size: 5,
        };
        let create = NewPutIntent {
            intent_id: intent_id(0x41),
            root,
            target_path: "docs/note.txt".to_owned(),
            staging_path: "put-0041".to_owned(),
            before: TargetBefore::Absent,
            after,
        };
        // 覆盖形态：前提带完整的现场证据（条目、revision、内容、文件身份）。
        let overwrite = NewPutIntent {
            intent_id: intent_id(0x42),
            root,
            target_path: "docs/old.txt".to_owned(),
            staging_path: "put-0042".to_owned(),
            before: TargetBefore::Present(TargetSnapshot {
                entry: entry(0x51),
                revision: 7,
                digest: ContentDigest::from_hex(HEX_B).expect("测试摘要"),
                identity: FileKey {
                    device: 7,
                    inode: 41,
                    links: 1,
                    bytes: 9,
                },
            }),
            after,
        };

        let mut tx = database.begin_write().await.expect("开始写事务");
        for (new, marker) in [(&create, 0x41u8), (&overwrite, 0x42)] {
            let record = begin_intent(
                &mut tx,
                owner,
                &NewFsIntent {
                    intent_id: new.intent_id,
                    root,
                    kind: FsIntentKind::PutContent,
                    target_path: new.target_path.clone(),
                    staging_path: Some(new.staging_path.clone()),
                    expected_before: new.before.to_json(),
                    desired_after: new.after.to_json(),
                },
                at(100 + i64::from(marker - 0x41)),
            )
            .await
            .expect("核心编码写入 prepared");
            assert_eq!(record.phase, FsIntentPhase::Prepared);
        }
        tx.commit().await.expect("提交");

        // prepared：两种 before 形态都能从库里还原成与提交时相同的 typed 证据。
        let mut conn = database.pool().acquire().await.expect("取连接");
        for new in [&create, &overwrite] {
            let stored = intent_in_scope(&mut conn, owner, new.intent_id)
                .await
                .expect("读意图")
                .expect("意图仍在");
            assert_eq!(stored.kind, FsIntentKind::PutContent);
            assert_eq!(decode(&stored), new.prepared());
        }

        // published / finalized：阶段文本必须与核心的 `PutPhase` 一一对应，证据文本不变。
        for (step, (next, expected)) in [
            (FsIntentPhase::Published, PutPhase::Published),
            (FsIntentPhase::Finalized, PutPhase::Finalized),
        ]
        .into_iter()
        .enumerate()
        {
            let mut tx = database.begin_write().await.expect("开始写事务");
            advance_intent_phase(
                &mut tx,
                owner,
                create.intent_id,
                next,
                at(110 + step as i64),
            )
            .await
            .expect("推进阶段");
            tx.commit().await.expect("提交");
            let stored = intent_in_scope(&mut conn, owner, create.intent_id)
                .await
                .expect("读意图")
                .expect("意图仍在");
            assert_eq!(decode(&stored).phase, expected);
            assert_eq!(decode(&stored).after, after);
        }
        drop(conn);
        database.close().await;

        // 重启后证据仍在：已收尾的不在待恢复清单里，未完成的覆盖意图可按核心编码还原现场前提。
        let database = Database::open(&path).await.expect("重启后打开库");
        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            decode(
                &intent_in_scope(&mut conn, owner, create.intent_id)
                    .await
                    .expect("读意图")
                    .expect("收尾的意图仍是证据")
            )
            .phase,
            PutPhase::Finalized
        );
        let unfinished = unfinished_intents(&mut conn, owner, root)
            .await
            .expect("列出未完成意图");
        assert_eq!(
            unfinished
                .iter()
                .map(|intent| intent.intent_id)
                .collect::<Vec<_>>(),
            vec![overwrite.intent_id]
        );
        assert_eq!(decode(&unfinished[0]).before, overwrite.before);
        drop(conn);
        database.close().await;
    }
}
