//! 真实提交执行适配器（S052）：把 `server-core` 的提交编排接到 SQLite 与 Linux 文件系统。
//!
//! S047 只落地了操作端点的幂等与结果查询，命令体一律 `503 SERVICE_NOT_READY`；S049/S050 也只落了
//! 外部观察的规则与平台部件。本模块补上**内容 put** 的完整执行链路，端口实现在这里、规则仍在核心：
//!
//! - [`SqlIntentLog`]（端口 `IntentLog`）：`fs_intents` 的读写，意图证据 JSON 直接用核心的
//!   `TargetBefore::to_json`/`TargetAfter::to_json` 编码，阶段推进只走
//!   [`vaultsyncd_server_store::intents::advance_intent_phase`]（非法转换由存储层拒绝）。
//! - [`WorkTreeTarget`]（端口 `TargetPort`）：经 `RootHandle` 观察与发布。`observe` 记下这次实测的
//!   身份，`publish` 用它作替换前提（`ExpectedTarget::Present/Absent`），因此“观察之后目标被外部
//!   改写”会在发布前被拒绝而**不会**被硬覆盖；发布本身走
//!   [`vaultsyncd_platform_linux_server::files::publish`]（FICLONE/校验复制 + `renameat2` + 目录刷盘
//!   + 发布后核对）。
//! - [`SqlPutStore`]（端口 `PutCommitStore`）：一个 `BEGIN IMMEDIATE` 事务里
//!   `commit_operation`（事件批次 + 操作终态）、`record_version`、条目 revision 比较并写与意图
//!   `finalized` 一起提交。事务没提交就没有 ACK，`pending` 也绝不会被当成 committed。
//!
//! 失败分类沿用核心的 [`PutFailure`]：`ENOSPC`/`EDQUOT` 映射成 `INSUFFICIENT_STORAGE`（507），
//! 存储/数据库故障 `NEEDS_RECOVERY`（503），过期基线 `BASE_MISMATCH`（409）等。
//!
//! 其余命令见 `commands`/`restore_ops`/`conflict_ops`；目录回收站恢复仍显式未就绪。
//! 启动补完尚未装配，有未完成意图时停写，不能据正常路径测试宣称已完成崩溃恢复。

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId, TxId, UserId, VersionId};
use vaultsyncd_domain::version::{Generation, Revision, ServerEpoch, UtcTimestamp};
use vaultsyncd_platform_linux_server::files::clone::StageFailure;
use vaultsyncd_platform_linux_server::files::publish::{
    self, PublishError, PublishFailure, WORK_FILE_MODE,
};
use vaultsyncd_platform_linux_server::files::{
    self, ExpectedTarget, FileIdentity, ResolveMode, RestrictError, RestrictedDir, RootHandle,
};
use vaultsyncd_protocol::generated::{OperationResult, OperationState as WireState};
use vaultsyncd_server_core::api::operations::OperationFailure;
use vaultsyncd_server_core::commit::intent::{
    NewPutIntent, PutFailure, PutFailureReason, PutIntent, PutPhase, TargetAfter, TargetBefore,
    TargetSnapshot,
};
use vaultsyncd_server_core::commit::put::{CommittedPut, PutRequest, RootCommitGate, commit_put};
use vaultsyncd_server_core::objects::{ContentDigest, ExpectedContent, ObjectLayout};
use vaultsyncd_server_core::ports::intent_log::{IntentLog, IntentLogError};
use vaultsyncd_server_core::ports::object_store::{FileKey, StorageFailure};
use vaultsyncd_server_core::ports::put_store::{
    EntryBaseline, EntryKind, ObjectState, PutCommitStore, PutRecord, PutStoreError, PutWrite,
    RootFacts,
};
use vaultsyncd_server_core::ports::target::{
    PublishRequest, PublishedTarget, TargetError, TargetObservation, TargetPort,
};
use vaultsyncd_server_store::db::{Database, DbError};
use vaultsyncd_server_store::entries as store_entries;
use vaultsyncd_server_store::intents as store_intents;
use vaultsyncd_server_store::intents::{FsIntent, FsIntentKind, FsIntentPhase, NewFsIntent};
use vaultsyncd_server_store::operations as store_operations;
use vaultsyncd_server_store::roots as store_roots;
use vaultsyncd_server_store::versions as store_versions;
use vaultsyncd_server_store::versions::{NewVersion, ObjectHash, VersionOrigin};

use crate::uploads::random_bytes;

/// 一次内容 put 的输入（端点从 wire 请求翻译而来）。
#[derive(Clone, Debug)]
pub struct PutCommand {
    /// 幂等键里的设备身份。
    pub device: vaultsyncd_domain::ids::DeviceId,
    /// 操作身份。
    pub op: vaultsyncd_domain::ids::OpId,
    /// 被替换内容的条目。
    pub entry: EntryId,
    /// 客户端期望的 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端计划时的条目 revision。
    pub base_revision: Revision,
    /// 已 finalize 的内容摘要与长度。
    pub content: ExpectedContent,
    /// 客户端本地代际（回显用）。
    pub local_generation: Option<Generation>,
}

/// 执行器：数据库 + 状态目录 + Root 句柄缓存 + 进程内提交串行化。
pub struct ExecutorContext {
    database: Database,
    state_dir: PathBuf,
    roots: Mutex<HashMap<RootId, Arc<RootHandle>>>,
    gate: tokio::sync::Mutex<RootCommitGate>,
    observer: Option<Arc<crate::external::ObserverRuntime>>,
}

impl ExecutorContext {
    /// 以数据库与状态目录装配；Root 句柄按需打开并缓存（注册时已经通过存储准入）。
    #[must_use]
    pub fn new(
        database: Database,
        state_dir: &Path,
        observer: Option<Arc<crate::external::ObserverRuntime>>,
    ) -> Self {
        Self {
            database,
            state_dir: state_dir.to_path_buf(),
            roots: Mutex::new(HashMap::new()),
            gate: tokio::sync::Mutex::new(RootCommitGate::new()),
            observer,
        }
    }

