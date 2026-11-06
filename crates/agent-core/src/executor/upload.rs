//! 本地 generation 绑定的上传动作（design §8.3—8.4、I02、I06、T04）。
//!
//! 上传动作把“固定快照、上传会话、带原 OpId 的提交、基线确认”串成一个恢复边界：
//! `LocalPort::snapshot` 只读取指定 generation，传输完成后再次 `observe`；generation 已变化时
//! 绝不提交旧快照。服务端只回 `pending` 时保留队列和续传记录，响应丢失时用同一 OpId 查询。
//! 只有 `UploadCommitStore` 确认对应 generation 的基线已经落库，执行器才允许清理续传记录并报告
//! `Completed`。基线存储实现必须以 generation 做条件更新，因此旧 ACK 不会清掉后续本地修改。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId};
use vaultsyncd_domain::version::{Generation, ServerEpoch};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_protocol::generated::{
    ConflictPolicy, OperationKind as WireOperationKind, OperationRequest, OperationResult,
    OperationState,
};
use vaultsyncd_transfer::ports::{PortError, PortFuture};
use vaultsyncd_transfer::upload::{self, UploadErrorKind, UploadOptions, UploadOutcome};

use super::{ActionFailure, ActionHandler, ActionOutcome, OperationKind};
use crate::ports::local::LocalFacts;
use crate::ports::remote::RemotePort;
use crate::ports::state::{QueuedOperation, StoredUploadResume};
use crate::ports::{PortError as AgentPortError, Ports};

/// 上传操作需要的平台/协议描述。
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UploadSpec {
    /// 本地文件路径。
    pub path: RelativePath,
    /// 固定快照的本地 generation。每次可观察本地保存必须由调用方推进它。
    pub generation: Generation,
    /// 新建条目的父目录；`upload_new_entry`/冲突副本必须提供。
    pub parent: Option<EntryId>,
    /// 新建或移动的目标名称。
    pub name: Option<String>,
    /// 新建条目是否声明目标原本不存在。
    pub expected_absent: Option<bool>,
    /// 发送过期基线时的策略。首发固定为保留双方。
    pub conflict_policy: Option<ConflictPolicy>,
}

impl UploadSpec {
    /// 构造一份上传规格。
    #[must_use]
    pub fn new(path: RelativePath, generation: Generation) -> Self {
        Self {
            path,
            generation,
            parent: None,
            name: None,
            expected_absent: None,
            conflict_policy: Some(ConflictPolicy::PreserveBoth),
        }
    }

    /// 设置新建/移动请求的目标父目录与名称。
    #[must_use]
    pub fn with_target(mut self, parent: EntryId, name: impl Into<String>) -> Self {
        self.parent = Some(parent);
        self.name = Some(name.into());
        self
    }

    /// 设置 `create` 的 expected-absent 声明。
    #[must_use]
    pub fn with_expected_absent(mut self, expected: bool) -> Self {
        self.expected_absent = Some(expected);
        self
    }
}

/// 已上传内容等待提交时的基线事务输入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadCommit {
    /// 原始操作身份；重试与查询必须复用它。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 固定的本地 generation。
    pub generation: Generation,
    /// 本地快照身份。
    pub snapshot_identity: String,
    /// finalize 后的内容引用（尚不是 committed 版本）。
    pub upload: UploadOutcome,
    /// 服务端 committed 回执。
    pub result: OperationResult,
}

/// 服务器 committed 后的条件基线写入结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadCommitDisposition {
    /// 当前本地 generation 仍是上传的 generation，基线已更新且可以清 dirty。
    Applied,
    /// 本地已经产生新 generation；只确认旧远端操作，不清理新 dirty 内容。
    StaleGeneration,
}

