//! `crates/client-store` 上的状态端口实现（design §7.3、§8.4、§9.3）。
//!
//! 四个实现共用一个 [`ClientStorePorts`]：同一个客户端库（`pending_ops`、`local_intents`、
//! `upload_resumes`、`transfers`）在应用入口只打开一次，执行器与传输执行器共享它。所有写操作都走
//! `BEGIN IMMEDIATE` 短事务，与 `crates/client-store` 的既有约定一致。
//!
//! # 映射规则
//!
//! - **队列**：`pending_ops` 的插入顺序就是调度顺序；失败只累加 `attempts` 与 `last_error_code`，
//!   成功才删除。
//! - **意图**：同一绑定的同一路径最多一条意图（数据库 `UNIQUE`）。重复尝试必须复用同一条意图；
//!   字段不同说明是另一份期望，明确拒绝而不是覆盖。
//! - **上传续传**：按 `OpId` 存快照与会话；操作出队时由外键级联删除。
//! - **下载进度**：`transfers` 以 `(binding, version)` 为主键；终态（cancelled/failed）不再返回
//!   可用进度，因为该行已不接受进度更新——调用方会重新开始一条传输，旧行由 `discard` 清理。
//!   下载进度里的 `entry` 不单独存储：同一 `VersionId` 只属于一个条目（design I12），版本身份已经
//!   足够区分不同内容。

use std::sync::Arc;

use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::intents::{self, IntentState, LocalIntentKind, NewLocalIntent};
use vaultsyncd_client_store::queue::{self, PendingOp};
use vaultsyncd_client_store::transfers::{self, NewTransfer, TransferState};
use vaultsyncd_client_store::upload_resumes::{self, NewUploadResume};
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, OpId, VersionId};
use vaultsyncd_transfer::bitmap::PartBitmap;
use vaultsyncd_transfer::chunk::ChunkGeometry;
use vaultsyncd_transfer::ports::{
    DownloadKey, DownloadProgressStore, LocalErrorKind, PortError, PortFuture, StoredProgress,
};

use crate::ports::state::{
    ActionIntent, IntentStore, LocalWriteKind, OperationStore, StoredIntent, StoredUploadResume,
    UploadResumeStore,
};

/// 基于 `crates/client-store` 的状态端口集合。
///
/// 用 `Arc<Database>` 共享同一个连接池：执行器与传输执行器可能并发使用，但写事务仍然是短事务，
/// 不会在持锁时等待网络。
#[derive(Clone)]
pub struct ClientStorePorts {
    database: Arc<Database>,
}

impl ClientStorePorts {
    /// 用已打开的客户端库构造。
    #[must_use]
    pub fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    /// 底层数据库（应用入口与测试用）。
    #[must_use]
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    fn write_error(context: &str, detail: impl std::fmt::Display) -> PortError {
        PortError::local(LocalErrorKind::Write, format!("{context}：{detail}"))
    }

    fn read_error(context: &str, detail: impl std::fmt::Display) -> PortError {
        PortError::local(LocalErrorKind::Read, format!("{context}：{detail}"))
    }
}