    /// 对象区布局（与上传 finalize 共用同一个目录）。
    #[must_use]
    pub fn objects(&self) -> ObjectLayout {
        ObjectLayout::new(self.state_dir.join("objects"))
    }

    /// 数据库句柄（命令适配器需要同一个连接池）。
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.database
    }

    /// 按 owner scope 取 Root 的绝对路径与已缓存的 Root 句柄。
    ///
    /// # Errors
    ///
    /// Root 不存在/越权（`RootUnavailable`）或句柄打不开（`SERVICE_NOT_READY` 之外的存储故障）。
    pub async fn root_handle(
        &self,
        owner: UserId,
        root: RootId,
    ) -> Result<Arc<RootHandle>, OperationFailure> {
        let record = {
            let mut conn = self.pool_conn().await?;
            store_roots::root_in_scope(&mut conn, owner, root)
                .await
                .map_err(|error| storage_error(&error.to_string()))?
                .ok_or(OperationFailure::RootUnavailable)?
        };
        self.handle(root, &record.absolute_path)
    }

    /// 读条目的当前 revision 与内容摘要（冲突副本判定用；目录没有摘要）。
    ///
    /// # Errors
    ///
    /// 条目不存在/越权或存储失败。
    pub async fn head_summary(
        &self,
        owner: UserId,
        root: RootId,
        entry: EntryId,
    ) -> Result<(Revision, Option<ContentDigest>), OperationFailure> {
        let mut conn = self.pool_conn().await?;
        let record = store_entries::entry_in_scope(&mut conn, owner, entry)
            .await
            .map_err(|error| storage_error(&error.to_string()))?
            .ok_or_else(|| OperationFailure::BaseMismatch {
                detail: format!("条目 {entry} 不存在或已删除"),
            })?;
        if record.root != root || record.deleted_at.is_some() {
            return Err(OperationFailure::BaseMismatch {
                detail: "条目不属于该 Root 或已删除".to_owned(),
            });
        }
        let digest = store_versions::current_version(&mut conn, owner, root, entry)
            .await
            .map_err(|error| storage_error(&error.to_string()))?
            .map(|version| ContentDigest::from_bytes(version.object_hash));
        Ok((record.revision, digest))
    }

    /// Root 提交串行化门（命名空间/删除命令与 put 共用同一把门）。
    pub fn commit_gate(&self) -> &tokio::sync::Mutex<RootCommitGate> {
        self.observer
            .as_ref()
            .map_or(&self.gate, |observer| observer.commit_gate())
    }

    /// 在共享提交门内核对尚未收尾的意图；失败关闭而不是继续破坏性写入。
    ///
    /// # Errors
    ///
    /// 无法读取或存在未完成意图时返回 NEEDS_RECOVERY。
    pub async fn ensure_no_unfinished(
        &self,
        owner: UserId,
        root: RootId,
    ) -> Result<(), OperationFailure> {
        let mut conn = self.pool_conn().await?;
        let intents = store_intents::unfinished_intents(&mut conn, owner, root)
            .await
            .map_err(|error| storage_error(&error.to_string()))?;
        if intents.is_empty() {
            Ok(())
        } else {
            Err(storage_error(
                "Root 有未完成的文件系统意图，暂停写入并等待恢复",
            ))
        }
    }

    /// 登记一条 `pending` 操作行；已存在时报错（调用方应先查重放）。
    ///
    /// # Errors
    ///
    /// 摘要冲突（`OP_ID_REUSED`）或存储失败。
    pub async fn begin_operation(
        &self,
        owner: UserId,
        key: store_operations::OperationKey,
        digest: store_operations::RequestDigest,
    ) -> Result<(), OperationFailure> {
        let mut tx = self.begin_write().await?;
        let began = store_operations::begin_operation(
            &mut tx,
            owner,
            &store_operations::NewOperation { key, digest },
        )
        .await
        .map_err(|error| operation_store_error(&error))?;
        tx.commit()
            .await
            .map_err(|error| storage_error(&error.to_string()))?;
        match began {
            store_operations::BeginOperation::Started => Ok(()),
            store_operations::BeginOperation::Replayed(_) => Err(OperationFailure::OpIdReused),
        }
    }

    /// 把一个还没有结论的操作记成稳定拒绝（不推进 RootSeq，不产生事件）。
    pub async fn reject_failure(
        &self,
        owner: UserId,
        key: store_operations::OperationKey,
        failure: &OperationFailure,
    ) {
        self.reject(owner, key, failure).await;
    }

    fn handle(&self, root: RootId, path: &str) -> Result<Arc<RootHandle>, OperationFailure> {
        let mut roots = self.roots.lock().map_err(|_| OperationFailure::Storage {
            detail: "Root 句柄缓存被污染".to_owned(),
        })?;
        if let Some(handle) = roots.get(&root) {
            return Ok(Arc::clone(handle));
        }
        let handle = RootHandle::open(Path::new(path), ResolveMode::Auto)
            .map_err(|_error| OperationFailure::RootUnavailable)?;
        let handle = Arc::new(handle);
        roots.insert(root, Arc::clone(&handle));
        Ok(handle)
    }

    /// 执行一条内容 put：先登记 `pending` 操作行，再走核心的提交编排。
    ///
    /// 返回 `Ok` 时元数据事务已经提交（committed）；失败不报告 committed。
    /// 没有未完成意图时尝试稳定拒绝；已有意图或证据读失败时保留 pending，等待恢复。
    ///
    /// # Errors
    ///
    /// 见 [`OperationFailure`]：命令前提、存储、空间不足或 Root 不可用。
    #[allow(clippy::too_many_lines, clippy::result_large_err)]
    pub async fn execute_put(
        &self,
        owner: UserId,
        key: store_operations::OperationKey,
        digest: store_operations::RequestDigest,
        command: &PutCommand,
        at: UtcTimestamp,
    ) -> Result<CommittedPut, OperationFailure> {
        // 1. 命名空间事实：条目路径、父目录、名称与当前 revision 都从库里读（客户端只给条目身份）。
        let root_record = {
            let mut conn = self.pool_conn().await?;
            store_roots::root_in_scope(&mut conn, owner, key.root)
                .await
                .map_err(|error| storage_error(&error.to_string()))?
                .ok_or(OperationFailure::RootUnavailable)?
        };
        let (entry, path) = {
            let mut conn = self.pool_conn().await?;
            let entry = store_entries::entry_in_scope(&mut conn, owner, command.entry)
                .await
                .map_err(|error| storage_error(&error.to_string()))?
                .ok_or_else(|| OperationFailure::BaseMismatch {
                    detail: format!("条目 {} 不存在或已删除", command.entry),
                })?;
            if entry.root != key.root {
                return Err(OperationFailure::BaseMismatch {
                    detail: "条目不属于该 Root".to_owned(),
                });
            }
            if entry.kind != store_entries::EntryKind::File {
                return Err(OperationFailure::UnsupportedEntry {
                    detail: "目录不能承载内容".to_owned(),
                });
            }
            let path = store_entries::path_of(&mut conn, owner, command.entry)
                .await
                .map_err(|error| storage_error(&error.to_string()))?
                .ok_or_else(|| OperationFailure::BaseMismatch {
                    detail: "条目路径不可还原".to_owned(),
                })?;
            (entry, path)
        };
        let parent = entry
            .parent
            .ok_or_else(|| OperationFailure::UnsupportedEntry {
                detail: "Root 根条目不能承载内容".to_owned(),
            })?;

        // 2. 先登记 pending 操作行：后续任何失败都基于这一行给出稳定结论。摘要必须与端点用来
        //    判定重放的是同一份（否则同一请求的重试会变成 OP_ID_REUSED）。
        let began = {
            let mut tx = self.begin_write().await?;
            let began = store_operations::begin_operation(
                &mut tx,
                owner,
                &store_operations::NewOperation { key, digest },
            )
            .await
            .map_err(|error| operation_store_error(&error))?;
            tx.commit()
                .await
                .map_err(|error| storage_error(&error.to_string()))?;
            began
        };
        if let store_operations::BeginOperation::Replayed(record) = began {
            return Err(match record.state {
                store_operations::OperationState::Committed => OperationFailure::Storage {
                    detail: "同一 OpId 已提交，端点应走重放分支".to_owned(),
                },
                store_operations::OperationState::Pending => OperationFailure::Storage {
                    detail: "同一 OpId 仍在 pending，端点应走重放分支".to_owned(),
                },
                store_operations::OperationState::Rejected => OperationFailure::ReconcileRequired {
                    detail: "同一 OpId 已被拒绝，端点应走重放分支".to_owned(),
                },
            });
        }

        let root_handle = match self.handle(key.root, &root_record.absolute_path) {
            Ok(handle) => handle,
            Err(failure) => {
                self.reject(owner, key, &failure).await;
                return Err(failure);
            }
        };
        let layout = self.objects();
        let mut target = WorkTreeTarget::new(Arc::clone(&root_handle), layout.clone());

        // 3. 现场前提：观察目标并按实测结果构造 `before`（与客户端的基线 revision 一致才继续）。
        let before = match target.observe(&path) {
            Ok(observation) => baseline_of(&observation, command.entry, command.base_revision),
            Err(error) => {
                let failure = target_failure(error);
                self.reject(owner, key, &failure).await;
                return Err(failure);
            }
        };
        if entry.revision != command.base_revision {
            let failure = OperationFailure::BaseMismatch {
                detail: format!(
                    "条目 revision 已是 {}，请求基线是 {}",
                    entry.revision, command.base_revision
                ),
            };
            self.reject(owner, key, &failure).await;
            return Err(failure);
        }

        let request = PutRequest {
            intent_id: IntentId::from_bytes(random_bytes()),
            root: key.root,
            entry: command.entry,
            expected_epoch: command.expected_epoch,
            expected_revision: command.base_revision,
            expected_parent: parent,
            expected_name: entry.name.clone(),
            target_path: path.clone(),
            staging_path: format!("put-{}", key.op.to_hex()),
            content: command.content,
            before,
        };

        // 4. 核心编排：epoch/基线/对象/现场复核 → 意图 → 发布 → 元数据事务。
        let mut log =
            SqlIntentLog::new(self.database.clone(), owner).bound(crate::journal::Binding::new(
                key,
                "put",
                serde_json::json!({
                    "entry": command.entry.to_hex(), "revision": command.base_revision.to_string(),
                    "local_generation": command.local_generation.map(|v| v.to_string()),
                }),
            ));
        let mut store = SqlPutStore {
            database: self.database.clone(),
            owner,
            key,
            local_generation: command.local_generation,
        };
        let mut gate = self.commit_gate().lock().await;
        self.ensure_no_unfinished(owner, key.root).await?;
        let outcome = commit_put(&mut gate, &mut store, &mut log, &mut target, at, &request).await;

        match outcome {
            Ok(committed) => {
                // 发布是本服务自己做的：登记自写意图，避免观察器把这次发布再采纳成外部修改。
                if let Some(observer) = self.observer.as_ref() {
                    observer
                        .expect_own_write(
                            key.root,
                            &path,
                            committed.target.identity,
                            command.content,
                        )
                        .await;
                }
                Ok(committed)
            }
            Err(failure) => {
                let mapped = map_put_failure(&failure);
                self.reject(owner, key, &mapped).await;
                Err(mapped)
            }
        }
    }

    /// 把一个还没有结论的操作记成稳定拒绝（不推进 RootSeq，不产生事件）。
    async fn reject(
        &self,
        owner: UserId,
        key: store_operations::OperationKey,
        failure: &OperationFailure,
    ) {
        let result = rejection_result(failure);
        let json = match serde_json::to_string(&result) {
            Ok(json) => json,
            Err(_) => return,
        };
        let Ok(mut tx) = self.begin_write().await else {
            return;
        };
        // 已经写入意图的失败不是稳定拒绝：保留 pending，恢复才有资格给出最终结论。
        // 无法读取证据同样失败关闭，不能将不确定结果写成 rejected。
        let Ok(unfinished) = store_intents::unfinished_intents(&mut tx, owner, key.root).await
        else {
            return;
        };
        if !unfinished.is_empty() {
            return;
        }
        let _ = store_operations::reject_operation(&mut tx, owner, &key, &json).await;
        let _ = tx.commit().await;
    }

    async fn pool_conn(
        &self,
    ) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, OperationFailure> {
        self.database
            .pool()
            .acquire()
            .await
            .map_err(|error| storage_error(&error.to_string()))
    }

    async fn begin_write(
        &self,
    ) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, OperationFailure> {
        self.database
            .begin_write()
            .await
            .map_err(|error: DbError| storage_error(&error.to_string()))
    }
}