/// 上传完成后的本地持久化端口。
///
/// 实现必须在一个短事务中：核对当前 generation；若仍相等则写 baseline、写远端 ACK 映射并清理
/// 对应 dirty 标记；若已变大则只保留冲突/ACK 事实并返回 `StaleGeneration`。不得以 mtime 决胜，
/// 也不得把 `StaleGeneration` 解释为可以覆盖本地内容。
pub trait UploadCommitStore: Send + Sync {
    /// 条件写入基线与冲突映射。
    fn commit_upload<'a>(
        &'a self,
        commit: &'a UploadCommit,
    ) -> PortFuture<'a, Result<UploadCommitDisposition, PortError>>;
}

/// 一个可恢复的上传动作处理器。
///
/// `UploadSpec` 由观察/规划层在入队时生成并按 OpId 注册；它包含队列表没有保存的本地路径与
/// generation。处理器本身不生成 OpId，也不按 mtime 判断内容。
pub struct UploadActionHandler {
    device_id: String,
    server_epoch: ServerEpoch,
    options: UploadOptions,
    specs: Mutex<HashMap<OpId, UploadSpec>>,
    commits: Arc<dyn UploadCommitStore>,
}

impl UploadActionHandler {
    /// 构造上传处理器。设备身份与 epoch 必须来自已认证绑定状态，不能临时生成。
    #[must_use]
    pub fn new(
        device_id: impl Into<String>,
        server_epoch: ServerEpoch,
        commits: Arc<dyn UploadCommitStore>,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            server_epoch,
            options: UploadOptions::default(),
            specs: Mutex::new(HashMap::new()),
            commits,
        }
    }

    /// 替换传输参数。
    #[must_use]
    pub fn with_options(mut self, options: UploadOptions) -> Self {
        self.options = options;
        self
    }

    /// 注册一个待执行 OpId 对应的本地快照规格。
    pub fn register(&self, op: OpId, spec: UploadSpec) {
        self.specs.lock().expect("上传规格锁").insert(op, spec);
    }

    /// 移除已完成或取消的规格。
    pub fn unregister(&self, op: OpId) -> Option<UploadSpec> {
        self.specs.lock().expect("上传规格锁").remove(&op)
    }

    fn spec(&self, op: OpId) -> Result<UploadSpec, ActionFailure> {
        self.specs
            .lock()
            .expect("上传规格锁")
            .get(&op)
            .cloned()
            .ok_or_else(|| {
                ActionFailure::new(ErrorCode::NeedsRecovery, "缺少上传操作的本地快照规格")
            })
    }
}

impl ActionHandler for UploadActionHandler {
    fn plan_intent<'a>(
        &'a self,
        _op: &'a QueuedOperation,
    ) -> PortFuture<'a, Result<Option<crate::ports::state::ActionIntent>, ActionFailure>> {
        // 上传只读取固定快照，不替换本地内容，因此没有 local_intents 行；远端提交仍由 pending_ops
        // 与 upload_resumes 持久化。破坏性本地动作由 S059 的处理器负责。
        Box::pin(async { Ok(None) })
    }

    fn execute<'a>(
        &'a self,
        ports: &'a Ports,
        op: &'a QueuedOperation,
        _intent: Option<&'a crate::ports::state::ActionIntent>,
    ) -> PortFuture<'a, Result<ActionOutcome, ActionFailure>> {
        Box::pin(async move { self.execute_upload(ports, op).await })
    }
}