impl OperationStore for ClientStorePorts {
    fn pending_ops(&self, binding: BindingId) -> PortFuture<'_, Result<Vec<PendingOp>, PortError>> {
        Box::pin(async move {
            crate::recovery::resync::ensure_unblocked(&self.database, binding).await?;
            let mut conn = self
                .database
                .pool()
                .acquire()
                .await
                .map_err(|error| Self::read_error("取数据库连接", error))?;
            let rows = queue::pending_ops_for_binding(&mut conn, binding)
                .await
                .map_err(|error| Self::read_error("读待执行队列", error))?;
            // 依赖隔离操作的新任务也不能把“调度视图里缺失”解释成“依赖已完成”。
            let blocked: Vec<Vec<u8>> = sqlx::query_scalar(
                "WITH RECURSIVE blocked(op_id) AS (SELECT op_id FROM resync_quarantined_ops UNION SELECT p.op_id FROM pending_ops p JOIN blocked b ON p.depends_on = b.op_id) SELECT op_id FROM blocked")
                .fetch_all(&mut *conn).await.map_err(|error| Self::read_error("读重建隔离队列", error))?;
            Ok(rows
                .into_iter()
                .filter(|row| !blocked.iter().any(|id| id.as_slice() == row.op.as_bytes()))
                .collect())
        })
    }

    fn record_failure(&self, op: OpId, code: ErrorCode) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let hit = queue::record_attempt_failure(&mut tx, op, code)
                .await
                .map_err(|error| Self::write_error("记录操作失败", error))?;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交操作失败", error))?;
            if !hit {
                return Err(PortError::local(
                    LocalErrorKind::Missing,
                    format!("排队操作 {op} 不存在"),
                ));
            }
            Ok(())
        })
    }

    fn complete(&self, op: OpId) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let mut conn = self
                .database
                .pool()
                .acquire()
                .await
                .map_err(|error| Self::read_error("读取操作绑定", error))?;
            if let Some(row) = queue::pending_op(&mut conn, op)
                .await
                .map_err(|error| Self::read_error("读取操作绑定", error))?
            {
                crate::recovery::resync::ensure_operation(&self.database, row.binding, op).await?;
            }
            drop(conn);
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let hit = queue::complete(&mut tx, op)
                .await
                .map_err(|error| Self::write_error("操作出队", error))?;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交操作出队", error))?;
            if !hit {
                return Err(PortError::local(
                    LocalErrorKind::Missing,
                    format!("排队操作 {op} 不存在"),
                ));
            }
            Ok(())
        })
    }
}

impl IntentStore for ClientStorePorts {
    fn begin_intent<'a>(
        &'a self,
        intent: &'a ActionIntent,
    ) -> PortFuture<'a, Result<i64, PortError>> {
        Box::pin(async move {
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let existing = intents::intents_for_binding(&mut tx, intent.binding)
                .await
                .map_err(|error| Self::read_error("读本地意图", error))?
                .into_iter()
                .find(|row| row.target_path == intent.target_path.to_string());
            if let Some(existing) = existing {
                let same = existing.kind == db_kind(intent.kind)
                    && existing.entry == intent.entry
                    && existing.source_path == intent.source_path
                    && existing.content_hash == intent.content_hash.map(|hash| hash.to_vec())
                    && existing.generation == Some(intent.generation);
                if !same {
                    return Err(PortError::rejected(
                        "INVALID_REQUEST",
                        format!("路径 {} 已有另一条本地意图，拒绝覆盖", intent.target_path),
                    ));
                }
                tx.commit()
                    .await
                    .map_err(|error| Self::write_error("提交意图复用", error))?;
                return Ok(existing.id);
            }
            let id = intents::begin_intent(
                &mut tx,
                &NewLocalIntent {
                    binding: intent.binding,
                    kind: db_kind(intent.kind),
                    entry: intent.entry,
                    target_path: intent.target_path.to_string(),
                    source_path: intent.source_path.clone(),
                    content_hash: intent.content_hash.map(|hash| hash.to_vec()),
                    generation: Some(intent.generation),
                },
            )
            .await
            .map_err(|error| Self::write_error("写入本地意图", error))?
            .id;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交本地意图", error))?;
            Ok(id)
        })
    }

    fn fail_intent(&self, intent: i64, code: ErrorCode) -> PortFuture<'_, Result<bool, PortError>> {
        Box::pin(async move {
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let hit = intents::record_intent_failure(&mut tx, intent, code)
                .await
                .map_err(|error| Self::write_error("记录意图失败", error))?;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交意图失败", error))?;
            Ok(hit)
        })
    }

    fn complete_intent(&self, intent: i64) -> PortFuture<'_, Result<bool, PortError>> {
        Box::pin(async move {
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let hit = intents::complete_intent(&mut tx, intent)
                .await
                .map_err(|error| Self::write_error("完成本地意图", error))?;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交意图完成", error))?;
            Ok(hit)
        })
    }

    fn pending_intents(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Vec<StoredIntent>, PortError>> {
        Box::pin(async move {
            let mut conn = self
                .database
                .pool()
                .acquire()
                .await
                .map_err(|error| Self::read_error("取数据库连接", error))?;
            let rows = intents::intents_for_binding(&mut conn, binding)
                .await
                .map_err(|error| Self::read_error("读本地意图", error))?;
            rows.into_iter()
                .map(|row| {
                    Ok(StoredIntent {
                        id: row.id,
                        binding: row.binding,
                        kind: core_kind(row.kind),
                        entry: row.entry,
                        target_path: row.target_path,
                        source_path: row.source_path,
                        content_hash: row.content_hash,
                        generation: row.generation,
                        failed: row.state == IntentState::Failed,
                        last_error: row.last_error,
                    })
                })
                .collect()
        })
    }
}

