//! 可恢复的本地动作意图：替换、移动、释放、导入（`docs/design.md` §7.3、§8.4）。
//!
//! 边界：本模块只保存“即将（或已经）对哪个本地路径做什么、依据哪份内容与哪个 generation”。
//! 它不执行文件系统动作、不校验哈希、不推进 generation：写入顺序由调用方遵守
//! “暂存内容 → 校验 → 持久意图 → 受控替换 → 更新 baseline”，本模块负责把意图先落库。
//!
//! 不变量：
//!
//! - **意图先于磁盘改动持久化**：`local_intents` 行在受控替换之前写入；非正常退出后按
//!   `intent_id`（插入序）重放，文件已换而数据库未更新时靠 `content_hash` 与 `source_path`
//!   辨认结果（design §8.4）。
//! - **同一路径同时只有一个意图**：`UNIQUE (binding_id, target_path)`，避免两条意图对同一路径
//!   给出互相矛盾的期望。
//! - **generation 是恢复时的判据，不是授权**：意图记录开始前的 generation；恢复时若本地
//!   generation 已经更大，说明期间有更新的本地修改，旧意图不得覆盖它（design §8.4、I02）。
//! - **失败不丢意图**：[`record_intent_failure`] 只把状态转成 `failed` 并记录原因，路径、哈希与
//!   generation 都保留，供人工处理后重放；成功完成由 [`complete_intent`] 删行。
//! - **意图身份是本地序号**：`intent_id` 不发给服务器，不作为 wire 身份（与 `pending_ops` 的
//!   `OpId` 不同），因此用整数插入序而不是 16 字节不透明身份。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, EntryId, ID_LEN};
use vaultsyncd_domain::version::Generation;

/// 本地意图的种类（`local_intents.kind`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalIntentKind {
    /// 用暂存内容替换目标文件。
    Replace,
    /// 本地移动：`source_path` 是旧位置，`target_path` 是新位置。
    Move,
    /// 释放目标条目的本地内容（保留元数据）。
    Release,
    /// 把本地新文件导入同步树。
    Import,
}

impl LocalIntentKind {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Move => "move",
            Self::Release => "release",
            Self::Import => "import",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理，不猜种类。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "replace" => Some(Self::Replace),
            "move" => Some(Self::Move),
            "release" => Some(Self::Release),
            "import" => Some(Self::Import),
            _ => None,
        }
    }
}

/// 意图状态（`local_intents.state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentState {
    /// 待执行或待恢复。
    Pending,
    /// 执行失败并停止自动推进；原因见 [`LocalIntent::last_error`]。
    Failed,
}

impl IntentState {
    /// 数据库里的规范文本。
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，由调用方按坏行处理。
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// 新建一条本地意图的输入。
///
/// 不变量（违反时 [`begin_intent`] 报错且不写入）：`target_path` 非空、无 NUL；`source_path` 给了
/// 就必须非空且无 NUL；`content_hash` 给了就必须非空；同一绑定下 `target_path` 未被占用。
#[derive(Clone, Debug)]
pub struct NewLocalIntent {
    /// 所属绑定。
    pub binding: BindingId,
    /// 动作种类。
    pub kind: LocalIntentKind,
    /// 目标条目；本地新文件在提交前还没有服务器身份。
    pub entry: Option<EntryId>,
    /// 动作作用的位置（相对绑定根）。
    pub target_path: String,
    /// 来源位置：替换的暂存文件、移动的旧路径、导入的来源文件。
    pub source_path: Option<String>,
    /// 期望内容哈希（SHA-256 原始字节）。
    pub content_hash: Option<Vec<u8>>,
    /// 意图开始前观察到的本地 generation。
    pub generation: Option<Generation>,
}

/// 一条本地意图（`local_intents` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalIntent {
    /// 本地意图序号（插入序，恢复时按它升序重放）。
    pub id: i64,
    /// 所属绑定。
    pub binding: BindingId,
    /// 动作种类。
    pub kind: LocalIntentKind,
    /// 目标条目。
    pub entry: Option<EntryId>,
    /// 动作作用的位置。
    pub target_path: String,
    /// 来源位置。
    pub source_path: Option<String>,
    /// 期望内容哈希。
    pub content_hash: Option<Vec<u8>>,
    /// 意图开始前观察到的 generation。
    pub generation: Option<Generation>,
    /// 当前状态。
    pub state: IntentState,
    /// 失败原因；只有 [`IntentState::Failed`] 有。
    pub last_error: Option<ErrorCode>,
}