impl UploadActionHandler {
    async fn execute_upload(
        &self,
        ports: &Ports,
        op: &QueuedOperation,
    ) -> Result<ActionOutcome, ActionFailure> {
        let kind = OperationKind::from_db(&op.kind).ok_or_else(|| {
            ActionFailure::new(
                ErrorCode::NeedsRecovery,
                format!("未知上传操作类型 {}", op.kind),
            )
        })?;
        if !matches!(
            kind,
            OperationKind::UploadContent
                | OperationKind::UploadNewEntry
                | OperationKind::UploadMoveWithContent
                | OperationKind::CreateConflictCopy
        ) {
            return Err(ActionFailure::new(
                ErrorCode::UnsupportedEntry,
                format!("操作 {} 不是内容上传", op.kind),
            ));
        }
        let spec = self.spec(op.op)?;

        // 快照前核对观察到的 generation。mtime 不参与判断；平台必须以稳定 generation/file identity
        // 提供事实。snapshot 内部还会做一次代际校验并返回固定内容源。
        let before = ports
            .local
            .observe(&spec.path)
            .await
            .map_err(|error| map_port_failure("上传前观察本地文件", error))?;
        ensure_generation(&before, spec.generation)?;
        let source = ports
            .local
            .snapshot(&spec.path, spec.generation)
            .await
            .map_err(|error| map_port_failure("固定本地上传快照", error))?;

        let stored = ports
            .upload_resumes
            .load_resume(op.op)
            .await
            .map_err(|error| map_port_failure("读取上传续传记录", error))?;
        let resume = match stored {
            Some(record) => Some(
                record
                    .to_transfer()
                    .map_err(|error| map_port_failure("解析上传续传记录", error))?,
            ),
            None => None,
        };
        let target = ports
            .remote
            .upload(op.binding)
            .await
            .map_err(|error| map_port_failure("获取上传端点", error))?;
        let outcome = match upload::run(
            source.as_ref(),
            target.as_ref(),
            resume.as_ref(),
            &self.options,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(failure) => return self.handle_upload_failure(ports, op, failure).await,
        };

        // 上传期间可能发生第二次保存。快照仍是旧内容时，旧内容可以留在服务端对象区，但不能
        // 进入 put/create 提交，也不能清除新 generation 的 dirty 状态。
        let after = ports
            .local
            .observe(&spec.path)
            .await
            .map_err(|error| map_port_failure("上传后复核本地 generation", error))?;
        ensure_generation(&after, spec.generation)?;
        ensure_snapshot_facts(&before, &after, source.total_size())?;

        // finalize 只给内容引用；先保存恢复记录，再发送带原 OpId 的提交命令。
        let resume = vaultsyncd_transfer::upload::UploadResume::new(
            vaultsyncd_transfer::upload::UploadSnapshot::new(
                source.identity().to_owned(),
                source.total_size(),
                outcome.content.digest,
            )
            .map_err(|error| map_upload_failure(error.into()))?,
            outcome.session.clone(),
        )
        .map_err(|error| map_upload_failure(error.into()))?;
        let stored_resume = StoredUploadResume::from_transfer(op.binding, op.op, &resume);
        ports
            .upload_resumes
            .save_resume(&stored_resume)
            .await
            .map_err(|error| map_port_failure("保存已 finalize 的上传会话", error))?;

        let request = self.request(op, &spec, &outcome);
        let result = self
            .submit_or_query(ports.remote.as_ref(), op.binding, op.op, &request)
            .await?;
        let outcome = settle_result(
            op,
            &spec,
            source.identity(),
            outcome,
            result,
            self.commits.clone(),
        )
        .await?;
        if matches!(outcome, ActionOutcome::Completed) {
            ports
                .upload_resumes
                .discard_resume(op.op)
                .await
                .map_err(|error| map_port_failure("清理已提交的上传会话", error))?;
        }
        Ok(outcome)
    }

    async fn handle_upload_failure(
        &self,
        ports: &Ports,
        op: &QueuedOperation,
        failure: vaultsyncd_transfer::upload::UploadFailure,
    ) -> Result<ActionOutcome, ActionFailure> {
        if matches!(failure.kind(), UploadErrorKind::SnapshotChanged { .. }) {
            // 旧快照不能再复用旧会话；下一代操作会建立新的会话。
            ports
                .upload_resumes
                .discard_resume(op.op)
                .await
                .map_err(|error| map_port_failure("丢弃已变化快照的上传会话", error))?;
        } else if let Some(resume) = failure.resume() {
            let stored = StoredUploadResume::from_transfer(op.binding, op.op, resume);
            ports
                .upload_resumes
                .save_resume(&stored)
                .await
                .map_err(|error| map_port_failure("保存上传失败后的续传会话", error))?;
        }
        Err(map_upload_failure(failure))
    }