impl UploadResumeStore for ClientStorePorts {
    fn load_resume(
        &self,
        op: OpId,
    ) -> PortFuture<'_, Result<Option<StoredUploadResume>, PortError>> {
        Box::pin(async move {
            let mut conn = self
                .database
                .pool()
                .acquire()
                .await
                .map_err(|error| Self::read_error("取数据库连接", error))?;
            let stored = upload_resumes::load(&mut conn, op)
                .await
                .map_err(|error| Self::read_error("读上传续传记录", error))?;
            Ok(stored.map(from_db_resume))
        })
    }

    fn save_resume<'a>(
        &'a self,
        resume: &'a StoredUploadResume,
    ) -> PortFuture<'a, Result<StoredUploadResume, PortError>> {
        Box::pin(async move {
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let saved = upload_resumes::save(
                &mut tx,
                &NewUploadResume {
                    op: resume.op,
                    binding: resume.binding,
                    snapshot_identity: resume.snapshot_identity.clone(),
                    total_size: resume.total_size,
                    digest: resume.digest,
                    session: resume.session.clone(),
                },
            )
            .await
            .map_err(|error| Self::write_error("写入上传续传记录", error))?;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交上传续传记录", error))?;
            Ok(from_db_resume(saved))
        })
    }

    fn discard_resume(&self, op: OpId) -> PortFuture<'_, Result<bool, PortError>> {
        Box::pin(async move {
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let hit = upload_resumes::discard(&mut tx, op)
                .await
                .map_err(|error| Self::write_error("丢弃上传续传记录", error))?;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交续传丢弃", error))?;
            Ok(hit)
        })
    }
}