/// 本地意图读写的失败。
#[derive(Debug)]
pub enum IntentError {
    /// SQLite 层面失败（含外键与 `CHECK` 拒绝）。
    Db(sqlx::Error),
    /// 路径不合格（空或含 NUL）。
    InvalidPath {
        /// 出问题的字段名（`target_path`/`source_path`）。
        field: &'static str,
        /// 违反的具体规则。
        reason: &'static str,
    },
    /// 给了空字节的内容哈希；空字节不是“未知”，未知用 `None`。
    EmptyBytes {
        /// 出问题的字段名。
        field: &'static str,
    },
    /// 同一绑定的同一路径已有意图。
    DuplicateTarget {
        /// 冲突的目标路径。
        path: String,
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
            Self::InvalidPath { field, reason } => write!(f, "路径 {field} 不合法：{reason}"),
            Self::EmptyBytes { field } => write!(f, "字段 {field} 是空字节"),
            Self::DuplicateTarget { path } => write!(f, "路径 {path} 已有未清理的意图"),
            Self::MalformedRow { column } => {
                write!(f, "数据库列 {column} 的值不符合类型前提")
            }
        }
    }
}

impl std::error::Error for IntentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::InvalidPath { .. }
            | Self::EmptyBytes { .. }
            | Self::DuplicateTarget { .. }
            | Self::MalformedRow { .. } => None,
        }
    }
}

/// `local_intents` 的列顺序。
const INTENT_COLUMNS: &str = "intent_id, binding_id, kind, entry_id, target_path, source_path, \
                              content_hash, generation, state, last_error_code";

