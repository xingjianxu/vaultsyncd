//! 真实上传动作处理器、客户端库和绑定控制消费者；测试平台不是Windows原生证据。
use super::*;
use crate::{
    bindings::control::BindingControls,
    executor::{
        UploadActionHandler, UploadCommit, UploadCommitDisposition, UploadCommitStore, UploadSpec,
    },
    ports::{local::*, remote::RemotePort},
};
use std::path::PathBuf;
use vaultsyncd_client_store::entries::{self, BaselineEntry, EntryKind, LocalEntry};
use vaultsyncd_domain::{
    state::{Dirty, Residency, Usage},
    version::{Revision, ServerEpoch},
};
use vaultsyncd_protocol::generated::{OperationRequest, OperationResult};
use vaultsyncd_transfer::ports::{SnapshotSource, UploadTarget, VersionedContent};
const BYTES: &[u8] = b"unconfirmed source bytes kept after no-space";

#[tokio::test]
async fn real_binding_pause_and_unbind_close_scheduler_gate_without_deleting_queue() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    let controls = Arc::new(BindingControls::new(rig.db.clone()));
    let mut scheduler = rig.scheduler();
    scheduler.gate = controls.clone();
    controls.set_paused(rig.binding, true).await.unwrap();
    assert!(
        scheduler
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
    assert_eq!(scheduler.status(rig.binding).await.unwrap()[0].op, op);
    controls.set_paused(rig.binding, false).await.unwrap();
    scheduler.run_round(rig.binding).await.unwrap();
    let queued = rig.enqueue(2, "apply_remote", 2, None).await;
    controls.unbind_keep_data(rig.binding).await.unwrap();
    assert!(
        scheduler
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
    assert_eq!(scheduler.status(rig.binding).await.unwrap()[0].op, queued);
    assert_eq!(rig.handler.calls(), vec![op]);
}

#[tokio::test]
async fn real_binding_exclusion_and_reconciliation_gate_retains_operation() {
    let rig = Rig::new().await;
    let op = rig.enqueue(1, "apply_remote", 1, None).await;
    let vault = EntryId::from_bytes([0xaa; 16]);
    let entry = EntryId::from_bytes([1; 16]);
    let mut tx = rig.db.begin_write().await.unwrap();
    for base in [
        BaselineEntry {
            entry: vault,
            parent: None,
            name: String::new(),
            kind: EntryKind::Directory,
            revision: Revision::new(1).unwrap(),
            content_version: None,
            deleted: false,
        },
        BaselineEntry {
            entry,
            parent: Some(vault),
            name: "private.txt".into(),
            kind: EntryKind::File,
            revision: Revision::new(1).unwrap(),
            content_version: Some(VersionId::from_bytes([1; 16])),
            deleted: false,
        },
    ] {
        entries::put_baseline(&mut tx, rig.binding, &base)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let controls = Arc::new(BindingControls::new(rig.db.clone()));
    let mut scheduler = rig.scheduler();
    scheduler.gate = controls.clone();
    controls
        .change_rules(rig.binding, "exclude-private", "private.txt\n")
        .await
        .unwrap();
    assert!(
        scheduler
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty()
    );
    controls
        .change_rules(rig.binding, "unexclude-private", "")
        .await
        .unwrap();
    assert!(
        scheduler
            .run_round(rig.binding)
            .await
            .unwrap()
            .tasks
            .is_empty(),
        "取消排除不能绕过三方重协调"
    );
    assert_eq!(scheduler.status(rig.binding).await.unwrap()[0].op, op);
    assert!(rig.handler.calls().is_empty());
}

struct Source(Vec<u8>);
impl SnapshotSource for Source {
    fn identity(&self) -> &str {
        "fixed-s062-snapshot"
    }
    fn total_size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            self.0
                .get(offset as usize..(offset + length) as usize)
                .map(|b| b.to_vec())
                .ok_or_else(|| PortError::protocol("短读"))
        })
    }
}
struct UploadLocal {
    path: PathBuf,
    no_space: bool,
}
impl LocalPort for UploadLocal {
    fn capabilities(&self) -> LocalCapabilities {
        LocalCapabilities::full()
    }
    fn observe<'a>(&'a self, path: &RelativePath) -> PortFuture<'a, Result<LocalFacts, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            Ok(LocalFacts {
                path,
                kind: LocalEntryKind::File,
                generation: Generation::new(7).unwrap(),
                size: std::fs::metadata(&self.path).unwrap().len(),
                file_identity: Some(vec![7]),
                usage: Usage::Idle,
            })
        })
    }
    fn snapshot(
        &self,
        _: &RelativePath,
        _: Generation,
    ) -> PortFuture<'_, Result<Arc<dyn SnapshotSource>, PortError>> {
        Box::pin(async move {
            if self.no_space {
                return Err(PortError::local(
                    crate::ports::LocalErrorKind::NoSpace,
                    "固定上传快照空间不足",
                ));
            }
            Ok(Arc::new(Source(std::fs::read(&self.path).unwrap())) as Arc<dyn SnapshotSource>)
        })
    }
    fn apply(&self, _: &ApplyRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { panic!("上传不能替换本地内容") })
    }
    fn move_entry(&self, _: &MoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { panic!("上传不能移动本地内容") })
    }
    fn remove(&self, _: &RemoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { panic!("上传不能删除本地内容") })
    }
    fn release(&self, _: &ReleaseRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { panic!("空间不足不能驱逐完整缓存") })
    }
}
struct UploadRemote;
impl RemotePort for UploadRemote {
    fn submit(&self, _: &OperationRequest) -> PortFuture<'_, Result<OperationResult, PortError>> {
        Box::pin(async { panic!("失败上传不提交操作") })
    }
    fn operation(
        &self,
        _: BindingId,
        _: OpId,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        Box::pin(async { panic!("上传未开始不查询提交") })
    }
    fn upload(&self, _: BindingId) -> PortFuture<'_, Result<Arc<dyn UploadTarget>, PortError>> {
        Box::pin(async {
            Err(PortError::unavailable(
                "远端服务暂不可达",
                Some(Duration::from_secs(123)),
            ))
        })
    }
    fn content(
        &self,
        _: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn VersionedContent>, PortError>> {
        Box::pin(async { panic!("上传不下载") })
    }
}
struct NoCommit;
impl UploadCommitStore for NoCommit {
    fn commit_upload<'a>(
        &'a self,
        _: &'a UploadCommit,
    ) -> PortFuture<'a, Result<UploadCommitDisposition, PortError>> {
        Box::pin(async { panic!("失败上传不能确认基线") })
    }
}