fn baseline_of(
    observation: &TargetObservation,
    entry: EntryId,
    revision: Revision,
) -> TargetBefore {
    match observation {
        TargetObservation::Absent => TargetBefore::Absent,
        TargetObservation::Present { identity, digest } => TargetBefore::Present(TargetSnapshot {
            entry,
            revision: revision.get(),
            digest: *digest,
            identity: *identity,
        }),
    }
}

fn storage_error(detail: &str) -> OperationFailure {
    OperationFailure::Storage {
        detail: detail.to_owned(),
    }
}

fn operation_store_error(error: &store_operations::OperationError) -> OperationFailure {
    match error {
        store_operations::OperationError::DigestMismatch { .. } => OperationFailure::OpIdReused,
        store_operations::OperationError::OutOfScope
        | store_operations::OperationError::UnknownOperation { .. } => {
            OperationFailure::RootUnavailable
        }
        other => storage_error(&other.to_string()),
    }
}

fn target_failure(error: TargetError) -> OperationFailure {
    match error {
        TargetError::InvalidRequest { detail } => OperationFailure::InvalidRequest { detail },
        TargetError::Unreadable { detail } => OperationFailure::Storage { detail },
        TargetError::Storage {
            stage,
            failure,
            detail,
        } => match failure {
            StorageFailure::NoSpace | StorageFailure::QuotaExceeded => OperationFailure::NoSpace,
            other => storage_error(&format!("{stage}/{}：{detail}", other.as_str())),
        },
    }
}