/// 写入一条本地意图并返回读回记录（含分配的 `intent_id`）。
pub async fn begin_intent(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewLocalIntent,
) -> Result<LocalIntent, IntentError> {
    validate_path("target_path", &new.target_path)?;
    if let Some(source) = new.source_path.as_deref() {
        validate_path("source_path", source)?;
    }
    if new.content_hash.as_ref().is_some_and(Vec::is_empty) {
        return Err(IntentError::EmptyBytes {
            field: "content_hash",
        });
    }

    let inserted = sqlx::query(
        "INSERT INTO local_intents \
         (binding_id, kind, entry_id, target_path, source_path, content_hash, generation) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(new.binding.as_bytes().as_slice())
    .bind(new.kind.as_text())
    .bind(entry_bytes(new.entry))
    .bind(&new.target_path)
    .bind(new.source_path.as_deref())
    .bind(new.content_hash.as_deref())
    .bind(new.generation.map(Generation::to_db_integer))
    .execute(&mut **tx)
    .await;

    let result = match inserted {
        Ok(result) => result,
        Err(error) => {
            if error
                .as_database_error()
                .map(|database_error| database_error.is_unique_violation())
                .unwrap_or(false)
            {
                return Err(IntentError::DuplicateTarget {
                    path: new.target_path.clone(),
                });
            }
            return Err(IntentError::Db(error));
        }
    };

    intent(tx, result.last_insert_rowid())
        .await?
        .ok_or(IntentError::MalformedRow {
            column: "intent_id",
        })
}

/// 读一条意图；不存在返回 `None`。
pub async fn intent(
    conn: &mut SqliteConnection,
    id: i64,
) -> Result<Option<LocalIntent>, IntentError> {
    let row = sqlx::query_as::<_, IntentRow>(&format!(
        "SELECT {INTENT_COLUMNS} FROM local_intents WHERE intent_id = ?1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(IntentError::Db)?;
    row.map(IntentRow::into_record).transpose()
}

/// 列出某个绑定的全部意图，按写入序（恢复顺序）。
pub async fn intents_for_binding(
    conn: &mut SqliteConnection,
    binding: BindingId,
) -> Result<Vec<LocalIntent>, IntentError> {
    let rows = sqlx::query_as::<_, IntentRow>(&format!(
        "SELECT {INTENT_COLUMNS} FROM local_intents WHERE binding_id = ?1 ORDER BY intent_id"
    ))
    .bind(binding.as_bytes().as_slice())
    .fetch_all(conn)
    .await
    .map_err(IntentError::Db)?;
    rows.into_iter().map(IntentRow::into_record).collect()
}

/// 记一次失败：状态转 `failed` 并写入原因，路径、哈希与 generation 不变。返回是否命中。
pub async fn record_intent_failure(
    tx: &mut Transaction<'_, Sqlite>,
    id: i64,
    code: ErrorCode,
) -> Result<bool, IntentError> {
    let updated = sqlx::query(
        "UPDATE local_intents SET state = ?1, last_error_code = ?2 WHERE intent_id = ?3",
    )
    .bind(IntentState::Failed.as_text())
    .bind(code.as_str())
    .bind(id)
    .execute(&mut **tx)
    .await
    .map_err(IntentError::Db)?;
    Ok(updated.rows_affected() > 0)
}

/// 意图已完成（磁盘动作与数据库更新都做完）：删除该行。返回是否命中。
pub async fn complete_intent(
    tx: &mut Transaction<'_, Sqlite>,
    id: i64,
) -> Result<bool, IntentError> {
    let deleted = sqlx::query("DELETE FROM local_intents WHERE intent_id = ?1")
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(IntentError::Db)?;
    Ok(deleted.rows_affected() > 0)
}

fn validate_path(field: &'static str, path: &str) -> Result<(), IntentError> {
    if path.is_empty() {
        return Err(IntentError::InvalidPath {
            field,
            reason: "不能为空",
        });
    }
    if path.contains('\0') {
        return Err(IntentError::InvalidPath {
            field,
            reason: "不能包含 NUL",
        });
    }
    Ok(())
}

fn entry_bytes(entry: Option<EntryId>) -> Option<Vec<u8>> {
    entry.map(|entry| entry.as_bytes().to_vec())
}

/// 数据库行到 [`LocalIntent`] 的中间形态。
#[derive(sqlx::FromRow)]
struct IntentRow {
    intent_id: i64,
    binding_id: Vec<u8>,
    kind: String,
    entry_id: Option<Vec<u8>>,
    target_path: String,
    source_path: Option<String>,
    content_hash: Option<Vec<u8>>,
    generation: Option<i64>,
    state: String,
    last_error_code: Option<String>,
}

impl IntentRow {
    fn into_record(self) -> Result<LocalIntent, IntentError> {
        Ok(LocalIntent {
            id: self.intent_id,
            binding: identity(self.binding_id, "binding_id", BindingId::from_bytes)?,
            kind: LocalIntentKind::from_text(&self.kind)
                .ok_or(IntentError::MalformedRow { column: "kind" })?,
            entry: self
                .entry_id
                .map(|bytes| identity(bytes, "entry_id", EntryId::from_bytes))
                .transpose()?,
            target_path: self.target_path,
            source_path: self.source_path,
            content_hash: self.content_hash,
            generation: self
                .generation
                .map(|value| {
                    Generation::from_db_integer(value).map_err(|_| IntentError::MalformedRow {
                        column: "generation",
                    })
                })
                .transpose()?,
            state: IntentState::from_text(&self.state)
                .ok_or(IntentError::MalformedRow { column: "state" })?,
            last_error: self
                .last_error_code
                .map(|text| {
                    ErrorCode::from_code(&text).ok_or(IntentError::MalformedRow {
                        column: "last_error_code",
                    })
                })
                .transpose()?,
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{TestDir, binding_id, entry_id, open_database, seed_binding};

    fn generation(value: u64) -> Generation {
        Generation::new(value).expect("generation")
    }

    fn replace_intent(binding: BindingId) -> NewLocalIntent {
        NewLocalIntent {
            binding,
            kind: LocalIntentKind::Replace,
            entry: Some(entry_id(0x11)),
            target_path: "dir/a.txt".to_owned(),
            source_path: Some("cache/staging-1.part".to_owned()),
            content_hash: Some(vec![0xAB; 32]),
            generation: Some(generation(7)),
        }
    }

    #[tokio::test]
    async fn intents_round_trip_in_creation_order_and_survive_restart() {
        let dir = TestDir::new("intents");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x21).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let first = begin_intent(&mut tx, &replace_intent(binding))
            .await
            .expect("写替换意图");
        let mut moved = replace_intent(binding);
        moved.kind = LocalIntentKind::Move;
        moved.target_path = "dir/b.txt".to_owned();
        moved.source_path = Some("dir/a.txt".to_owned());
        moved.content_hash = None;
        let second = begin_intent(&mut tx, &moved).await.expect("写移动意图");
        let mut imported = replace_intent(binding);
        imported.kind = LocalIntentKind::Import;
        imported.entry = None;
        imported.target_path = "dir/new.txt".to_owned();
        imported.source_path = Some("inbox/new.txt".to_owned());
        imported.generation = None;
        let third = begin_intent(&mut tx, &imported).await.expect("写导入意图");
        tx.commit().await.expect("提交");

        assert!(first.id < second.id && second.id < third.id, "按插入序编号");
        assert_eq!(first.state, IntentState::Pending);
        assert_eq!(first.entry, Some(entry_id(0x11)));
        assert_eq!(first.content_hash, Some(vec![0xAB; 32]));
        assert_eq!(first.generation, Some(generation(7)));
        assert_eq!(second.kind, LocalIntentKind::Move);
        assert_eq!(second.source_path.as_deref(), Some("dir/a.txt"));
        assert_eq!(third.kind, LocalIntentKind::Import);
        assert_eq!(third.entry, None);
        database.close().await;

        // 重启：意图按写入序保留，恢复时从这里接着做。
        let reopened = open_database(&dir).await;
        let mut conn = reopened.pool().acquire().await.expect("取连接");
        assert_eq!(
            intents_for_binding(&mut conn, binding)
                .await
                .expect("列意图"),
            vec![first, second, third]
        );
        drop(conn);
        reopened.close().await;
    }

    #[tokio::test]
    async fn duplicate_target_and_invalid_fields_are_rejected() {
        let dir = TestDir::new("intents-reject");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x22).await;
        let other = seed_binding(&database, 0x23).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        begin_intent(&mut tx, &replace_intent(binding))
            .await
            .expect("写第一条");

        assert!(matches!(
            begin_intent(&mut tx, &replace_intent(binding))
                .await
                .expect_err("同一路径不能有两个意图"),
            IntentError::DuplicateTarget { .. }
        ));
        // 不同绑定可以有相同相对路径。
        begin_intent(&mut tx, &replace_intent(other))
            .await
            .expect("另一绑定写同一路径");

        let mut empty_target = replace_intent(binding);
        empty_target.target_path = String::new();
        assert!(matches!(
            begin_intent(&mut tx, &empty_target)
                .await
                .expect_err("目标路径不能为空"),
            IntentError::InvalidPath {
                field: "target_path",
                ..
            }
        ));

        let mut nul_source = replace_intent(binding);
        nul_source.target_path = "dir/c.txt".to_owned();
        nul_source.source_path = Some("cache/a\0b".to_owned());
        assert!(matches!(
            begin_intent(&mut tx, &nul_source)
                .await
                .expect_err("来源路径不能含 NUL"),
            IntentError::InvalidPath {
                field: "source_path",
                ..
            }
        ));

        let mut empty_hash = replace_intent(binding);
        empty_hash.target_path = "dir/d.txt".to_owned();
        empty_hash.content_hash = Some(Vec::new());
        assert!(matches!(
            begin_intent(&mut tx, &empty_hash)
                .await
                .expect_err("空哈希不是未知"),
            IntentError::EmptyBytes {
                field: "content_hash"
            }
        ));

        let mut unknown_binding = replace_intent(binding_id(0xFB));
        unknown_binding.target_path = "dir/e.txt".to_owned();
        assert!(
            begin_intent(&mut tx, &unknown_binding).await.is_err(),
            "不存在的绑定必须被外键拒绝"
        );
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        assert_eq!(
            intents_for_binding(&mut conn, binding)
                .await
                .expect("列意图")
                .len(),
            1,
            "被拒绝的意图不得留下行"
        );
        drop(conn);
        database.close().await;
    }

    #[tokio::test]
    async fn failure_keeps_the_intent_and_completion_removes_it() {
        let dir = TestDir::new("intents-state");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x24).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        let written = begin_intent(&mut tx, &replace_intent(binding))
            .await
            .expect("写意图");
        assert!(
            record_intent_failure(&mut tx, written.id, ErrorCode::InsufficientStorage)
                .await
                .expect("记失败"),
            "应命中该意图"
        );
        assert!(
            !record_intent_failure(&mut tx, written.id + 1, ErrorCode::InsufficientStorage)
                .await
                .expect("记失败"),
            "不存在的意图不应命中"
        );
        let failed = intent(&mut tx, written.id)
            .await
            .expect("读意图")
            .expect("意图必须存在");
        assert_eq!(failed.state, IntentState::Failed);
        assert_eq!(failed.last_error, Some(ErrorCode::InsufficientStorage));
        assert_eq!(failed.source_path, Some("cache/staging-1.part".to_owned()));
        assert_eq!(failed.content_hash, Some(vec![0xAB; 32]));
        assert_eq!(failed.generation, Some(generation(7)));

        assert!(
            complete_intent(&mut tx, written.id).await.expect("完成"),
            "完成删除该行"
        );
        assert!(
            !complete_intent(&mut tx, written.id)
                .await
                .expect("再次完成"),
            "已删除的意图不再命中"
        );
        assert_eq!(intent(&mut tx, written.id).await.expect("读意图"), None);
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[tokio::test]
    async fn unknown_error_codes_are_rejected_as_malformed_rows() {
        let dir = TestDir::new("intents-malformed");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x25).await;

        // 结构合法但错误码文本不属于 design §15.4：读取时按坏行拒绝，而不是当成可重试错误。
        let mut tx = database.begin_write().await.expect("开始写事务");
        sqlx::query(
            "INSERT INTO local_intents \
             (binding_id, kind, target_path, state, last_error_code) \
             VALUES (?1, 'replace', 'dir/a.txt', 'failed', 'NOT_A_CODE')",
        )
        .bind(binding.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .expect("写坏行");
        tx.commit().await.expect("提交");

        let mut conn = database.pool().acquire().await.expect("取连接");
        let error = intents_for_binding(&mut conn, binding)
            .await
            .expect_err("坏行必须被拒绝");
        assert!(
            matches!(
                error,
                IntentError::MalformedRow {
                    column: "last_error_code"
                }
            ),
            "实际错误 {error:?}"
        );
        drop(conn);
        database.close().await;
    }
}