impl DownloadProgressStore for ClientStorePorts {
    fn load<'a>(
        &'a self,
        key: &'a DownloadKey,
    ) -> PortFuture<'a, Result<Option<StoredProgress>, PortError>> {
        Box::pin(async move {
            let binding = parse_binding(&key.binding)?;
            let version = parse_version(&key.version)?;
            let mut conn = self
                .database
                .pool()
                .acquire()
                .await
                .map_err(|error| Self::read_error("取数据库连接", error))?;
            let record = transfers::transfer(&mut conn, binding, version)
                .await
                .map_err(|error| Self::read_error("读下载进度", error))?;
            let Some(record) = record else {
                return Ok(None);
            };
            // 终态行不再接受进度更新（S031 的传输状态机），因此没有可复用的进度。
            if !matches!(
                record.state,
                TransferState::Running | TransferState::Completed
            ) {
                return Ok(None);
            }
            let geometry = ChunkGeometry::new(record.total_size, record.chunk_size)
                .map_err(|error| Self::read_error("下载进度的块几何", error))?;
            let bitmap = PartBitmap::from_bytes(geometry, record.bitmap)
                .map_err(|error| Self::read_error("下载进度的位图", error))?;
            let progress = StoredProgress::new(
                record.chunk_size,
                record.total_size,
                record.temp_path,
                bitmap,
            )
            .map_err(|detail| Self::read_error("下载进度的块几何", detail))?;
            Ok(Some(progress))
        })
    }

    fn save<'a>(
        &'a self,
        key: &'a DownloadKey,
        progress: &'a StoredProgress,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let binding = parse_binding(&key.binding)?;
            let version = parse_version(&key.version)?;
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            let existing = transfers::transfer(&mut tx, binding, version)
                .await
                .map_err(|error| Self::read_error("读下载进度", error))?;

            let same_geometry = existing.as_ref().is_some_and(|record| {
                record.chunk_size == progress.chunk_size()
                    && record.total_size == progress.total_size()
                    && record.temp_path == progress.temp_path()
            });
            let mut already_completed = false;
            match &existing {
                Some(record) if record.state == TransferState::Completed && same_geometry => {
                    // 已校验完成的记录是幂等终态：不重复记块，也不回退。
                    already_completed = true;
                }
                Some(record) if record.state == TransferState::Running && same_geometry => {
                    // 位图只能增长：已记的块不能从进度里消失。
                    for index in 0..record.chunk_count() {
                        if record.has_chunk(index) && !progress.bitmap().contains(index) {
                            return Err(Self::write_error(
                                "保存下载进度",
                                format!("第 {index} 块已记录但新进度里没有它"),
                            ));
                        }
                    }
                    for index in 0..record.chunk_count() {
                        if progress.bitmap().contains(index) && !record.has_chunk(index) {
                            transfers::record_chunk(&mut tx, binding, version, index)
                                .await
                                .map_err(|error| Self::write_error("记录下载块", error))?;
                        }
                    }
                }
                Some(_) => {
                    // 块几何、临时路径或状态变了：旧进度必须作废，不能拿旧块边界套新文件。
                    transfers::discard_transfer(&mut tx, binding, version)
                        .await
                        .map_err(|error| Self::write_error("作废旧下载进度", error))?;
                    start_transfer(&mut tx, binding, version, progress).await?;
                }
                None => start_transfer(&mut tx, binding, version, progress).await?,
            }

            if progress.bitmap().is_complete() && !already_completed {
                transfers::complete_transfer(&mut tx, binding, version)
                    .await
                    .map_err(|error| Self::write_error("完成下载进度", error))?;
            }
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交下载进度", error))?;
            Ok(())
        })
    }

    fn discard<'a>(&'a self, key: &'a DownloadKey) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let binding = parse_binding(&key.binding)?;
            let version = parse_version(&key.version)?;
            let mut tx = self
                .database
                .begin_write()
                .await
                .map_err(|error| Self::write_error("开始写事务", error))?;
            transfers::discard_transfer(&mut tx, binding, version)
                .await
                .map_err(|error| Self::write_error("丢弃下载进度", error))?;
            tx.commit()
                .await
                .map_err(|error| Self::write_error("提交下载进度丢弃", error))?;
            Ok(())
        })
    }
}

async fn start_transfer(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    binding: BindingId,
    version: VersionId,
    progress: &StoredProgress,
) -> Result<(), PortError> {
    transfers::start_transfer(
        tx,
        &NewTransfer {
            binding,
            version,
            chunk_size: progress.chunk_size(),
            total_size: progress.total_size(),
            bitmap: progress.bitmap().bytes().to_vec(),
            temp_path: progress.temp_path().to_owned(),
        },
    )
    .await
    .map_err(|error| ClientStorePorts::write_error("开始下载进度", error))?;
    Ok(())
}

fn db_kind(kind: LocalWriteKind) -> LocalIntentKind {
    match kind {
        LocalWriteKind::Replace => LocalIntentKind::Replace,
        LocalWriteKind::Move => LocalIntentKind::Move,
        LocalWriteKind::Release => LocalIntentKind::Release,
        LocalWriteKind::Import => LocalIntentKind::Import,
    }
}

fn core_kind(kind: LocalIntentKind) -> LocalWriteKind {
    match kind {
        LocalIntentKind::Replace => LocalWriteKind::Replace,
        LocalIntentKind::Move => LocalWriteKind::Move,
        LocalIntentKind::Release => LocalWriteKind::Release,
        LocalIntentKind::Import => LocalWriteKind::Import,
    }
}

fn from_db_resume(resume: upload_resumes::UploadResume) -> StoredUploadResume {
    StoredUploadResume {
        op: resume.op,
        binding: resume.binding,
        snapshot_identity: resume.snapshot_identity,
        total_size: resume.total_size,
        digest: resume.digest,
        session: resume.session,
    }
}

fn parse_binding(text: &str) -> Result<BindingId, PortError> {
    BindingId::from_hex(text)
        .map_err(|error| ClientStorePorts::read_error("下载进度的绑定身份", error))
}