/// 观察目标现场并构造 `TargetBefore`（恢复与 put 共用同一份现场复核语义）。
///
/// # Errors
///
/// 目标不可读（权限/挂载/IO）时返回存储失败，不把它当成“不存在”。
pub fn observe_before(
    target: &mut WorkTreeTarget,
    target_path: &str,
    entry: EntryId,
    revision: Revision,
) -> Result<TargetBefore, OperationFailure> {
    let observation = target.observe(target_path).map_err(target_failure)?;
    match observation {
        TargetObservation::Absent => Ok(TargetBefore::Absent),
        TargetObservation::Present { identity, digest } => {
            Ok(TargetBefore::Present(TargetSnapshot {
                entry,
                revision: revision.get(),
                digest,
                identity,
            }))
        }
    }
}

/// 核心提交失败 → 操作端点失败（状态码与错误码由核心的契约表给出）。
fn map_put_failure(failure: &PutFailure) -> OperationFailure {
    match &failure.reason {
        PutFailureReason::EpochChanged { expected, actual } => OperationFailure::EpochChanged {
            expected: *expected,
            actual: *actual,
        },
        PutFailureReason::BaseMismatch { detail } => OperationFailure::BaseMismatch {
            detail: detail.clone(),
        },
        PutFailureReason::SubtreeChanged { detail } => OperationFailure::SubtreeChanged {
            detail: detail.clone(),
        },
        PutFailureReason::Reconcile { .. } | PutFailureReason::BeforeMismatch { .. } => {
            OperationFailure::ReconcileRequired {
                detail: failure.to_string(),
            }
        }
        PutFailureReason::UnsupportedEntry { detail } => OperationFailure::UnsupportedEntry {
            detail: detail.clone(),
        },
        PutFailureReason::InvalidRequest { detail } => OperationFailure::InvalidRequest {
            detail: detail.clone(),
        },
        PutFailureReason::ObjectNotPersisted { digest } => OperationFailure::ReconcileRequired {
            detail: format!("内容对象 {digest} 尚未持久化，需要重新上传"),
        },
        PutFailureReason::ObjectSizeMismatch { expected, actual } => {
            OperationFailure::ReconcileRequired {
                detail: format!("对象长度 {actual} 与请求 {expected} 不符"),
            }
        }
        PutFailureReason::TargetUnreadable(error) => match error {
            TargetError::Storage { failure, .. } => match failure {
                StorageFailure::NoSpace | StorageFailure::QuotaExceeded => {
                    OperationFailure::NoSpace
                }
                _ => storage_error(&failure.to_string()),
            },
            other => storage_error(&other.to_string()),
        },
        PutFailureReason::Publish(error) => match error {
            TargetError::Storage { failure, .. } => match failure {
                StorageFailure::NoSpace | StorageFailure::QuotaExceeded => {
                    OperationFailure::NoSpace
                }
                _ => OperationFailure::Storage {
                    detail: error.to_string(),
                },
            },
            other => OperationFailure::Storage {
                detail: other.to_string(),
            },
        },
        PutFailureReason::OpIdReused { .. } => OperationFailure::OpIdReused,
        PutFailureReason::Gate(_) => storage_error("同一 Root 已有提交在进行，稍后重试"),
        _ => OperationFailure::Storage {
            detail: failure.to_string(),
        },
    }
}

