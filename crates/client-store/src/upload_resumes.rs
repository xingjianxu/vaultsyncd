//! 上传续传会话的持久化（`docs/design.md` §9.3、§13.2；表属 S055）。
//!
//! 边界：本模块只保存“哪条排队操作固定了哪份快照、继续复用哪个服务端会话”。它不上传块、不计算
//! 摘要、不判断快照是否变化：这些是 `crates/transfer::upload` 的规则，本模块只把结果落库。
//!
//! 不变量：
//!
//! - **已收块以服务端为准**（design §9.3）：这里**不**存块位图。续传时由 `UploadTarget::status`
//!   读回服务端真实已收块，避免客户端位图与服务端 `upload_parts` 出现两份矛盾事实。
//! - **一条操作最多一个会话**：`op_id` 既是主键又外键到 `pending_ops`。操作成功出队或行被删除时，
//!   续传记录随 `ON DELETE CASCADE` 一起消失，不会留下指向已不存在操作的孤儿会话。
//! - **会话绑在快照上**：`snapshot_identity`/`total_size`/`digest` 一起标识快照；任一变化都必须在
//!   上层新开会话（S054 `SnapshotChanged`），不能拿旧会话拼一份新内容。
//! - **摘要只有一种编码**：`digest` 是 SHA-256 的 32 个原始字节，不是十六进制文本。

use sqlx::{Sqlite, SqliteConnection, Transaction};
use vaultsyncd_domain::ids::{BindingId, ID_LEN, OpId};
use vaultsyncd_domain::version::DB_INTEGER_MAX;

/// 快照摘要的字节数（SHA-256）。
pub const DIGEST_BYTES: usize = 32;

/// 写入一条上传续传记录的输入。
///
/// 不变量（违反时 [`save`] 报错且不写入）：快照身份非空且不含 NUL；会话身份非空且不含 NUL；
/// `total_size` 不超过 SQLite `INTEGER` 上限；`op_id` 必须已在 `pending_ops` 里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewUploadResume {
    /// 所属排队操作。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 固定快照的本地身份（本地代际或等价的快照标识）。
    pub snapshot_identity: String,
    /// 快照总字节数。
    pub total_size: u64,
    /// 整份内容的 SHA-256。
    pub digest: [u8; DIGEST_BYTES],
    /// 服务端上传会话身份。
    pub session: String,
}

/// 一条上传续传记录（`upload_resumes` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadResume {
    /// 所属排队操作。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 固定快照的本地身份。
    pub snapshot_identity: String,
    /// 快照总字节数。
    pub total_size: u64,
    /// 整份内容的 SHA-256。
    pub digest: [u8; DIGEST_BYTES],
    /// 服务端上传会话身份。
    pub session: String,
}

impl UploadResume {
    /// 会话是否仍可用于这份快照：身份、大小与摘要都必须相同。
    ///
    /// 判断在 Rust 里做（不是 SQL），因为“同一份快照”的语义由 S054 的 `UploadSnapshot` 定义。
    #[must_use]
    pub fn matches_snapshot(
        &self,
        identity: &str,
        total_size: u64,
        digest: &[u8; DIGEST_BYTES],
    ) -> bool {
        self.snapshot_identity == identity
            && self.total_size == total_size
            && self.digest == *digest
    }
}

/// 上传续传读写的失败。
#[derive(Debug)]
pub enum UploadResumeError {
    /// SQLite 层面失败。
    Db(sqlx::Error),
    /// 快照身份不合格。
    InvalidSnapshotIdentity {
        /// 违反的具体规则。
        reason: &'static str,
    },
    /// 会话身份不合格。
    InvalidSession {
        /// 违反的具体规则。
        reason: &'static str,
    },
    /// 总长度超过 SQLite `INTEGER` 上限。
    SizeAboveDbLimit,
    /// 引用的排队操作不存在（外键拒绝）。
    UnknownOperation {
        /// 缺失的操作。
        op: OpId,
    },
    /// 数据库行不满足本模块的类型前提。
    MalformedRow {
        /// 出问题的列名。
        column: &'static str,
    },
}