#[tokio::test]
async fn actual_upload_handler_through_adapter_preserves_retry_after_no_space_and_dirty_bytes() {
    for no_space in [false, true] {
        let rig = Rig::new().await;
        let op = rig.enqueue(1, "upload_content", 1, None).await;
        let source = rig.dir.database().with_extension("dirty-bytes");
        std::fs::write(&source, BYTES).unwrap();
        let mut tx = rig.db.begin_write().await.unwrap();
        entries::put_local(
            &mut tx,
            rig.binding,
            &LocalEntry {
                entry: EntryId::from_bytes([1; 16]),
                generation: Generation::new(7).unwrap(),
                dirty: Dirty::AwaitingCommit,
                residency: Residency::Materialized,
                fingerprint: Some(vaultsyncd_transfer::hash::digest_of(BYTES).to_vec()),
                file_identity: Some(vec![7]),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let upload = UploadActionHandler::new(
            "02".repeat(16),
            ServerEpoch::new(4).unwrap(),
            Arc::new(NoCommit),
        );
        upload.register(
            op,
            UploadSpec::new(
                RelativePath::parse("private.txt").unwrap(),
                Generation::new(7).unwrap(),
            ),
        );
        let state = Arc::new(ClientStorePorts::new(rig.db.clone()));
        let ports = Ports::new(
            Arc::new(UploadLocal {
                path: source.clone(),
                no_space,
            }),
            Arc::new(rig.clock.clone()),
            Arc::new(UploadRemote),
            state.clone(),
            state.clone(),
            state,
        );
        let scheduler = Scheduler::new(
            rig.db.clone(),
            ports,
            ActionAdapter(upload),
            rig.gate.clone(),
            Arc::new(Samples(AtomicU64::new(0))),
            Limits::default(),
        )
        .unwrap();
        scheduler.run_round(rig.binding).await.unwrap();
        let status = scheduler.status(rig.binding).await.unwrap().remove(0);
        assert_eq!(
            status.error,
            Some(if no_space {
                ErrorCode::InsufficientStorage
            } else {
                ErrorCode::RateLimited
            })
        );
        assert_eq!(
            status.due.as_unix_seconds(),
            if no_space { 1060 } else { 1123 }
        );
        assert_eq!(status.offline, !no_space);
        assert_eq!(status.needs_attention(), no_space);
        assert!(!status.halted);
        assert_eq!(std::fs::read(&source).unwrap(), BYTES);
        let mut conn = rig.db.pool().acquire().await.unwrap();
        assert_eq!(
            entries::local_entry(&mut conn, rig.binding, EntryId::from_bytes([1; 16]))
                .await
                .unwrap()
                .unwrap()
                .dirty,
            Dirty::AwaitingCommit
        );
    }
}