fn rejection_result(failure: &OperationFailure) -> OperationResult {
    use vaultsyncd_protocol::generated::ErrorCode;
    let code = match failure.design_error_code() {
        "BASE_MISMATCH" => Some(ErrorCode::BaseMismatch),
        "SUBTREE_CHANGED" => Some(ErrorCode::SubtreeChanged),
        "RECONCILE_REQUIRED" => Some(ErrorCode::ReconcileRequired),
        "INVALID_REQUEST" => Some(ErrorCode::InvalidRequest),
        "INVALID_NAME" => Some(ErrorCode::InvalidName),
        "UNSUPPORTED_ENTRY" => Some(ErrorCode::UnsupportedEntry),
        "EPOCH_CHANGED" => Some(ErrorCode::EpochChanged),
        "OP_ID_REUSED" => Some(ErrorCode::OpIdReused),
        "INSUFFICIENT_STORAGE" => Some(ErrorCode::InsufficientStorage),
        _ => Some(ErrorCode::NeedsRecovery),
    };
    OperationResult {
        op_id: String::new(),
        state: WireState::Rejected,
        replayed: None,
        conflict: None,
        echo_local_generation: None,
        entry_id: None,
        error_code: code,
        error_message: Some(failure.to_string()),
        revision: None,
        root_seq: None,
        version_id: None,
    }
}

/// `fs_intents` 的 `IntentLog` 适配器。
pub struct SqlIntentLog {
    database: Database,
    owner: UserId,
    binding: Option<crate::journal::Binding>,
}

impl SqlIntentLog {
    /// 以数据库与 Root 所属账号装配（内容 put 与历史版本恢复共用）。
    #[must_use]
    pub fn new(database: Database, owner: UserId) -> Self {
        Self {
            database,
            owner,
            binding: None,
        }
    }

    /// 把完整操作键及业务上下文与新意图原子落盘。
    pub fn bound(mut self, binding: crate::journal::Binding) -> Self {
        self.binding = Some(binding);
        self
    }
}

fn intent_error(error: store_intents::IntentError) -> IntentLogError {
    match error {
        store_intents::IntentError::Db(source) => IntentLogError::Storage {
            detail: source.to_string(),
        },
        store_intents::IntentError::UnknownIntent { id } => IntentLogError::UnknownIntent { id },
        store_intents::IntentError::DuplicateId { id } => IntentLogError::DuplicateIntent { id },
        store_intents::IntentError::OutOfScope => IntentLogError::OutOfScope,
        other => IntentLogError::Rejected {
            code: "intent_rejected",
            detail: other.to_string(),
        },
    }
}

fn phase_of(phase: FsIntentPhase) -> PutPhase {
    match phase {
        FsIntentPhase::Prepared => PutPhase::Prepared,
        FsIntentPhase::Published => PutPhase::Published,
        FsIntentPhase::Finalized => PutPhase::Finalized,
        FsIntentPhase::NeedsRecovery => PutPhase::NeedsRecovery,
    }
}

fn fs_phase_of(phase: PutPhase) -> FsIntentPhase {
    match phase {
        PutPhase::Prepared => FsIntentPhase::Prepared,
        PutPhase::Published => FsIntentPhase::Published,
        PutPhase::Finalized => FsIntentPhase::Finalized,
        PutPhase::NeedsRecovery => FsIntentPhase::NeedsRecovery,
    }
}

pub(crate) fn put_intent_of(record: FsIntent) -> Result<PutIntent, IntentLogError> {
    if record.kind != FsIntentKind::PutContent {
        return Err(IntentLogError::MalformedIntent {
            detail: "意图种类不是 put_content".to_owned(),
        });
    }
    let staging_path = record
        .staging_path
        .ok_or_else(|| IntentLogError::MalformedIntent {
            detail: "put 意图缺少暂存名".to_owned(),
        })?;
    let before = TargetBefore::from_json(&record.expected_before).map_err(|error| {
        IntentLogError::MalformedIntent {
            detail: format!("expected_before 无法解码：{error}"),
        }
    })?;
    let after = TargetAfter::from_json(&record.desired_after).map_err(|error| {
        IntentLogError::MalformedIntent {
            detail: format!("desired_after 无法解码：{error}"),
        }
    })?;
    Ok(PutIntent {
        intent_id: record.intent_id,
        root: record.root,
        target_path: record.target_path,
        staging_path,
        before,
        after,
        phase: phase_of(record.phase),
    })
}

impl IntentLog for SqlIntentLog {
    async fn begin_put(
        &mut self,
        new: &NewPutIntent,
        at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        let mut tx =
            self.database
                .begin_write()
                .await
                .map_err(|error| IntentLogError::Storage {
                    detail: error.to_string(),
                })?;
        let record = store_intents::begin_intent(
            &mut tx,
            self.owner,
            &NewFsIntent {
                intent_id: new.intent_id,
                root: new.root,
                kind: FsIntentKind::PutContent,
                target_path: new.target_path.clone(),
                staging_path: Some(new.staging_path.clone()),
                expected_before: new.before.to_json(),
                desired_after: new.after.to_json(),
            },
            at,
        )
        .await
        .map_err(intent_error)?;
        if let Some(binding) = &self.binding {
            binding
                .attach(&mut tx, new.intent_id)
                .await
                .map_err(|error| IntentLogError::Storage {
                    detail: error.to_string(),
                })?;
        }
        tx.commit().await.map_err(|error| IntentLogError::Storage {
            detail: error.to_string(),
        })?;
        put_intent_of(record)
    }