    fn request(
        &self,
        op: &QueuedOperation,
        spec: &UploadSpec,
        outcome: &UploadOutcome,
    ) -> OperationRequest {
        let (kind, expected_absent) = match OperationKind::from_db(&op.kind) {
            Some(OperationKind::UploadNewEntry | OperationKind::CreateConflictCopy) => (
                WireOperationKind::Create,
                spec.expected_absent.or(Some(true)),
            ),
            Some(OperationKind::UploadMoveWithContent) => (WireOperationKind::PutContent, None),
            _ => (WireOperationKind::PutContent, None),
        };
        OperationRequest {
            base_revision: op.baseline_revision.map(|value| value.to_decimal()),
            base_version_id: op.baseline_version.map(|value| value.to_string()),
            conflict_policy: spec.conflict_policy,
            device_id: self.device_id.clone(),
            entry_id: op.entry.map(|value| value.to_string()),
            expected_absent,
            kind,
            local_generation: Some(spec.generation.to_decimal()),
            name: spec.name.clone(),
            op_id: op.op.to_string(),
            parent_id: spec.parent.map(|value| value.to_string()),
            server_epoch: self.server_epoch.to_decimal(),
            subtree_token: None,
            upload_id: Some(outcome.session.clone()),
            version_id: None,
        }
    }

    async fn submit_or_query(
        &self,
        remote: &dyn RemotePort,
        binding: BindingId,
        op: OpId,
        request: &OperationRequest,
    ) -> Result<OperationResult, ActionFailure> {
        match remote.submit(request).await {
            Ok(result) => validate_result(op, result),
            Err(first) if matches!(first, PortError::Unavailable { .. }) => {
                // POST 的响应可能丢失。查询原 OpId，而不是生成第二个提交身份；查询失败时保留
                // 原网络错误，让调度器退避并在下一轮继续查询。
                match remote.operation(binding, op).await {
                    Ok(result) => validate_result(op, result),
                    Err(_) => Err(map_port_failure("提交响应丢失，按原 OpId 查询失败", first)),
                }
            }
            Err(error) => Err(map_port_failure("提交上传操作", error)),
        }
    }
}

async fn settle_result(
    op: &QueuedOperation,
    spec: &UploadSpec,
    snapshot_identity: &str,
    upload: UploadOutcome,
    result: OperationResult,
    commits: Arc<dyn UploadCommitStore>,
) -> Result<ActionOutcome, ActionFailure> {
    match result.state {
        OperationState::Pending => Ok(ActionOutcome::Pending { poll_after: None }),
        OperationState::Rejected => Err(ActionFailure::new(
            result
                .error_code
                .as_ref()
                .and_then(map_wire_error_code)
                .unwrap_or(ErrorCode::NeedsRecovery),
            result
                .error_message
                .unwrap_or_else(|| "服务器拒绝上传提交".to_owned()),
        )),
        OperationState::Committed => {
            let expected = spec.generation.to_decimal();
            if result.echo_local_generation.as_deref() != Some(expected.as_str()) {
                return Err(ActionFailure::new(
                    ErrorCode::HashMismatch,
                    "服务器 ACK 的 local_generation 与固定上传快照不符",
                ));
            }
            let commit = UploadCommit {
                op: op.op,
                binding: op.binding,
                generation: spec.generation,
                snapshot_identity: snapshot_identity.to_owned(),
                upload,
                result,
            };
            match commits
                .commit_upload(&commit)
                .await
                .map_err(|error| map_port_failure("提交后写入本地 baseline", error))?
            {
                UploadCommitDisposition::Applied | UploadCommitDisposition::StaleGeneration => {}
            }
            // StaleGeneration 也表示服务器已 committed；commit store 已负责保留新 dirty 内容，
            // 因此旧 ACK 只能完成旧 OpId，不能把新代际标 clean。
            Ok(ActionOutcome::Completed)
        }
    }
}

