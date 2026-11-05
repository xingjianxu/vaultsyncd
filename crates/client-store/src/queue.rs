//! 本地待执行操作队列（`pending_ops`）的事务内读写（`docs/design.md` §7.3、§8.4、§13.3）。
//!
//! 边界：本模块只持久化“要做什么、依据哪份基线、依赖谁、固定哪个内容版本、失败过几次”。
//! 它不执行操作、不调度重试、不判断错误是否可重试（那是执行器与端口卡的事，design §5.4
//! `crates/agent-core`）。入队与状态更新都发生在调用方已开的 `BEGIN IMMEDIATE` 写事务里。
//!
//! 不变量：
//!
//! - **队列顺序是插入顺序**（`rowid`），不使用客户端时钟：时间戳只用于展示与检测候选变化
//!   （I04），不能决定先后。
//! - **操作类型不在这里冻结**：`kind` 的取值集合由执行器卡（S055）定义，本层只要求非空，
//!   避免提前发布一组字符串再改。
//! - **基线可选**：首次同步的本地新文件没有共同基线；`baseline_revision`/`baseline_version`
//!   同时给出或同时为空由写入方决定，本层只保存。
//! - **依赖必须指向另一条真实存在的操作**：自依赖在写入前被拒绝；指向不存在的操作被拒绝，
//!   不静默降级成“无依赖”。`ON DELETE RESTRICT` 保证被依赖的操作不能在依赖方还在时被删除。
//! - **失败记录不改变其它字段**：[`record_attempt_failure`] 只累加 `attempts` 并写入最近错误码；
//!   它不清除基线、依赖或固定版本，重试仍按原意图重规划（design §13.3）。
//! - **成功即出队**：[`complete`] 删除该行；`pending` 不是 `committed`（I01），出队只发生在
//!   服务器确认之后，由调用方决定何时调用。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, EntryId, ID_LEN, OpId, VersionId};
use vaultsyncd_domain::version::Revision;

/// 入队一条本地操作的输入。
///
/// 不变量（违反时 [`enqueue`] 报错且不写入）：`kind` 非空；`op` 未被占用；`depends_on` 不是
/// 自己，且必须已存在于同一数据库的队列里。
#[derive(Clone, Debug)]
pub struct NewPendingOp {
    /// 操作身份（随机不透明标识，design §7.1）。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 操作类型（取值集合由执行器卡定义）。
    pub kind: String,
    /// 目标条目；本地新建在提交前还没有服务器 `EntryId`。
    pub entry: Option<EntryId>,
    /// 规划时使用的基线修订号。
    pub baseline_revision: Option<Revision>,
    /// 规划时使用的基线内容版本。
    pub baseline_version: Option<VersionId>,
    /// 前置操作。
    pub depends_on: Option<OpId>,
    /// 需要固定（不得被 GC）的内容版本。
    pub pinned_version: Option<VersionId>,
}

/// 从数据库读出的一条 `pending_ops`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOp {
    /// 操作身份。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 操作类型。
    pub kind: String,
    /// 目标条目。
    pub entry: Option<EntryId>,
    /// 基线修订号。
    pub baseline_revision: Option<Revision>,
    /// 基线内容版本。
    pub baseline_version: Option<VersionId>,
    /// 前置操作。
    pub depends_on: Option<OpId>,
    /// 固定内容版本。
    pub pinned_version: Option<VersionId>,
    /// 已尝试次数。
    pub attempts: u32,
    /// 最近一次尝试的失败原因；`None` 表示尚未失败。
    pub last_error: Option<ErrorCode>,
}

/// 队列读写的失败。
#[derive(Debug)]
pub enum QueueError {
    /// SQLite 层面失败（含外键与 `CHECK` 拒绝）。
    Db(sqlx::Error),
    /// 操作类型为空。
    EmptyKind,
    /// 操作身份已被占用。
    DuplicateOp {
        /// 冲突的操作身份。
        op: OpId,
    },
    /// 依赖指向自己。
    SelfDependency {
        /// 该操作。
        op: OpId,
    },
    /// 依赖指向队列里不存在的操作。
    UnknownDependency {
        /// 缺失的前置操作。
        op: OpId,
    },
    /// 数据库行不满足本模块的类型前提（长度、枚举或计数越界）。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::EmptyKind => f.write_str("操作类型为空"),
            Self::DuplicateOp { op } => write!(f, "操作身份 {op} 已在队列里"),
            Self::SelfDependency { op } => write!(f, "操作 {op} 不能依赖自己"),
            Self::UnknownDependency { op } => write!(f, "前置操作 {op} 不在队列里"),
            Self::MalformedRow { column } => {
                write!(f, "数据库列 {column} 的值不符合类型前提")
            }
        }
    }
}