    async fn advance_put(
        &mut self,
        id: IntentId,
        next: PutPhase,
        at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        let mut tx =
            self.database
                .begin_write()
                .await
                .map_err(|error| IntentLogError::Storage {
                    detail: error.to_string(),
                })?;
        let record =
            store_intents::advance_intent_phase(&mut tx, self.owner, id, fs_phase_of(next), at)
                .await
                .map_err(intent_error)?;
        tx.commit().await.map_err(|error| IntentLogError::Storage {
            detail: error.to_string(),
        })?;
        put_intent_of(record)
    }

    async fn load_put(&mut self, id: IntentId) -> Result<Option<PutIntent>, IntentLogError> {
        let mut conn =
            self.database
                .pool()
                .acquire()
                .await
                .map_err(|error| IntentLogError::Storage {
                    detail: error.to_string(),
                })?;
        match store_intents::intent_in_scope(&mut conn, self.owner, id)
            .await
            .map_err(intent_error)?
        {
            Some(record) => put_intent_of(record).map(Some),
            None => Ok(None),
        }
    }

    async fn unfinished_puts(&mut self, root: RootId) -> Result<Vec<PutIntent>, IntentLogError> {
        let mut conn =
            self.database
                .pool()
                .acquire()
                .await
                .map_err(|error| IntentLogError::Storage {
                    detail: error.to_string(),
                })?;
        let records = store_intents::unfinished_intents(&mut conn, self.owner, root)
            .await
            .map_err(intent_error)?;
        records
            .into_iter()
            .filter(|record| record.kind == FsIntentKind::PutContent)
            .map(put_intent_of)
            .collect()
    }
}

/// 目标文件的父目录句柄：Root 自身直接复用已核对的根句柄，子目录另开受限句柄。
enum ParentDir<'a> {
    Root(&'a RestrictedDir),
    Child(RestrictedDir),
}

impl std::ops::Deref for ParentDir<'_> {
    type Target = RestrictedDir;

    fn deref(&self) -> &Self::Target {
        match self {
            ParentDir::Root(dir) => dir,
            ParentDir::Child(dir) => dir,
        }
    }
}

/// Root 工作目录的 `TargetPort` 实现。
pub struct WorkTreeTarget {
    root: Arc<RootHandle>,
    layout: ObjectLayout,
    /// 最近一次 `observe` 的实测身份：`publish` 拿它当替换前提，观察后被改写就拒绝。
    observed: RefCell<BTreeMap<String, ExpectedTarget>>,
}

impl WorkTreeTarget {
    /// 以 Root 句柄与对象区布局建立端口。
    #[must_use]
    pub fn new(root: Arc<RootHandle>, layout: ObjectLayout) -> Self {
        Self {
            root,
            layout,
            observed: RefCell::new(BTreeMap::new()),
        }
    }

    fn parent_and_name(&self, target_path: &str) -> Result<(ParentDir<'_>, PathBuf), TargetError> {
        let path = Path::new(target_path);
        let name: PathBuf = path
            .file_name()
            .ok_or_else(|| TargetError::InvalidRequest {
                detail: format!("目标路径 {target_path:?} 没有文件名"),
            })?
            .into();
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let dir = if parent.as_os_str().is_empty() {
            ParentDir::Root(self.root.root_dir())
        } else {
            ParentDir::Child(
                self.root
                    .open_dir(parent)
                    .map_err(|error| unreadable(&error))?,
            )
        };
        Ok((dir, name))
    }
}

fn file_key(identity: &FileIdentity) -> FileKey {
    FileKey {
        device: (u64::from(identity.dev_major) << 32) | u64::from(identity.dev_minor),
        inode: identity.inode,
        links: identity.nlink,
        bytes: identity.size,
    }
}

fn unreadable(error: &RestrictError) -> TargetError {
    TargetError::Unreadable {
        detail: error.to_string(),
    }
}

fn failure_of_restrict(error: &RestrictError) -> StorageFailure {
    let os = match error {
        RestrictError::OpenRoot { source, .. }
        | RestrictError::Resolve { source, .. }
        | RestrictError::EscapesRoot { source, .. }
        | RestrictError::SymlinkRejected { source, .. }
        | RestrictError::NotFound { source, .. }
        | RestrictError::AlreadyExists { source, .. }
        | RestrictError::CrossDeviceRename { source, .. }
        | RestrictError::Rename { source, .. }
        | RestrictError::Unlink { source, .. }
        | RestrictError::Fsync { source, .. }
        | RestrictError::Stat { source, .. }
        | RestrictError::PermissionDenied { source, .. }
        | RestrictError::ReadDir { source, .. } => source.raw_os_error(),
        _ => None,
    };
    if matches!(error, RestrictError::PermissionDenied { .. }) {
        return StorageFailure::PermissionDenied;
    }
    classify_os(os)
}

fn classify_os(code: Option<i32>) -> StorageFailure {
    match code {
        Some(28) => StorageFailure::NoSpace,
        Some(122) => StorageFailure::QuotaExceeded,
        Some(13) | Some(1) => StorageFailure::PermissionDenied,
        Some(30) => StorageFailure::ReadOnly,
        Some(_) => StorageFailure::Io,
        None => StorageFailure::Other,
    }
}

fn classify_io(error: &std::io::Error) -> StorageFailure {
    if matches!(error.kind(), std::io::ErrorKind::StorageFull) {
        return StorageFailure::NoSpace;
    }
    classify_os(error.raw_os_error())
}

fn publish_error(error: &PublishError, stage: &'static str) -> TargetError {
    let failure = match &error.reason {
        PublishFailure::Staging(stage_error) => stage_failure_of(&stage_error.reason),
        PublishFailure::TargetObservation(restrict) => failure_of_restrict(restrict),
        PublishFailure::Prepare { source, .. } => classify_io(source),
        PublishFailure::Rename(restrict) | PublishFailure::DirectoryFsync(restrict) => {
            failure_of_restrict(restrict)
        }
        PublishFailure::PreconditionRefused(_) | PublishFailure::TargetVerify(_) => {
            StorageFailure::Other
        }
    };
    TargetError::Storage {
        stage,
        failure,
        detail: error.to_string(),
    }
}