fn ensure_generation(facts: &LocalFacts, expected: Generation) -> Result<(), ActionFailure> {
    if facts.generation != expected {
        return Err(ActionFailure::new(
            ErrorCode::LocalChanged,
            format!(
                "本地 generation 已从 {} 变为 {}，不提交旧快照",
                expected, facts.generation
            ),
        ));
    }
    Ok(())
}

fn ensure_snapshot_facts(
    before: &LocalFacts,
    after: &LocalFacts,
    snapshot_size: u64,
) -> Result<(), ActionFailure> {
    if before.size != snapshot_size || after.size != snapshot_size {
        return Err(ActionFailure::new(
            ErrorCode::LocalChanged,
            format!(
                "本地文件长度在固定快照前后不一致：观察到 {} -> {}，快照为 {}",
                before.size, after.size, snapshot_size
            ),
        ));
    }
    if before.file_identity.is_some()
        && after.file_identity.is_some()
        && before.file_identity != after.file_identity
    {
        return Err(ActionFailure::new(
            ErrorCode::LocalChanged,
            "本地文件身份在上传期间发生变化，不提交旧快照",
        ));
    }
    Ok(())
}

fn validate_result(op: OpId, result: OperationResult) -> Result<OperationResult, ActionFailure> {
    if result.op_id != op.to_string() {
        return Err(ActionFailure::new(
            ErrorCode::OpIdReused,
            "服务器返回的操作身份不是原始 OpId",
        ));
    }
    Ok(result)
}

fn map_wire_error_code(code: &vaultsyncd_protocol::generated::ErrorCode) -> Option<ErrorCode> {
    use vaultsyncd_protocol::generated::ErrorCode as WireErrorCode;
    Some(match code {
        WireErrorCode::AuthExpired => ErrorCode::AuthExpired,
        WireErrorCode::AccessDenied => ErrorCode::AccessDenied,
        WireErrorCode::BaseMismatch => ErrorCode::BaseMismatch,
        WireErrorCode::SubtreeChanged => ErrorCode::SubtreeChanged,
        WireErrorCode::OpIdReused => ErrorCode::OpIdReused,
        WireErrorCode::ReconcileRequired => ErrorCode::ReconcileRequired,
        WireErrorCode::CursorExpired => ErrorCode::CursorExpired,
        WireErrorCode::EpochChanged => ErrorCode::EpochChanged,
        WireErrorCode::VersionGone => ErrorCode::VersionGone,
        WireErrorCode::InvalidName => ErrorCode::InvalidName,
        WireErrorCode::InvalidRequest | WireErrorCode::PartConflict => {
            return None;
        }
        WireErrorCode::UnsupportedEntry => ErrorCode::UnsupportedEntry,
        WireErrorCode::HashMismatch => ErrorCode::HashMismatch,
        WireErrorCode::UnsupportedStorageBackend => ErrorCode::UnsupportedStorageBackend,
        WireErrorCode::ServerPlaceholderUnsupported => ErrorCode::ServerPlaceholderUnsupported,
        WireErrorCode::RootUnavailable => ErrorCode::RootUnavailable,
        WireErrorCode::NeedsRecovery => ErrorCode::NeedsRecovery,
        WireErrorCode::InsufficientStorage => ErrorCode::InsufficientStorage,
        WireErrorCode::RateLimited => ErrorCode::RateLimited,
        WireErrorCode::ServiceNotReady => {
            return None;
        }
    })
}

fn map_upload_failure(failure: vaultsyncd_transfer::upload::UploadFailure) -> ActionFailure {
    let code = match failure.kind() {
        UploadErrorKind::SnapshotChanged { .. } | UploadErrorKind::ReadSnapshot { .. } => {
            ErrorCode::LocalChanged
        }
        UploadErrorKind::PartRejected { .. }
        | UploadErrorKind::SessionMismatch { .. }
        | UploadErrorKind::FinalizedMismatch { .. } => ErrorCode::HashMismatch,
        UploadErrorKind::Cancelled => ErrorCode::RateLimited,
        UploadErrorKind::Port(error) => map_port_code(error),
        UploadErrorKind::InvalidOptions { .. } | UploadErrorKind::InvalidSnapshot { .. } => {
            ErrorCode::NeedsRecovery
        }
        UploadErrorKind::SessionClosed { .. } => ErrorCode::ReconcileRequired,
    };
    ActionFailure::new(code, failure.to_string())
}