impl std::fmt::Display for UploadResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(source) => write!(f, "SQLite 操作失败：{source}"),
            Self::InvalidSnapshotIdentity { reason } => {
                write!(f, "快照身份不合法：{reason}")
            }
            Self::InvalidSession { reason } => write!(f, "上传会话身份不合法：{reason}"),
            Self::SizeAboveDbLimit => {
                write!(f, "快照长度超过数据库有符号 64 位上限 {DB_INTEGER_MAX}")
            }
            Self::UnknownOperation { op } => write!(f, "排队操作 {op} 不存在"),
            Self::MalformedRow { column } => {
                write!(f, "数据库列 {column} 的值不符合类型前提")
            }
        }
    }
}

impl std::error::Error for UploadResumeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(source) => Some(source),
            Self::InvalidSnapshotIdentity { .. }
            | Self::InvalidSession { .. }
            | Self::SizeAboveDbLimit
            | Self::UnknownOperation { .. }
            | Self::MalformedRow { .. } => None,
        }
    }
}

/// `upload_resumes` 的列顺序。
const RESUME_COLUMNS: &str = "op_id, binding_id, snapshot_identity, total_size, digest, session_id";

/// 写入或覆盖一条续传记录（同一操作重试时更新快照/会话），返回重新读出的记录。
///
/// 覆盖是安全的：会话与快照是一起写的，不存在“旧会话配新快照”的中间状态。
pub async fn save(
    tx: &mut Transaction<'_, Sqlite>,
    new: &NewUploadResume,
) -> Result<UploadResume, UploadResumeError> {
    validate_label("snapshot_identity", &new.snapshot_identity)?;
    validate_label("session_id", &new.session)?;
    if new.total_size > DB_INTEGER_MAX {
        return Err(UploadResumeError::SizeAboveDbLimit);
    }

    let written = sqlx::query(
        "INSERT INTO upload_resumes \
         (op_id, binding_id, snapshot_identity, total_size, digest, session_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (op_id) DO UPDATE SET \
             binding_id = excluded.binding_id, \
             snapshot_identity = excluded.snapshot_identity, \
             total_size = excluded.total_size, \
             digest = excluded.digest, \
             session_id = excluded.session_id",
    )
    .bind(new.op.as_bytes().as_slice())
    .bind(new.binding.as_bytes().as_slice())
    .bind(&new.snapshot_identity)
    .bind(new.total_size as i64)
    .bind(new.digest.as_slice())
    .bind(&new.session)
    .execute(&mut **tx)
    .await;

    if let Err(error) = written {
        if error
            .as_database_error()
            .map(sqlx::error::DatabaseError::is_foreign_key_violation)
            .unwrap_or(false)
        {
            return Err(UploadResumeError::UnknownOperation { op: new.op });
        }
        return Err(UploadResumeError::Db(error));
    }

    load(tx, new.op)
        .await?
        .ok_or(UploadResumeError::UnknownOperation { op: new.op })
}

/// 读一条续传记录；没有返回 `None`。
pub async fn load(
    conn: &mut SqliteConnection,
    op: OpId,
) -> Result<Option<UploadResume>, UploadResumeError> {
    let row = sqlx::query_as::<_, ResumeRow>(&format!(
        "SELECT {RESUME_COLUMNS} FROM upload_resumes WHERE op_id = ?1"
    ))
    .bind(op.as_bytes().as_slice())
    .fetch_optional(conn)
    .await
    .map_err(UploadResumeError::Db)?;
    row.map(ResumeRow::into_record).transpose()
}

/// 删除一条续传记录（放弃会话，下次从 `begin` 重新申请）；返回是否命中。
pub async fn discard(
    tx: &mut Transaction<'_, Sqlite>,
    op: OpId,
) -> Result<bool, UploadResumeError> {
    let deleted = sqlx::query("DELETE FROM upload_resumes WHERE op_id = ?1")
        .bind(op.as_bytes().as_slice())
        .execute(&mut **tx)
        .await
        .map_err(UploadResumeError::Db)?;
    Ok(deleted.rows_affected() > 0)
}