fn stage_failure_of(stage: &StageFailure) -> StorageFailure {
    match stage {
        StageFailure::Create(restrict) => failure_of_restrict(restrict),
        StageFailure::Io { source, .. } => classify_io(source),
        StageFailure::ReflinkFailed(source) => classify_io(source),
        StageFailure::WrongFilesystem { .. }
        | StageFailure::SourceChanged { .. }
        | StageFailure::Verify { .. } => StorageFailure::Other,
    }
}

impl TargetPort for WorkTreeTarget {
    fn observe(&mut self, target_path: &str) -> Result<TargetObservation, TargetError> {
        let (dir, name) = self.parent_and_name(target_path)?;
        let observed = dir.identity_at(&name).map_err(|error| unreadable(&error))?;
        let (observation, expected) = match observed {
            None => (TargetObservation::Absent, ExpectedTarget::Absent),
            Some(identity) => {
                let opened = self
                    .root
                    .open_read(Path::new(target_path))
                    .map_err(|error| unreadable(&error))?;
                let (digest, _bytes) =
                    files::hash_file(&opened.file).map_err(|error| TargetError::Unreadable {
                        detail: error.to_string(),
                    })?;
                (
                    TargetObservation::Present {
                        identity: file_key(&identity),
                        digest: ContentDigest::from_bytes(digest),
                    },
                    ExpectedTarget::Present(identity),
                )
            }
        };
        self.observed
            .borrow_mut()
            .insert(target_path.to_owned(), expected);
        Ok(observation)
    }

    fn publish(&mut self, request: &PublishRequest) -> Result<PublishedTarget, TargetError> {
        let (dir, name) = self.parent_and_name(&request.target_path)?;
        let expected = self
            .observed
            .borrow_mut()
            .remove(&request.target_path)
            .ok_or_else(|| TargetError::InvalidRequest {
                detail: "发布前没有观察过目标现场".to_owned(),
            })?;
        let object_path = self.layout.object_path(&request.digest);
        let object = File::open(&object_path).map_err(|error| TargetError::Storage {
            stage: "open_object",
            failure: classify_io(&error),
            detail: error.to_string(),
        })?;
        let identity =
            FileIdentity::of_fd(std::os::fd::AsRawFd::as_raw_fd(&object)).map_err(|error| {
                TargetError::Unreadable {
                    detail: error.to_string(),
                }
            })?;
        let outcome = publish::publish(publish::PublishRequest {
            source: &object,
            source_identity: &identity,
            target_dir: &dir,
            target_name: Path::new(&name),
            expected: &expected,
            mode: WORK_FILE_MODE,
        })
        .map_err(|error| publish_error(&error, "publish"))?;
        if outcome.bytes != request.size || outcome.sha256 != *request.digest.as_bytes() {
            return Err(TargetError::Storage {
                stage: "publish_verify",
                failure: StorageFailure::Other,
                detail: format!(
                    "发布后实测 {} 字节 / {}，期望 {} 字节 / {}",
                    outcome.bytes,
                    ContentDigest::from_bytes(outcome.sha256),
                    request.size,
                    request.digest
                ),
            });
        }
        Ok(PublishedTarget {
            identity: FileKey {
                device: (u64::from(outcome.target_after.dev_major) << 32)
                    | u64::from(outcome.target_after.dev_minor),
                inode: outcome.target_after.inode,
                links: outcome.target_after.nlink,
                bytes: outcome.bytes,
            },
            digest: ContentDigest::from_bytes(outcome.sha256),
        })
    }
}

/// `PutCommitStore` 的 SQLx 实现：一次 `BEGIN IMMEDIATE` 里写完批次、版本、revision 与意图收尾。
pub struct SqlPutStore {
    pub(crate) database: Database,
    pub(crate) owner: UserId,
    pub(crate) key: store_operations::OperationKey,
    pub(crate) local_generation: Option<Generation>,
}