impl std::error::Error for QueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::EmptyKind
            | Self::DuplicateOp { .. }
            | Self::SelfDependency { .. }
            | Self::UnknownDependency { .. }
            | Self::MalformedRow { .. } => None,
        }
    }
}

/// `pending_ops` 的列顺序。
const OP_COLUMNS: &str = "op_id, binding_id, kind, entry_id, baseline_revision, \
                          baseline_version_id, depends_on, pinned_version_id, attempts, \
                          last_error_code";

/// 入队一条操作，返回重新读出的记录。
pub async fn enqueue(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewPendingOp,
) -> Result<PendingOp, QueueError> {
    if new.kind.is_empty() {
        return Err(QueueError::EmptyKind);
    }
    if new.depends_on == Some(new.op) {
        return Err(QueueError::SelfDependency { op: new.op });
    }
    if let Some(dependency) = new.depends_on {
        let found: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT op_id FROM pending_ops WHERE op_id = ?1")
                .bind(dependency.as_bytes().as_slice())
                .fetch_optional(&mut **tx)
                .await
                .map_err(QueueError::Db)?;
        if found.is_none() {
            return Err(QueueError::UnknownDependency { op: dependency });
        }
    }

    let inserted = sqlx::query(
        "INSERT INTO pending_ops \
         (op_id, binding_id, kind, entry_id, baseline_revision, baseline_version_id, depends_on, \
          pinned_version_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(new.op.as_bytes().as_slice())
    .bind(new.binding.as_bytes().as_slice())
    .bind(&new.kind)
    .bind(entry_bytes(new.entry))
    .bind(new.baseline_revision.map(Revision::to_db_integer))
    .bind(version_bytes(new.baseline_version))
    .bind(op_bytes(new.depends_on))
    .bind(version_bytes(new.pinned_version))
    .execute(&mut **tx)
    .await;

    if let Err(error) = inserted {
        if error
            .as_database_error()
            .map(|database_error| database_error.is_unique_violation())
            .unwrap_or(false)
        {
            return Err(QueueError::DuplicateOp { op: new.op });
        }
        return Err(QueueError::Db(error));
    }

    pending_op(tx, new.op)
        .await?
        .ok_or(QueueError::MalformedRow { column: "op_id" })
}

/// 读一条队列操作；不存在返回 `None`。
pub async fn pending_op(
    conn: &mut SqliteConnection,
    op: OpId,
) -> Result<Option<PendingOp>, QueueError> {
    let row = sqlx::query_as::<_, OpRow>(&format!(
        "SELECT {OP_COLUMNS} FROM pending_ops WHERE op_id = ?1"
    ))
    .bind(op.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(QueueError::Db)?;
    row.map(OpRow::into_record).transpose()
}

/// 按插入顺序列出某个绑定的队列。
pub async fn pending_ops_for_binding(
    conn: &mut SqliteConnection,
    binding: BindingId,
) -> Result<Vec<PendingOp>, QueueError> {
    let rows = sqlx::query_as::<_, OpRow>(&format!(
        "SELECT {OP_COLUMNS} FROM pending_ops WHERE binding_id = ?1 ORDER BY rowid"
    ))
    .bind(binding.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(QueueError::Db)?;
    rows.into_iter().map(OpRow::into_record).collect()
}

/// 记一次失败：`attempts + 1`，并写入最近错误码；其余字段不变。返回是否命中该操作。
pub async fn record_attempt_failure(
    tx: &mut Transaction<'_, Sqlite>,
    op: OpId,
    code: ErrorCode,
) -> Result<bool, QueueError> {
    let updated = sqlx::query(
        "UPDATE pending_ops SET attempts = attempts + 1, last_error_code = ?1 WHERE op_id = ?2",
    )
    .bind(code.as_str())
    .bind(op.as_bytes().as_slice())
    .execute(&mut **tx)
    .await
    .map_err(QueueError::Db)?;
    Ok(updated.rows_affected() > 0)
}

/// 成功后出队：删除该行。返回是否命中该操作。
///
/// 被别的操作依赖时删除会因 `ON DELETE RESTRICT` 失败——依赖方必须先完成或改依赖，不能靠
/// 级联把未完成的前置悄悄丢掉。
pub async fn complete(tx: &mut Transaction<'_, Sqlite>, op: OpId) -> Result<bool, QueueError> {
    let deleted = sqlx::query("DELETE FROM pending_ops WHERE op_id = ?1")
        .bind(op.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(QueueError::Db)?;
    Ok(deleted.rows_affected() > 0)
}

/// 数据库行到 [`PendingOp`] 的中间形态。
#[derive(sqlx::FromRow)]
struct OpRow {
    op_id: Vec<u8>,
    binding_id: Vec<u8>,
    kind: String,
    entry_id: Option<Vec<u8>>,
    baseline_revision: Option<i64>,
    baseline_version_id: Option<Vec<u8>>,
    depends_on: Option<Vec<u8>>,
    pinned_version_id: Option<Vec<u8>>,
    attempts: i64,
    last_error_code: Option<String>,
}

impl OpRow {
    fn into_record(self) -> Result<PendingOp, QueueError> {
        let attempts = u32::try_from(self.attempts)
            .map_err(|_| QueueError::MalformedRow { column: "attempts" })?;
        Ok(PendingOp {
            op: identity(self.op_id, "op_id", OpId::from_bytes)?,
            binding: identity(self.binding_id, "binding_id", BindingId::from_bytes)?,
            kind: self.kind,
            entry: self
                .entry_id
                .map(|bytes| identity(bytes, "entry_id", EntryId::from_bytes))
                .transpose()?,
            baseline_revision: self
                .baseline_revision
                .map(|value| {
                    Revision::from_db_integer(value).map_err(|_| QueueError::MalformedRow {
                        column: "baseline_revision",
                    })
                })
                .transpose()?,
            baseline_version: self
                .baseline_version_id
                .map(|bytes| identity(bytes, "baseline_version_id", VersionId::from_bytes))
                .transpose()?,
            depends_on: self
                .depends_on
                .map(|bytes| identity(bytes, "depends_on", OpId::from_bytes))
                .transpose()?,
            pinned_version: self
                .pinned_version_id
                .map(|bytes| identity(bytes, "pinned_version_id", VersionId::from_bytes))
                .transpose()?,
            attempts,
            last_error: self
                .last_error_code
                .map(|text| {
                    ErrorCode::from_code(&text).ok_or(QueueError::MalformedRow {
                        column: "last_error_code",
                    })
                })
                .transpose()?,
        })
    }
}

fn entry_bytes(entry: Option<EntryId>) -> Option<Vec<u8>> {
    entry.map(|entry| entry.as_bytes().to_vec())
}

fn op_bytes(op: Option<OpId>) -> Option<Vec<u8>> {
    op.map(|op| op.as_bytes().to_vec())
}

fn version_bytes(version: Option<VersionId>) -> Option<Vec<u8>> {
    version.map(|version| version.as_bytes().to_vec())
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, QueueError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| QueueError::MalformedRow { column })?;
    Ok(wrap(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{
        TestDir, binding_id, entry_id, op_id, open_database, seed_binding, version_id,
    };

    fn new_op(marker: u8, binding: BindingId) -> NewPendingOp {
        NewPendingOp {
            op: op_id(marker),
            binding,
            kind: "update_file".to_owned(),
            entry: Some(entry_id(0x31)),
            baseline_revision: Some(Revision::new(3).expect("revision")),
            baseline_version: Some(version_id(0x41)),
            depends_on: None,
            pinned_version: None,
        }
    }

    #[tokio::test]
    async fn enqueue_round_trips_and_keeps_insertion_order() {
        let dir = TestDir::new("enqueue");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let first = enqueue(&mut tx, &new_op(0x51, binding))
            .await
            .expect("入队第一条");
        let mut dependent = new_op(0x52, binding);
        dependent.depends_on = Some(op_id(0x51));
        dependent.pinned_version = Some(version_id(0x42));
        dependent.entry = None;
        let second = enqueue(&mut tx, &dependent).await.expect("入队第二条");
        tx.commit().await.expect("提交");

        assert_eq!(first.op, op_id(0x51));
        assert_eq!(first.binding, binding);
        assert_eq!(first.kind, "update_file");
        assert_eq!(first.entry, Some(entry_id(0x31)));
        assert_eq!(
            first.baseline_revision,
            Some(Revision::new(3).expect("revision"))
        );
        assert_eq!(first.baseline_version, Some(version_id(0x41)));
        assert_eq!(first.attempts, 0);
        assert_eq!(first.last_error, None);
        assert_eq!(second.depends_on, Some(op_id(0x51)));
        assert_eq!(second.pinned_version, Some(version_id(0x42)));
        assert_eq!(second.entry, None);

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            pending_ops_for_binding(&mut conn, binding)
                .await
                .expect("列队列"),
            vec![first, second]
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn empty_kind_duplicate_self_and_unknown_dependency_are_rejected() {
        let dir = TestDir::new("queue-reject");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x22).await;

        let mut tx = database.begin_write().await.expect("开始写事务");

        let mut empty = new_op(0x61, binding);
        empty.kind = String::new();
        assert!(matches!(
            enqueue(&mut tx, &empty)
                .await
                .expect_err("空类型必须被拒绝"),
            QueueError::EmptyKind
        ));

        let mut self_dependent = new_op(0x62, binding);
        self_dependent.depends_on = Some(op_id(0x62));
        assert!(matches!(
            enqueue(&mut tx, &self_dependent)
                .await
                .expect_err("自依赖必须被拒绝"),
            QueueError::SelfDependency { .. }
        ));

        let mut dangling = new_op(0x63, binding);
        dangling.depends_on = Some(op_id(0x64));
        assert!(matches!(
            enqueue(&mut tx, &dangling)
                .await
                .expect_err("缺失依赖必须被拒绝"),
            QueueError::UnknownDependency { .. }
        ));

        enqueue(&mut tx, &new_op(0x65, binding))
            .await
            .expect("入队");
        assert!(matches!(
            enqueue(&mut tx, &new_op(0x65, binding))
                .await
                .expect_err("重复操作身份必须被拒绝"),
            QueueError::DuplicateOp { .. }
        ));

        // 不存在的绑定由外键拒绝。
        let mut unknown_binding = new_op(0x66, binding);
        unknown_binding.binding = binding_id(0xFE);
        assert!(
            enqueue(&mut tx, &unknown_binding).await.is_err(),
            "不存在的绑定必须被外键拒绝"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            pending_ops_for_binding(&mut conn, binding)
                .await
                .expect("列队列")
                .len(),
            1,
            "被拒绝的入队不得留下行"
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn failure_attempts_accumulate_without_losing_the_intent() {
        let dir = TestDir::new("attempts");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x23).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        enqueue(&mut tx, &new_op(0x71, binding))
            .await
            .expect("入队");
        assert!(
            record_attempt_failure(&mut tx, op_id(0x71), ErrorCode::BaseMismatch)
                .await
                .expect("记失败"),
            "应命中该操作"
        );
        assert!(
            record_attempt_failure(&mut tx, op_id(0x71), ErrorCode::RateLimited)
                .await
                .expect("再记失败"),
            "应命中该操作"
        );
        assert!(
            !record_attempt_failure(&mut tx, op_id(0x72), ErrorCode::RateLimited)
                .await
                .expect("记失败"),
            "不存在的操作不应命中"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let op = pending_op(&mut conn, op_id(0x71))
            .await
            .expect("读操作")
            .expect("操作必须存在");
        assert_eq!(op.attempts, 2);
        assert_eq!(op.last_error, Some(ErrorCode::RateLimited));
        assert_eq!(op.entry, Some(entry_id(0x31)), "失败记录不得丢掉目标");
        assert_eq!(
            op.baseline_version,
            Some(version_id(0x41)),
            "失败记录不得丢掉基线"
        );
        drop(conn);

        // 成功后出队；被依赖的操作在依赖方还在时不能删除。
        let mut tx = database.begin_write().await.expect("开始写事务");
        let mut dependent = new_op(0x73, binding);
        dependent.depends_on = Some(op_id(0x71));
        enqueue(&mut tx, &dependent).await.expect("入队依赖方");
        assert!(
            complete(&mut tx, op_id(0x71)).await.is_err(),
            "仍有依赖方时不得删除前置操作"
        );
        assert!(
            complete(&mut tx, op_id(0x73)).await.expect("完成依赖方"),
            "依赖方应被删除"
        );
        assert!(
            complete(&mut tx, op_id(0x71)).await.expect("完成前置"),
            "依赖方出队后前置可删除"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert!(
            pending_ops_for_binding(&mut conn, binding)
                .await
                .expect("列队列")
                .is_empty()
        );
        assert_eq!(
            pending_op(&mut conn, op_id(0x71)).await.expect("读操作"),
            None
        );
        drop(conn);
        database.close().await;
    }
}