fn parse_version(text: &str) -> Result<VersionId, PortError> {
    VersionId::from_hex(text)
        .map_err(|error| ClientStorePorts::read_error("下载进度的版本身份", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_transfer::bitmap::PartBitmap;
    use vaultsyncd_transfer::chunk::ChunkGeometry;

    use crate::ports::state::{ActionIntent, LocalWriteKind};
    use crate::testing::{TestDir, enqueue_operation, open_database, seed_binding, test_op_id};
    use vaultsyncd_fs_common::path::RelativePath;

    fn download_key(binding: BindingId, version: VersionId) -> DownloadKey {
        DownloadKey::new(binding.to_hex(), "00".repeat(16), version.to_hex()).expect("下载键")
    }

    fn progress(total: u64, chunk: u64, temp: &str, received: &[u64]) -> StoredProgress {
        let geometry = ChunkGeometry::new(total, chunk).expect("块几何");
        let mut bitmap = PartBitmap::empty(geometry);
        for index in received {
            assert!(bitmap.mark(*index).expect("记块"), "块 {index} 是新块");
        }
        StoredProgress::new(chunk, total, temp, bitmap).expect("进度")
    }

    #[tokio::test]
    async fn download_progress_round_trips_through_the_transfers_table() {
        let dir = TestDir::new("download-progress");
        let database = Arc::new(open_database(&dir).await);
        let binding = seed_binding(&database, 0x41).await;
        let version = VersionId::from_bytes([0x51; 16]);
        let ports = ClientStorePorts::new(Arc::clone(&database));
        let key = download_key(binding, version);

        assert!(ports.load(&key).await.expect("读进度").is_none());
        let partial = progress(10, 4, "cache/51.part", &[0, 2]);
        ports.save(&key, &partial).await.expect("保存进度");
        assert_eq!(ports.load(&key).await.expect("读进度"), Some(partial));

        // 完成的位图把记录标成已校验；再次保存是幂等的，不会回退。
        let complete = progress(10, 4, "cache/51.part", &[0, 1, 2]);
        ports.save(&key, &complete).await.expect("保存完成进度");
        assert_eq!(
            ports.load(&key).await.expect("读进度"),
            Some(complete.clone())
        );
        ports.save(&key, &complete).await.expect("重复保存");
        assert_eq!(ports.load(&key).await.expect("读进度"), Some(complete));

        // 几何变了必须作废旧进度，而不是把旧块套到新文件上。
        let resized = progress(20, 4, "cache/51.part", &[]);
        ports.save(&key, &resized).await.expect("换几何");
        assert_eq!(ports.load(&key).await.expect("读进度"), Some(resized));

        // 位图不能回退。
        let earlier = progress(20, 4, "cache/51.part", &[0]);
        ports.save(&key, &earlier).await.expect("记一块");
        assert!(
            ports
                .save(&key, &progress(20, 4, "cache/51.part", &[]))
                .await
                .is_err(),
            "已记录的块不能从进度里消失"
        );

        ports.discard(&key).await.expect("丢弃进度");
        assert!(ports.load(&key).await.expect("读进度").is_none());
        drop(database);
    }

    #[tokio::test]
    async fn upload_resume_is_bound_to_the_operation_and_removed_with_it() {
        let dir = TestDir::new("upload-resume");
        let database = Arc::new(open_database(&dir).await);
        let binding = seed_binding(&database, 0x42).await;
        let op = test_op_id(0x61);
        enqueue_operation(&database, binding, op).await;
        let ports = ClientStorePorts::new(Arc::clone(&database));

        let resume = StoredUploadResume {
            op,
            binding,
            snapshot_identity: "generation-3".to_owned(),
            total_size: 9,
            digest: [0x71; 32],
            session: "aabbcc".to_owned(),
        };
        assert!(ports.load_resume(op).await.expect("读续传").is_none());
        assert_eq!(ports.save_resume(&resume).await.expect("写续传"), resume);
        assert_eq!(ports.load_resume(op).await.expect("读续传"), Some(resume));

        // 操作成功出队 → 外键级联删除续传记录。
        ports.complete(op).await.expect("操作出队");
        assert!(ports.load_resume(op).await.expect("读续传").is_none());
        drop(database);
    }

    #[tokio::test]
    async fn the_queue_records_failures_without_dropping_the_operation() {
        let dir = TestDir::new("queue");
        let database = Arc::new(open_database(&dir).await);
        let binding = seed_binding(&database, 0x43).await;
        let first = test_op_id(0x62);
        let second = test_op_id(0x63);
        enqueue_operation(&database, binding, first).await;
        enqueue_operation(&database, binding, second).await;
        let ports = ClientStorePorts::new(Arc::clone(&database));

        let queued = ports.pending_ops(binding).await.expect("读队列");
        assert_eq!(
            queued.iter().map(|op| op.op).collect::<Vec<_>>(),
            vec![first, second],
            "队列顺序 = 插入顺序"
        );

        ports
            .record_failure(first, ErrorCode::RateLimited)
            .await
            .expect("记失败");
        let queued = ports.pending_ops(binding).await.expect("读队列");
        assert_eq!(queued.len(), 2, "失败不删操作");
        assert_eq!(queued[0].attempts, 1);
        assert_eq!(queued[0].last_error, Some(ErrorCode::RateLimited));

        ports.complete(first).await.expect("完成");
        assert_eq!(
            ports
                .pending_ops(binding)
                .await
                .expect("读队列")
                .iter()
                .map(|op| op.op)
                .collect::<Vec<_>>(),
            vec![second]
        );
        assert!(
            ports.complete(first).await.is_err(),
            "重复完成是明确的缺失，不是静默成功"
        );
        drop(database);
    }

    #[tokio::test]
    async fn intents_are_idempotent_per_target_and_survive_restart() {
        let dir = TestDir::new("intents");
        let database = Arc::new(open_database(&dir).await);
        let binding = seed_binding(&database, 0x44).await;
        let op = test_op_id(0x64);
        enqueue_operation(&database, binding, op).await;
        let ports = ClientStorePorts::new(Arc::clone(&database));

        let intent = ActionIntent {
            op,
            binding,
            kind: LocalWriteKind::Replace,
            entry: None,
            target_path: RelativePath::parse("docs/report.txt").expect("路径"),
            source_path: Some("cache/report.part".to_owned()),
            content_hash: Some([0x81; 32]),
            generation: vaultsyncd_domain::version::Generation::new(5).expect("代际"),
        };
        let id = ports.begin_intent(&intent).await.expect("写意图");
        assert_eq!(ports.begin_intent(&intent).await.expect("重复写意图"), id);

        // 同路径不同期望：明确拒绝，不覆盖另一条意图。
        let different = ActionIntent {
            content_hash: Some([0x82; 32]),
            ..intent.clone()
        };
        let error = ports
            .begin_intent(&different)
            .await
            .expect_err("不允许换期望");
        assert!(
            matches!(error, PortError::Rejected { .. }),
            "实际错误 {error:?}"
        );

        ports
            .fail_intent(id, ErrorCode::HashMismatch)
            .await
            .expect("标记失败");
        let pending = ports.pending_intents(binding).await.expect("读意图");
        assert_eq!(pending.len(), 1);
        assert!(pending[0].failed);
        assert_eq!(pending[0].last_error, Some(ErrorCode::HashMismatch));
        assert_eq!(pending[0].generation, Some(intent.generation));
        assert_eq!(pending[0].content_hash, Some(vec![0x81; 32]));

        // 重启（重新打开同一个库）后意图仍在。
        drop(ports);
        drop(database);
        let reopened = Arc::new(open_database(&dir).await);
        let ports = ClientStorePorts::new(Arc::clone(&reopened));
        assert_eq!(
            ports.pending_intents(binding).await.expect("读意图").len(),
            1
        );
        assert!(ports.complete_intent(id).await.expect("完成意图"));
        assert!(
            ports
                .pending_intents(binding)
                .await
                .expect("读意图")
                .is_empty()
        );
        assert!(!ports.complete_intent(id).await.expect("重复完成"));
        drop(reopened);
    }
}