fn validate_label(field: &str, value: &str) -> Result<(), UploadResumeError> {
    let failure = |reason| match field {
        "snapshot_identity" => UploadResumeError::InvalidSnapshotIdentity { reason },
        _ => UploadResumeError::InvalidSession { reason },
    };
    if value.is_empty() {
        return Err(failure("不能为空"));
    }
    if value.contains('\0') {
        return Err(failure("不能包含 NUL"));
    }
    Ok(())
}

/// 数据库行到 [`UploadResume`] 的中间形态。
#[derive(sqlx::FromRow)]
struct ResumeRow {
    op_id: Vec<u8>,
    binding_id: Vec<u8>,
    snapshot_identity: String,
    total_size: i64,
    digest: Vec<u8>,
    session_id: String,
}

impl ResumeRow {
    fn into_record(self) -> Result<UploadResume, UploadResumeError> {
        let op = identity(self.op_id, "op_id", OpId::from_bytes)?;
        let binding = identity(self.binding_id, "binding_id", BindingId::from_bytes)?;
        let total_size =
            u64::try_from(self.total_size).map_err(|_| UploadResumeError::MalformedRow {
                column: "total_size",
            })?;
        let digest: [u8; DIGEST_BYTES] = self
            .digest
            .as_slice()
            .try_into()
            .map_err(|_| UploadResumeError::MalformedRow { column: "digest" })?;
        validate_label("snapshot_identity", &self.snapshot_identity).map_err(|_| {
            UploadResumeError::MalformedRow {
                column: "snapshot_identity",
            }
        })?;
        validate_label("session_id", &self.session_id).map_err(|_| {
            UploadResumeError::MalformedRow {
                column: "session_id",
            }
        })?;
        Ok(UploadResume {
            op,
            binding,
            snapshot_identity: self.snapshot_identity,
            total_size,
            digest,
            session: self.session_id,
        })
    }
}