impl PutCommitStore for SqlPutStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, PutStoreError> {
        let mut conn = connect(&self.database).await?;
        match store_roots::root_in_scope(&mut conn, self.owner, root).await {
            Ok(Some(record)) => Ok(RootFacts {
                epoch: record.epoch,
            }),
            Ok(None) => Err(PutStoreError::UnknownRoot { root }),
            Err(error) => Err(store_failure(error)),
        }
    }

    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, PutStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, entry)
            .await
            .map_err(store_failure)?
            .ok_or(PutStoreError::EntryMissing { entry })?;
        if record.deleted_at.is_some() {
            return Err(PutStoreError::EntryMissing { entry });
        }
        let parent = record
            .parent
            .ok_or_else(|| PutStoreError::MalformedIntent {
                detail: "根条目不能承载内容".to_owned(),
            })?;
        Ok(EntryBaseline {
            entry,
            root,
            parent,
            name: record.name,
            revision: record.revision,
            kind: match record.kind {
                store_entries::EntryKind::File => EntryKind::File,
                store_entries::EntryKind::Directory => EntryKind::Directory,
            },
        })
    }

    async fn object_state(
        &mut self,
        root: RootId,
        digest: ContentDigest,
    ) -> Result<ObjectState, PutStoreError> {
        let mut conn = connect(&self.database).await?;
        let hash: ObjectHash = *digest.as_bytes();
        match store_versions::object_in_scope(&mut conn, self.owner, root, &hash).await {
            Ok(Some(record)) => Ok(ObjectState::Persisted { bytes: record.size }),
            Ok(None) => Ok(ObjectState::Missing),
            Err(error) => Err(store_failure(error)),
        }
    }

    async fn apply_put(&mut self, write: &PutWrite) -> Result<PutRecord, PutStoreError> {
        let mut tx = self
            .database
            .begin_write()
            .await
            .map_err(|error| PutStoreError::Storage {
                detail: error.to_string(),
            })?;
        let revision =
            write
                .expected_revision
                .next()
                .map_err(|_| PutStoreError::MalformedIntent {
                    detail: "revision 越界".to_owned(),
                })?;
        let version = VersionId::from_bytes(random_bytes());
        let tx_id = TxId::from_bytes(random_bytes());
        let events = events_json(
            "put",
            write.entry,
            Some(&write.target_path),
            revision,
            Some((version, write.content.digest, write.content.bytes)),
            "client",
        );
        let result = OperationResult {
            op_id: self.key.op.to_hex(),
            state: WireState::Committed,
            replayed: None,
            conflict: None,
            echo_local_generation: self.local_generation.map(|value| value.to_string()),
            entry_id: Some(write.entry.to_hex()),
            error_code: None,
            error_message: None,
            revision: Some(revision.to_string()),
            root_seq: None,
            version_id: Some(version.to_hex()),
        };
        let result_json =
            serde_json::to_string(&result).map_err(|error| PutStoreError::MalformedIntent {
                detail: format!("操作结果无法序列化：{error}"),
            })?;
        let committed = store_operations::commit_operation(
            &mut tx,
            self.owner,
            &self.key,
            tx_id,
            &events,
            &result_json,
            write.at,
        )
        .await
        .map_err(|error| PutStoreError::Storage {
            detail: error.to_string(),
        })?;

        store_versions::record_version(
            &mut tx,
            self.owner,
            &NewVersion {
                id: version,
                root: write.root,
                entry: write.entry,
                object_hash: *write.content.digest.as_bytes(),
                size: write.content.bytes,
                created_seq: committed.batch.seq,
                origin: VersionOrigin::Client,
                at: write.at,
            },
        )
        .await
        .map_err(version_failure)?;

        store_entries::advance_entry_revision(
            &mut tx,
            self.owner,
            write.entry,
            write.expected_revision,
            revision,
        )
        .await
        .map_err(|error| match error {
            store_entries::EntryError::RevisionChanged { expected, actual } => {
                PutStoreError::RevisionChanged { expected, actual }
            }
            other => PutStoreError::Storage {
                detail: other.to_string(),
            },
        })?;

        store_intents::advance_intent_phase(
            &mut tx,
            self.owner,
            write.intent_id,
            FsIntentPhase::Finalized,
            write.at,
        )
        .await
        .map_err(|error| PutStoreError::Storage {
            detail: error.to_string(),
        })?;

        tx.commit().await.map_err(|error| PutStoreError::Storage {
            detail: error.to_string(),
        })?;

        Ok(PutRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            version,
            revision,
            seq: committed.batch.seq,
            tx: committed.batch.tx,
            digest: write.content.digest,
            size: write.content.bytes,
        })
    }
}

async fn connect(
    database: &Database,
) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, PutStoreError> {
    database
        .pool()
        .acquire()
        .await
        .map_err(|error| PutStoreError::Storage {
            detail: error.to_string(),
        })
}

fn store_failure(error: impl std::fmt::Display) -> PutStoreError {
    PutStoreError::Storage {
        detail: error.to_string(),
    }
}

fn version_failure(error: store_versions::VersionError) -> PutStoreError {
    match error {
        store_versions::VersionError::UnknownObject { .. } => PutStoreError::Storage {
            detail: "版本引用的对象不存在".to_owned(),
        },
        other => PutStoreError::Storage {
            detail: other.to_string(),
        },
    }
}

/// 一条事务批次的事件 JSON（`changes.events_json`）。
///
/// 形状固定为对象数组，字段都是十进制/十六进制文本（与 wire 的十进制计数约定一致）：
/// `kind`（`put`/`create`/`update`/`move`/`delete`）、`entry_id`、`path`、`revision`，内容变化另带
/// `version_id`/`hash`/`size`，`origin` 区分客户端提交与外部采集。客户端整批原子应用。
#[must_use]
pub fn events_json(
    kind: &str,
    entry: EntryId,
    path: Option<&str>,
    revision: Revision,
    content: Option<(VersionId, ContentDigest, u64)>,
    origin: &str,
) -> String {
    let mut event = serde_json::Map::new();
    event.insert(
        "kind".to_owned(),
        serde_json::Value::String(kind.to_owned()),
    );
    event.insert(
        "entry_id".to_owned(),
        serde_json::Value::String(entry.to_hex()),
    );
    if let Some(path) = path {
        event.insert(
            "path".to_owned(),
            serde_json::Value::String(path.to_owned()),
        );
    }
    event.insert(
        "revision".to_owned(),
        serde_json::Value::String(revision.to_string()),
    );
    if let Some((version, digest, size)) = content {
        event.insert(
            "version_id".to_owned(),
            serde_json::Value::String(version.to_hex()),
        );
        event.insert(
            "hash".to_owned(),
            serde_json::Value::String(digest.to_hex()),
        );
        event.insert(
            "size".to_owned(),
            serde_json::Value::String(size.to_string()),
        );
    }
    event.insert(
        "origin".to_owned(),
        serde_json::Value::String(origin.to_owned()),
    );
    serde_json::Value::Array(vec![serde_json::Value::Object(event)]).to_string()
}

/// 服务端当前时间（UTC Unix 秒）。
///
/// # Errors
///
/// 系统时钟早于 1970 或不可用。
pub fn server_now() -> Result<UtcTimestamp, String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("系统时钟不可用：{error}"))?
        .as_secs();
    UtcTimestamp::from_unix_seconds(i64::try_from(seconds).unwrap_or(i64::MAX))
        .map_err(|error| format!("系统时钟越界：{error}"))
}