fn map_port_failure(context: &str, error: AgentPortError) -> ActionFailure {
    ActionFailure::new(map_port_code(&error), format!("{context}：{error}"))
}

fn map_port_code(error: &PortError) -> ErrorCode {
    match error {
        PortError::Unavailable { .. } => ErrorCode::RateLimited,
        PortError::ReauthRequired { .. } => ErrorCode::AuthExpired,
        PortError::Cancelled => ErrorCode::RateLimited,
        PortError::Rejected { code, .. } => {
            ErrorCode::from_code(code).unwrap_or(ErrorCode::NeedsRecovery)
        }
        PortError::Protocol { .. } => ErrorCode::NeedsRecovery,
        PortError::Local { kind, .. } => match kind {
            vaultsyncd_transfer::ports::LocalErrorKind::Read
            | vaultsyncd_transfer::ports::LocalErrorKind::Missing => ErrorCode::LocalChanged,
            vaultsyncd_transfer::ports::LocalErrorKind::Write
            | vaultsyncd_transfer::ports::LocalErrorKind::NoSpace => ErrorCode::FileBusy,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vaultsyncd_domain::version::Revision;

    #[test]
    fn generation_guard_rejects_an_ack_for_a_newer_local_save() {
        let path = RelativePath::parse("notes.txt").expect("路径");
        let facts = LocalFacts {
            path,
            kind: crate::ports::local::LocalEntryKind::File,
            generation: Generation::new(8).expect("generation"),
            size: 1,
            file_identity: Some(vec![1]),
            usage: vaultsyncd_domain::state::Usage::Idle,
        };
        let error = ensure_generation(&facts, Generation::new(7).expect("generation"))
            .expect_err("旧 ACK 不能确认新 generation");
        assert_eq!(error.code, ErrorCode::LocalChanged);
    }

    #[test]
    fn request_carries_generation_and_original_op_id() {
        let handler = UploadActionHandler::new(
            "11".repeat(16),
            ServerEpoch::new(3).expect("epoch"),
            Arc::new(NoopCommitStore),
        );
        let op = QueuedOperation {
            op: OpId::from_bytes([0x22; 16]),
            binding: BindingId::from_bytes([0x33; 16]),
            kind: "upload_content".to_owned(),
            entry: Some(EntryId::from_bytes([0x44; 16])),
            baseline_revision: Some(Revision::new(2).expect("revision")),
            baseline_version: None,
            depends_on: None,
            pinned_version: None,
            attempts: 0,
            last_error: None,
        };
        let spec = UploadSpec::new(
            RelativePath::parse("notes.txt").expect("路径"),
            Generation::new(7).expect("generation"),
        );
        let outcome = UploadOutcome {
            content: vaultsyncd_transfer::upload::ContentReference {
                digest: [1; 32],
                size: 1,
                part_count: 1,
                storage_path: "objects/x".to_owned(),
                replayed: false,
            },
            session: "55".repeat(16),
            uploaded_parts: 1,
            reused_parts: 0,
            resumed: false,
        };
        let request = handler.request(&op, &spec, &outcome);
        assert_eq!(request.op_id, op.op.to_string());
        assert_eq!(request.local_generation.as_deref(), Some("7"));
        assert_eq!(request.base_revision.as_deref(), Some("2"));
        assert_eq!(request.upload_id.as_deref(), Some(outcome.session.as_str()));
    }

    struct NoopCommitStore;
    impl UploadCommitStore for NoopCommitStore {
        fn commit_upload<'a>(
            &'a self,
            _commit: &'a UploadCommit,
        ) -> PortFuture<'a, Result<UploadCommitDisposition, PortError>> {
            Box::pin(async { Ok(UploadCommitDisposition::StaleGeneration) })
        }
    }
}