/// 身份列长度检查：BLOB 列可能被外部写入短值，读到就明确报坏行，不截断、不补零。
fn identity<T>(
    bytes: Vec<u8>,
    column: &'static str,
    wrap: fn([u8; ID_LEN]) -> T,
) -> Result<T, UploadResumeError> {
    let array: [u8; ID_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| UploadResumeError::MalformedRow { column })?;
    Ok(wrap(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{
        TestDir, binding_id, enqueue_op, op_id, open_database, seed_binding,
    };

    fn new_resume(binding: BindingId, marker: u8, session: &str) -> NewUploadResume {
        NewUploadResume {
            op: op_id(marker),
            binding,
            snapshot_identity: format!("generation-{marker:02x}"),
            total_size: 10,
            digest: [marker; DIGEST_BYTES],
            session: session.to_owned(),
        }
    }

    #[tokio::test]
    async fn resume_round_trips_overwrites_and_discards() {
        let dir = TestDir::new("upload-resume");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x31).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        enqueue_op(&mut tx, binding, op_id(0x71)).await;
        let saved = save(&mut tx, &new_resume(binding, 0x71, "aa"))
            .await
            .expect("写入续传");
        assert_eq!(saved.op, op_id(0x71));
        assert!(saved.matches_snapshot("generation-71", 10, &[0x71; DIGEST_BYTES]));

        // 同一操作重试：快照与会话一起被覆盖，不会留下旧会话配新快照。
        let updated = save(&mut tx, &new_resume(binding, 0x71, "bb"))
            .await
            .expect("覆盖续传");
        assert_eq!(updated.session, "bb");
        assert_eq!(
            load(&mut tx, op_id(0x71)).await.expect("读续传"),
            Some(updated.clone())
        );
        assert!(!updated.matches_snapshot("generation-70", 10, &[0x71; DIGEST_BYTES]));
        assert!(!updated.matches_snapshot("generation-71", 11, &[0x71; DIGEST_BYTES]));
        assert!(!updated.matches_snapshot("generation-71", 10, &[0x70; DIGEST_BYTES]));
        tx.commit().await.expect("提交");
        database.close().await;

        // 重启后仍在。
        let reopened = open_database(&dir).await;
        let mut conn = reopened.pool().acquire().await.expect("取连接");
        assert_eq!(
            load(&mut conn, op_id(0x71)).await.expect("读续传"),
            Some(updated)
        );
        drop(conn);

        let mut tx = reopened.begin_write().await.expect("开始写事务");
        assert!(
            discard(&mut tx, op_id(0x71)).await.expect("丢弃续传"),
            "存在的记录可以被丢弃"
        );
        assert!(!discard(&mut tx, op_id(0x71)).await.expect("再次丢弃"));
        assert_eq!(load(&mut tx, op_id(0x71)).await.expect("读续传"), None);
        tx.commit().await.expect("提交");
        reopened.close().await;
    }

    #[tokio::test]
    async fn a_committed_operation_takes_its_resume_with_it() {
        let dir = TestDir::new("upload-resume-cascade");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x32).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        enqueue_op(&mut tx, binding, op_id(0x72)).await;
        save(&mut tx, &new_resume(binding, 0x72, "cc"))
            .await
            .expect("写入续传");
        // 操作成功出队 → 续传记录级联删除，不会留下孤儿会话。
        assert!(
            crate::queue::complete(&mut tx, op_id(0x72))
                .await
                .expect("出队")
        );
        assert_eq!(load(&mut tx, op_id(0x72)).await.expect("读续传"), None);
        tx.commit().await.expect("提交");
        database.close().await;
    }

    #[tokio::test]
    async fn invalid_fields_and_unknown_operations_are_rejected_before_write() {
        let dir = TestDir::new("upload-resume-invalid");
        let database = open_database(&dir).await;
        let binding = seed_binding(&database, 0x33).await;

        let mut tx = database.begin_write().await.expect("开始写事务");
        enqueue_op(&mut tx, binding, op_id(0x73)).await;

        let mut empty_identity = new_resume(binding, 0x73, "dd");
        empty_identity.snapshot_identity = String::new();
        assert!(matches!(
            save(&mut tx, &empty_identity).await,
            Err(UploadResumeError::InvalidSnapshotIdentity { .. })
        ));

        let mut nul_session = new_resume(binding, 0x73, "d\0d");
        nul_session.session = "d\0d".to_owned();
        assert!(matches!(
            save(&mut tx, &nul_session).await,
            Err(UploadResumeError::InvalidSession { .. })
        ));

        let mut too_big = new_resume(binding, 0x73, "dd");
        too_big.total_size = u64::MAX;
        assert!(matches!(
            save(&mut tx, &too_big).await,
            Err(UploadResumeError::SizeAboveDbLimit)
        ));

        // 操作不在队列里：外键拒绝，不写入孤儿会话。
        let orphan = new_resume(binding, 0x74, "ee");
        assert!(matches!(
            save(&mut tx, &orphan).await,
            Err(UploadResumeError::UnknownOperation { op }) if op == op_id(0x74)
        ));
        assert_eq!(load(&mut tx, op_id(0x74)).await.expect("读续传"), None);
        tx.rollback().await.expect("回滚");
        database.close().await;

        // 一个从未写过的绑定之外的身份也一样被拒绝（绑定的外键）。
        let reopened = open_database(&dir).await;
        let mut tx = reopened.begin_write().await.expect("开始写事务");
        let missing_binding = binding_id(0xFC);
        enqueue_op(&mut tx, binding, op_id(0x75)).await;
        assert!(matches!(
            save(&mut tx, &new_resume(missing_binding, 0x75, "ff")).await,
            Err(UploadResumeError::UnknownOperation { .. })
        ));
        tx.rollback().await.expect("回滚");
        reopened.close().await;
    }
}
