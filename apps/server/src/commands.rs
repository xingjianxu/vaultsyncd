//! 命令执行适配器（S052 续接）：命名空间、删除、恢复与冲突副本的 SQLx + 文件系统实现。
//!
//! S052 的第一阶段只接了内容 put（见 [`crate::executor`]）；本模块补齐其余四类命令的端口实现，
//! 规则仍在 `server-core`（[`vaultsyncd_server_core::commit::namespace`]、
//! [`vaultsyncd_server_core::commit::delete`]、`history::{version,trash}`、
//! [`vaultsyncd_server_core::commit::conflict`]），这里只做三件事：
//!
//! - **意图日志**：`fs_intents` 的 `create`/`move`/`delete` 三类意图，证据 JSON 直接用核心的
//!   `NamespaceBefore/After::to_json`、`DeleteSnapshot/DeleteAfter::to_json` 编码，阶段推进只走
//!   `server-store::intents`（非法转换由存储层拒绝）。
//! - **存储事务**：命名空间只改 `parent_id`/`name`/`name_key`/`revision`（不写版本、不搬内容）；
//!   删除按核心给的有界批次软删除，写回收站清单与墓碑，**不删**对象/版本行；恢复按版本追加新当前
//!   版本（旧版本行一字不改）或把回收站子树写回命名空间；冲突副本在单一事务里写冲突条目、版本、
//!   映射与操作结果。每个 `apply_*` 都在一个 `BEGIN IMMEDIATE` 里提交事件批次与意图收尾。
//! - **工作目录动作**：`mkdir`/`renameat2`（不覆盖）与有界批次的删除；读不到一律是错误，不报成
//!   “不存在”。
//!
//! 失败语义与 put 一致：任何失败都不是 committed；端点把失败记成稳定 `rejected`。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId, TxId, UserId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};
use vaultsyncd_platform_linux_server::files::{
    FileIdentity, RestrictError, RestrictedDir, RootHandle,
};
use vaultsyncd_server_core::api::operations::OperationFailure;
use vaultsyncd_server_core::commit::delete::{
    DeleteAfter, DeleteBatch, DeleteIntent, DeleteSnapshot, NewDeleteIntent, SubtreeEntry,
    SubtreeImage,
};
use vaultsyncd_server_core::commit::intent::PutPhase;
use vaultsyncd_server_core::commit::namespace::{
    NamespaceAfter, NamespaceBefore, NamespaceIntent, NamespaceOpKind,
};
use vaultsyncd_server_core::ports::intent_log::{
    DeleteIntentLog, IntentLogError, NamespaceIntentLog,
};
use vaultsyncd_server_core::ports::namespace::{NamespaceError, NamespacePort, PathState};
use vaultsyncd_server_core::ports::namespace_store::{
    NamespaceCommitStore, NamespaceRecord, NamespaceStoreError, NamespaceWrite,
};
use vaultsyncd_server_core::ports::object_store::{FileKey, StorageFailure};
use vaultsyncd_server_core::ports::put_store::{EntryBaseline, EntryKind, RootFacts};
use vaultsyncd_server_core::ports::removal::{RemovalError, RemovalPort, RemovalProgress};
use vaultsyncd_server_core::ports::trash_store::{
    DeleteApply, DeleteRecord, DeleteStore, TrashManifestEntry, TrashStoreError,
};
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::entries as store_entries;
use vaultsyncd_server_store::entries::{EntryError, NewEntry};
use vaultsyncd_server_store::intents as store_intents;
use vaultsyncd_server_store::intents::{FsIntent, FsIntentKind, FsIntentPhase, NewFsIntent};
use vaultsyncd_server_store::operations as store_operations;
use vaultsyncd_server_store::roots as store_roots;
use vaultsyncd_server_store::trash as store_trash;
use vaultsyncd_server_store::versions as store_versions;

use crate::uploads::random_bytes;

/// `fs_intents` 的命名空间/删除意图日志。
pub struct SqlFsIntentLog {
    database: Database,
    owner: UserId,
    binding: Option<crate::journal::Binding>,
    only: Option<IntentId>,
}

impl SqlFsIntentLog {
    /// 以数据库与 Root 所属账号装配。
    #[must_use]
    pub fn new(database: Database, owner: UserId) -> Self {
        Self {
            database,
            owner,
            binding: None,
            only: None,
        }
    }
    /// 新意图的完整业务关联。
    pub fn bound(mut self, binding: crate::journal::Binding) -> Self {
        self.binding = Some(binding);
        self
    }
    /// 恢复只处理指定意图，避免另一操作借用本操作的事务键。
    pub fn only(mut self, id: IntentId) -> Self {
        self.only = Some(id);
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

/// `fs_intents` 阶段 → 核心阶段（恢复适配器也要用）。
#[must_use]
pub fn phase_text(phase: FsIntentPhase) -> PutPhase {
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

fn namespace_intent_of(record: FsIntent) -> Result<NamespaceIntent, IntentLogError> {
    let op = NamespaceOpKind::from_text(record.kind.as_text()).ok_or_else(|| {
        IntentLogError::MalformedIntent {
            detail: format!("意图种类 {} 不是命名空间操作", record.kind.as_text()),
        }
    })?;
    let before = NamespaceBefore::from_json(&record.expected_before).map_err(|error| {
        IntentLogError::MalformedIntent {
            detail: format!("expected_before 无法解码：{error}"),
        }
    })?;
    let after = NamespaceAfter::from_json(&record.desired_after).map_err(|error| {
        IntentLogError::MalformedIntent {
            detail: format!("desired_after 无法解码：{error}"),
        }
    })?;
    Ok(NamespaceIntent {
        intent_id: record.intent_id,
        root: record.root,
        op,
        before,
        after,
        phase: phase_text(record.phase),
    })
}

fn delete_intent_of(record: FsIntent) -> Result<DeleteIntent, IntentLogError> {
    if record.kind != FsIntentKind::Delete {
        return Err(IntentLogError::MalformedIntent {
            detail: "意图种类不是 delete".to_owned(),
        });
    }
    let before = DeleteSnapshot::from_json(&record.expected_before).map_err(|error| {
        IntentLogError::MalformedIntent {
            detail: format!("expected_before 无法解码：{error}"),
        }
    })?;
    let after = DeleteAfter::from_json(&record.desired_after).map_err(|error| {
        IntentLogError::MalformedIntent {
            detail: format!("desired_after 无法解码：{error}"),
        }
    })?;
    Ok(DeleteIntent {
        intent_id: record.intent_id,
        root: record.root,
        before,
        after,
        phase: phase_text(record.phase),
    })
}

async fn connect(database: &Database) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, String> {
    database
        .pool()
        .acquire()
        .await
        .map_err(|error| error.to_string())
}

impl NamespaceIntentLog for SqlFsIntentLog {
    async fn begin_namespace(
        &mut self,
        new: &vaultsyncd_server_core::commit::namespace::NewNamespaceIntent,
        at: UtcTimestamp,
    ) -> Result<NamespaceIntent, IntentLogError> {
        let kind = match new.op {
            NamespaceOpKind::Create => FsIntentKind::Create,
            NamespaceOpKind::Move => FsIntentKind::Move,
        };
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
                kind,
                target_path: new.after.path.clone(),
                staging_path: None,
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
        namespace_intent_of(record)
    }

    async fn advance_namespace(
        &mut self,
        id: IntentId,
        next: PutPhase,
        at: UtcTimestamp,
    ) -> Result<NamespaceIntent, IntentLogError> {
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
        namespace_intent_of(record)
    }

    async fn load_namespace(
        &mut self,
        id: IntentId,
    ) -> Result<Option<NamespaceIntent>, IntentLogError> {
        let mut conn = connect(&self.database)
            .await
            .map_err(|detail| IntentLogError::Storage { detail })?;
        match store_intents::intent_in_scope(&mut conn, self.owner, id)
            .await
            .map_err(intent_error)?
        {
            Some(record) if record.kind != FsIntentKind::PutContent => {
                namespace_intent_of(record).map(Some)
            }
            Some(_) | None => Ok(None),
        }
    }

    async fn unfinished_namespaces(
        &mut self,
        root: RootId,
    ) -> Result<Vec<NamespaceIntent>, IntentLogError> {
        let mut conn = connect(&self.database)
            .await
            .map_err(|detail| IntentLogError::Storage { detail })?;
        let records = store_intents::unfinished_intents(&mut conn, self.owner, root)
            .await
            .map_err(intent_error)?;
        records
            .into_iter()
            .filter(|record| self.only.is_none_or(|id| record.intent_id == id))
            .filter(|record| matches!(record.kind, FsIntentKind::Create | FsIntentKind::Move))
            .map(namespace_intent_of)
            .collect()
    }
}

impl DeleteIntentLog for SqlFsIntentLog {
    async fn begin_delete(
        &mut self,
        new: &NewDeleteIntent,
        at: UtcTimestamp,
    ) -> Result<DeleteIntent, IntentLogError> {
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
                kind: FsIntentKind::Delete,
                target_path: new.before.path.clone(),
                staging_path: None,
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
        delete_intent_of(record)
    }

    async fn advance_delete(
        &mut self,
        id: IntentId,
        next: PutPhase,
        at: UtcTimestamp,
    ) -> Result<DeleteIntent, IntentLogError> {
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
        delete_intent_of(record)
    }

    async fn load_delete(&mut self, id: IntentId) -> Result<Option<DeleteIntent>, IntentLogError> {
        let mut conn = connect(&self.database)
            .await
            .map_err(|detail| IntentLogError::Storage { detail })?;
        match store_intents::intent_in_scope(&mut conn, self.owner, id)
            .await
            .map_err(intent_error)?
        {
            Some(record) if record.kind == FsIntentKind::Delete => {
                delete_intent_of(record).map(Some)
            }
            Some(_) | None => Ok(None),
        }
    }

    async fn unfinished_deletes(
        &mut self,
        root: RootId,
    ) -> Result<Vec<DeleteIntent>, IntentLogError> {
        let mut conn = connect(&self.database)
            .await
            .map_err(|detail| IntentLogError::Storage { detail })?;
        let records = store_intents::unfinished_intents(&mut conn, self.owner, root)
            .await
            .map_err(intent_error)?;
        records
            .into_iter()
            .filter(|record| self.only.is_none_or(|id| record.intent_id == id))
            .filter(|record| record.kind == FsIntentKind::Delete)
            .map(delete_intent_of)
            .collect()
    }
}

/// 工作目录的命名空间/移除端口（同一个 Root 句柄同时实现两者）。
pub struct WorkTreeNamespace {
    root: Arc<RootHandle>,
}

impl WorkTreeNamespace {
    /// 以已核对的 Root 句柄装配。
    #[must_use]
    pub fn new(root: Arc<RootHandle>) -> Self {
        Self { root }
    }

    fn parent_dir(&self, path: &str) -> Result<(ParentDir<'_>, PathBuf), NamespaceError> {
        let path = Path::new(path);
        let name = path
            .file_name()
            .ok_or_else(|| NamespaceError::InvalidRequest {
                detail: "路径没有名称分量".to_owned(),
            })?;
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let dir = if parent.as_os_str().is_empty() {
            ParentDir::Root(self.root.root_dir())
        } else {
            match self.root.open_dir(parent) {
                Ok(dir) => ParentDir::Child(dir),
                Err(RestrictError::NotFound { .. }) => {
                    return Err(NamespaceError::Unreadable {
                        detail: "祖先目录不存在".to_owned(),
                    });
                }
                Err(error) => {
                    return Err(NamespaceError::Unreadable {
                        detail: error.to_string(),
                    });
                }
            }
        };
        Ok((dir, PathBuf::from(name)))
    }
}

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

fn file_key(identity: &FileIdentity) -> FileKey {
    FileKey {
        device: (u64::from(identity.dev_major) << 32) | u64::from(identity.dev_minor),
        inode: identity.inode,
        links: identity.nlink,
        bytes: identity.size,
    }
}

fn namespace_failure_of_restrict(error: &RestrictError) -> NamespaceError {
    match error {
        RestrictError::NotFound { .. } => NamespaceError::Unreadable {
            detail: "路径不存在".to_owned(),
        },
        RestrictError::PermissionDenied { .. } => NamespaceError::Unreadable {
            detail: error.to_string(),
        },
        RestrictError::AlreadyExists { .. } => NamespaceError::Storage {
            stage: "mkdir",
            failure: StorageFailure::Other,
            detail: error.to_string(),
        },
        RestrictError::CrossDeviceRename { .. } => NamespaceError::Storage {
            stage: "rename",
            failure: StorageFailure::Other,
            detail: error.to_string(),
        },
        other => NamespaceError::Storage {
            stage: "namespace",
            failure: classify_restrict(other),
            detail: other.to_string(),
        },
    }
}

fn removal_failure_of_restrict(error: &RestrictError, stage: &'static str) -> RemovalError {
    match error {
        RestrictError::NotFound { .. } => RemovalError::Storage {
            stage,
            failure: StorageFailure::Other,
            detail: "路径不存在".to_owned(),
        },
        RestrictError::PermissionDenied { .. } => RemovalError::Storage {
            stage,
            failure: StorageFailure::PermissionDenied,
            detail: error.to_string(),
        },
        RestrictError::AlreadyExists { .. } => RemovalError::InUse {
            path: error.to_string(),
            detail: "目录非空或仍被占用".to_owned(),
        },
        other => RemovalError::Storage {
            stage,
            failure: classify_restrict(other),
            detail: other.to_string(),
        },
    }
}

fn classify_restrict(error: &RestrictError) -> StorageFailure {
    let code = match error {
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
    match code {
        Some(28) => StorageFailure::NoSpace,
        Some(122) => StorageFailure::QuotaExceeded,
        Some(13) | Some(1) => StorageFailure::PermissionDenied,
        Some(30) => StorageFailure::ReadOnly,
        Some(_) => StorageFailure::Io,
        None => StorageFailure::Other,
    }
}

impl NamespacePort for WorkTreeNamespace {
    fn inspect(&mut self, path: &str) -> Result<PathState, NamespaceError> {
        let (dir, name) = self.parent_dir(path)?;
        match dir.identity_at(&name) {
            Ok(None) => Ok(PathState::Absent),
            Ok(Some(identity)) => {
                let key = file_key(&identity);
                if identity.is_directory() {
                    Ok(PathState::Directory { identity: key })
                } else if identity.is_regular_file() {
                    Ok(PathState::File { identity: key })
                } else {
                    Err(NamespaceError::Unreadable {
                        detail: format!("路径 {path} 不是普通文件或目录"),
                    })
                }
            }
            Err(error) => Err(namespace_failure_of_restrict(&error)),
        }
    }

    fn create_directory(&mut self, path: &str) -> Result<PathState, NamespaceError> {
        let (dir, name) = self.parent_dir(path)?;
        dir.mkdir(&name, 0o755)
            .map_err(|error| namespace_failure_of_restrict(&error))?;
        dir.fsync_dir()
            .map_err(|error| namespace_failure_of_restrict(&error))?;
        self.inspect(path)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<PathState, NamespaceError> {
        let (from_dir, from_name) = self.parent_dir(from)?;
        let (to_dir, to_name) = self.parent_dir(to)?;
        from_dir
            .rename(&from_name, &to_dir, &to_name, true)
            .map_err(|error| namespace_failure_of_restrict(&error))?;
        to_dir
            .fsync_dir()
            .map_err(|error| namespace_failure_of_restrict(&error))?;
        from_dir
            .fsync_dir()
            .map_err(|error| namespace_failure_of_restrict(&error))?;
        self.inspect(to)
    }
}

impl RemovalPort for WorkTreeNamespace {
    fn remove_file(&mut self, path: &str) -> Result<(), RemovalError> {
        let (dir, name) = self
            .parent_dir(path)
            .map_err(|error| RemovalError::Unreadable {
                detail: error.to_string(),
            })?;
        dir.unlink(&name)
            .map_err(|error| removal_failure_of_restrict(&error, "unlink"))?;
        dir.fsync_dir()
            .map_err(|error| removal_failure_of_restrict(&error, "fsync"))?;
        Ok(())
    }

    fn remove_directory_batch(
        &mut self,
        path: &str,
        max_entries: usize,
    ) -> Result<RemovalProgress, RemovalError> {
        if max_entries == 0 {
            return Err(RemovalError::InvalidRequest {
                detail: "批次上限不能是 0".to_owned(),
            });
        }
        let (dir, name) = self
            .parent_dir(path)
            .map_err(|error| RemovalError::Unreadable {
                detail: error.to_string(),
            })?;
        // 目标已经不存在（前一批删完或因外部删除）：对调用方就是“删完了”。
        match dir.identity_at(&name) {
            Ok(None) => return Ok(RemovalProgress::finished(0)),
            Ok(Some(identity)) if !identity.is_directory() => {
                return Err(RemovalError::InvalidRequest {
                    detail: format!("路径 {path} 不是目录"),
                });
            }
            Ok(Some(_)) => {}
            Err(error) => return Err(removal_failure_of_restrict(&error, "inspect")),
        }
        let mut budget = max_entries;
        let mut removed = remove_tree_bounded(&dir, &name, &mut budget, 0)?;
        if budget == 0 {
            // 预算用完：目录自身留到下一批。它现在可能是空的，下一批会先看空目录再删掉。
            return Ok(RemovalProgress::partial(removed));
        }
        // 子树内容都删完了，最后删目录自身（非空说明还有本批预算之外的项，报失败而不是递归）。
        dir.unlink_dir(&name)
            .map_err(|error| removal_failure_of_restrict(&error, "rmdir"))?;
        dir.fsync_dir()
            .map_err(|error| removal_failure_of_restrict(&error, "fsync"))?;
        removed += 1;
        Ok(RemovalProgress::finished(removed))
    }
}

/// 递归删除 `dir/name`，最多处理 `budget` 个目录项（含目录自身）；返回删除数量。
fn remove_tree_bounded(
    parent: &RestrictedDir,
    name: &Path,
    budget: &mut usize,
    depth: usize,
) -> Result<u64, RemovalError> {
    const MAX_DEPTH: usize = 128;
    if depth > MAX_DEPTH {
        return Err(RemovalError::InvalidRequest {
            detail: "目录层级过深，拒绝递归".to_owned(),
        });
    }
    let dir = match parent.child_dir(name) {
        Ok(dir) => dir,
        Err(RestrictError::NotFound { .. }) => return Ok(0),
        Err(error) => return Err(removal_failure_of_restrict(&error, "open_dir")),
    };
    let mut removed = 0_u64;
    let entries = dir
        .list_entries()
        .map_err(|error| removal_failure_of_restrict(&error, "list"))?;
    for entry in entries {
        if *budget == 0 {
            return Ok(removed);
        }
        let child = Path::new(&entry.name);
        if entry.kind == vaultsyncd_platform_linux_server::files::EntryKind::Directory {
            let nested = remove_tree_bounded(&dir, child, budget, depth + 1)?;
            removed += nested;
            if *budget == 0 {
                return Ok(removed);
            }
            dir.unlink_dir(child)
                .map_err(|error| removal_failure_of_restrict(&error, "rmdir"))?;
            *budget -= 1;
            removed += 1;
        } else if entry.kind == vaultsyncd_platform_linux_server::files::EntryKind::File {
            dir.unlink(child)
                .map_err(|error| removal_failure_of_restrict(&error, "unlink"))?;
            *budget -= 1;
            removed += 1;
        } else {
            return Err(RemovalError::InvalidRequest {
                detail: format!("目录项 {:?} 不是普通文件或目录", entry.name),
            });
        }
    }
    Ok(removed)
}

/// 命名空间提交的存储实现。
pub struct SqlNamespaceStore {
    database: Database,
    owner: UserId,
    key: store_operations::OperationKey,
    digest: store_operations::RequestDigest,
    local_generation: Option<vaultsyncd_domain::version::Generation>,
}

impl SqlNamespaceStore {
    /// 以数据库、Root 所属账号与本次操作的幂等键装配（结果与事件在同一事务里提交）。
    #[must_use]
    pub fn new(
        database: Database,
        owner: UserId,
        key: store_operations::OperationKey,
        digest: store_operations::RequestDigest,
    ) -> Self {
        Self {
            database,
            owner,
            key,
            digest,
            local_generation: None,
        }
    }

    /// 客户端本地代际：写进持久结果，重放与重启补完都回显它。
    #[must_use]
    pub fn with_local_generation(
        mut self,
        generation: Option<vaultsyncd_domain::version::Generation>,
    ) -> Self {
        self.local_generation = generation;
        self
    }
}

fn namespace_store_error(error: impl std::fmt::Display) -> NamespaceStoreError {
    NamespaceStoreError::Storage {
        detail: error.to_string(),
    }
}

fn map_entry_error(error: EntryError) -> NamespaceStoreError {
    match error {
        EntryError::RevisionChanged { expected, actual } => {
            NamespaceStoreError::RevisionChanged { expected, actual }
        }
        EntryError::DuplicateName { parent, name } => {
            NamespaceStoreError::SlotOccupied { parent, name }
        }
        EntryError::DuplicateId { id } => NamespaceStoreError::EntryExists { entry: id },
        EntryError::EntryNotFound { id } | EntryError::ParentNotFound { id } => {
            NamespaceStoreError::EntryMissing { entry: id }
        }
        other => namespace_store_error(other),
    }
}

impl NamespaceCommitStore for SqlNamespaceStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, NamespaceStoreError> {
        let mut conn = connect(&self.database)
            .await
            .map_err(namespace_store_error)?;
        match store_roots::root_in_scope(&mut conn, self.owner, root)
            .await
            .map_err(namespace_store_error)?
        {
            Some(record) => Ok(RootFacts {
                epoch: record.epoch,
            }),
            None => Err(NamespaceStoreError::UnknownRoot { root }),
        }
    }

    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, NamespaceStoreError> {
        let mut conn = connect(&self.database)
            .await
            .map_err(namespace_store_error)?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, entry)
            .await
            .map_err(namespace_store_error)?
            .ok_or(NamespaceStoreError::EntryMissing { entry })?;
        if record.root != root || record.deleted_at.is_some() {
            return Err(NamespaceStoreError::EntryMissing { entry });
        }
        // 端口类型里 parent 不是可选：合成根条目用自身当“没有父目录”的哨兵。唯一读 parent 的
        // 消费点是 `check_directory_parent`（只看 kind）与移动基线（根条目不能移动）。
        let parent = record.parent.unwrap_or(entry);
        Ok(EntryBaseline {
            entry,
            root,
            parent,
            name: record.name,
            revision: record.revision,
            kind: entry_kind(record.kind),
        })
    }

    async fn apply_namespace(
        &mut self,
        write: &NamespaceWrite,
    ) -> Result<NamespaceRecord, NamespaceStoreError> {
        let mut tx = self
            .database
            .begin_write()
            .await
            .map_err(namespace_store_error)?;
        crate::journal::ensure_digest(&mut tx, self.owner, &self.key, &self.digest)
            .await
            .map_err(namespace_store_error)?;
        let intent = store_intents::intent_in_scope(&mut tx, self.owner, write.intent_id)
            .await
            .map_err(namespace_store_error)?
            .ok_or(NamespaceStoreError::IllegalIntentTransition {
                from: PutPhase::Prepared,
                to: PutPhase::Finalized,
            })?;
        if intent.phase != FsIntentPhase::Published {
            return Err(NamespaceStoreError::IllegalIntentTransition {
                from: phase_text(intent.phase),
                to: PutPhase::Finalized,
            });
        }
        let tx_id = TxId::from_bytes(random_bytes());
        let next = match write.expected_revision {
            Some(expected) => expected.next().map_err(namespace_store_error)?,
            None => Revision::ZERO,
        };
        let kind_text = if write.from.is_some() {
            "move"
        } else {
            "create"
        };
        let events = serde_json::json!([{
            "kind": kind_text,
            "entry_id": write.entry.to_hex(),
            "parent_id": write.to.parent.to_hex(),
            "name": write.to.name,
            "revision": next.to_string(),
            "origin": "client",
        }])
        .to_string();
        let result_json = crate::journal::with_echo(
            serde_json::json!({
                "op_id": self.key.op.to_hex(),
                "state": "committed",
                "entry_id": write.entry.to_hex(),
                "revision": next.to_string(),
            })
            .to_string(),
            self.local_generation,
        );
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
        .map_err(|error| match error {
            store_operations::OperationError::DigestMismatch { .. } => {
                NamespaceStoreError::Rejected {
                    code: "op_id_reused",
                    detail: "同一 OpId 已用于不同请求".to_owned(),
                }
            }
            other => namespace_store_error(other),
        })?;
        let batch = committed.batch;

        match (&write.from, write.kind) {
            (None, EntryKind::Directory) => {
                store_entries::insert_entry(
                    &mut tx,
                    self.owner,
                    &NewEntry {
                        id: write.entry,
                        root: write.root,
                        parent: write.to.parent,
                        name: write.to.name.clone(),
                        name_key: write.to.name.clone(),
                        kind: store_entries::EntryKind::Directory,
                        revision: Revision::ZERO,
                        content_version: None,
                    },
                    write.at,
                )
                .await
                .map_err(map_entry_error)?;
            }
            (None, _) => {
                return Err(NamespaceStoreError::Rejected {
                    code: "unsupported_create_kind",
                    detail: "新建命令只能创建目录".to_owned(),
                });
            }
            (Some(from), _) => {
                let expected =
                    write
                        .expected_revision
                        .ok_or_else(|| NamespaceStoreError::Rejected {
                            code: "missing_baseline",
                            detail: "移动必须带基线 revision".to_owned(),
                        })?;
                let current = store_entries::entry_in_scope(&mut tx, self.owner, write.entry)
                    .await
                    .map_err(namespace_store_error)?
                    .ok_or(NamespaceStoreError::EntryMissing { entry: write.entry })?;
                if current.revision != expected {
                    return Err(NamespaceStoreError::RevisionChanged {
                        expected,
                        actual: current.revision,
                    });
                }
                if current.parent != Some(from.parent) || current.name != from.name {
                    return Err(NamespaceStoreError::Rejected {
                        code: "source_moved",
                        detail: format!("条目已不在 {} 下名为 {:?}", from.parent, from.name),
                    });
                }
                store_entries::move_entry(
                    &mut tx,
                    self.owner,
                    write.entry,
                    write.to.parent,
                    &write.to.name,
                    &write.to.name,
                    next,
                    write.at,
                )
                .await
                .map_err(map_entry_error)?;
            }
        }

        store_intents::advance_intent_phase(
            &mut tx,
            self.owner,
            write.intent_id,
            FsIntentPhase::Finalized,
            write.at,
        )
        .await
        .map_err(namespace_store_error)?;
        tx.commit().await.map_err(namespace_store_error)?;
        Ok(NamespaceRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            entry: write.entry,
            revision: next,
            seq: batch.seq,
            tx: batch.tx,
        })
    }
}

fn store_kind(kind: EntryKind) -> store_entries::EntryKind {
    match kind {
        EntryKind::File => store_entries::EntryKind::File,
        EntryKind::Directory => store_entries::EntryKind::Directory,
    }
}

fn entry_kind(kind: store_entries::EntryKind) -> EntryKind {
    match kind {
        store_entries::EntryKind::File => EntryKind::File,
        store_entries::EntryKind::Directory => EntryKind::Directory,
    }
}

/// 删除事务的存储实现。
pub struct SqlDeleteStore {
    database: Database,
    owner: UserId,
    key: store_operations::OperationKey,
    digest: store_operations::RequestDigest,
    local_generation: Option<vaultsyncd_domain::version::Generation>,
}

impl SqlDeleteStore {
    /// 以数据库、Root 所属账号与本次操作的幂等键装配（结果与事件在同一事务里提交）。
    #[must_use]
    pub fn new(
        database: Database,
        owner: UserId,
        key: store_operations::OperationKey,
        digest: store_operations::RequestDigest,
    ) -> Self {
        Self {
            database,
            owner,
            key,
            digest,
            local_generation: None,
        }
    }

    /// 客户端本地代际：写进持久结果，重放与重启补完都回显它。
    #[must_use]
    pub fn with_local_generation(
        mut self,
        generation: Option<vaultsyncd_domain::version::Generation>,
    ) -> Self {
        self.local_generation = generation;
        self
    }
}

fn trash_store_error(error: impl std::fmt::Display) -> TrashStoreError {
    TrashStoreError::Storage {
        detail: error.to_string(),
    }
}

impl DeleteStore for SqlDeleteStore {
    async fn root_facts(&mut self, root: RootId) -> Result<RootFacts, TrashStoreError> {
        let mut conn = connect(&self.database).await.map_err(trash_store_error)?;
        match store_roots::root_in_scope(&mut conn, self.owner, root)
            .await
            .map_err(trash_store_error)?
        {
            Some(record) => Ok(RootFacts {
                epoch: record.epoch,
            }),
            None => Err(TrashStoreError::UnknownRoot { root }),
        }
    }

    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, TrashStoreError> {
        let mut conn = connect(&self.database).await.map_err(trash_store_error)?;
        let record = store_entries::entry_in_scope(&mut conn, self.owner, entry)
            .await
            .map_err(trash_store_error)?
            .ok_or(TrashStoreError::EntryMissing { entry })?;
        if record.deleted_at.is_some() {
            return Err(TrashStoreError::EntryMissing { entry });
        }
        // 同命名空间适配器：合成根条目没有父目录，用自身作哨兵；核心随后按清单拒绝删根条目。
        let parent = record.parent.unwrap_or(entry);
        Ok(EntryBaseline {
            entry,
            root,
            parent,
            name: record.name,
            revision: record.revision,
            kind: entry_kind(record.kind),
        })
    }

    async fn subtree_image(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<SubtreeImage, TrashStoreError> {
        let mut conn = connect(&self.database).await.map_err(trash_store_error)?;
        let root_path = store_entries::path_of(&mut conn, self.owner, entry)
            .await
            .map_err(trash_store_error)?
            .ok_or(TrashStoreError::EntryMissing { entry })?;
        let mut entries = Vec::new();
        collect_subtree(&mut conn, self.owner, root, entry, &root_path, &mut entries).await?;
        SubtreeImage::new(entry, entries).map_err(|detail| TrashStoreError::MalformedRow { detail })
    }

    async fn apply_delete(&mut self, apply: &DeleteApply) -> Result<DeleteRecord, TrashStoreError> {
        let mut tx = self
            .database
            .begin_write()
            .await
            .map_err(trash_store_error)?;
        crate::journal::ensure_digest(&mut tx, self.owner, &self.key, &self.digest)
            .await
            .map_err(trash_store_error)?;
        let intent = store_intents::intent_in_scope(&mut tx, self.owner, apply.intent_id)
            .await
            .map_err(trash_store_error)?
            .ok_or(TrashStoreError::IllegalIntentTransition {
                from: PutPhase::Prepared,
                to: PutPhase::Finalized,
            })?;
        if intent.phase != FsIntentPhase::Published {
            return Err(TrashStoreError::IllegalIntentTransition {
                from: phase_text(intent.phase),
                to: PutPhase::Finalized,
            });
        }
        let tx_id = TxId::from_bytes(random_bytes());
        let mut events = Vec::new();
        for item in apply.trash.iter() {
            let mut event = serde_json::json!({
                "kind": "delete",
                "entry_id": item.entry.to_hex(),
                "path": item.path,
                "revision": item.revision.to_string(),
                "origin": "client",
            });
            // 删除事件必须带上被删版本，否则客户端“同 revision”的已确认基线与事件事实
            // 无法相等（S074 T11/T14：本地删除 ACK 保留了版本，事件却缺字段，整批被拒）。
            if let Some(version) = item.content_version {
                let facts = store_versions::version_facts(&mut tx, self.owner, apply.root, version)
                    .await
                    .map_err(trash_store_error)?
                    .ok_or_else(|| TrashStoreError::MalformedRow {
                        detail: format!("删除版本 {version} 缺少版本事实"),
                    })?;
                event["version_id"] = serde_json::Value::String(version.to_hex());
                event["hash"] = serde_json::Value::String(
                    facts
                        .object_hash
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                );
                event["size"] = serde_json::Value::String(facts.size.to_string());
            }
            events.push(event);
        }
        let events_json = serde_json::Value::Array(events).to_string();
        let result_json = crate::journal::with_echo(
            serde_json::json!({
                "op_id": self.key.op.to_hex(),
                "state": "committed",
                "entry_id": apply.entry.to_hex(),
                "revision": apply.trash.iter().find(|item| item.entry == apply.entry)
                    .map(|item| item.revision.to_string()),
            })
            .to_string(),
            self.local_generation,
        );
        let committed = store_operations::commit_operation(
            &mut tx,
            self.owner,
            &self.key,
            tx_id,
            &events_json,
            &result_json,
            apply.at,
        )
        .await
        .map_err(|error| match error {
            store_operations::OperationError::DigestMismatch { .. } => TrashStoreError::Rejected {
                code: "op_id_reused",
                detail: "同一 OpId 已用于不同请求".to_owned(),
            },
            other => trash_store_error(other),
        })?;
        let batch = committed.batch;

        for batch in &apply.batches {
            let items: Vec<store_trash::SoftDelete> = batch
                .entries
                .iter()
                .map(|item| store_trash::SoftDelete {
                    entry: item.entry,
                    revision: item.revision,
                })
                .collect();
            store_trash::soft_delete_batch(&mut tx, self.owner, apply.root, &items, apply.at)
                .await
                .map_err(trash_store_error)?;
        }
        for item in &apply.trash {
            let new = store_trash::NewTrashEntry {
                entry: item.entry,
                root: apply.root,
                parent: item.parent,
                path: item.path.clone(),
                kind: store_kind(item.kind),
                revision: item.revision,
                content_version: item.content_version,
                deleted_at: item.deleted_at,
                expires_at: item.expires_at,
            };
            store_trash::record_trash_entry(&mut tx, self.owner, &new)
                .await
                .map_err(trash_store_error)?;
            store_trash::insert_tombstone(&mut tx, self.owner, &new)
                .await
                .map_err(trash_store_error)?;
            sqlx::query("INSERT INTO trash_membership(entry_id,deletion_root,deletion_id) VALUES (?1,?2,?3)")
                .bind(item.entry.as_bytes().as_slice()).bind(apply.entry.as_bytes().as_slice())
                .bind(apply.intent_id.as_bytes().as_slice()).execute(&mut *tx).await.map_err(trash_store_error)?;
        }
        store_intents::advance_intent_phase(
            &mut tx,
            self.owner,
            apply.intent_id,
            FsIntentPhase::Finalized,
            apply.at,
        )
        .await
        .map_err(trash_store_error)?;
        tx.commit().await.map_err(trash_store_error)?;
        Ok(DeleteRecord {
            intent_id: apply.intent_id,
            phase: PutPhase::Finalized,
            root: apply.root,
            entry: apply.entry,
            trashed: apply.trash.len() as u64,
            seq: batch.seq,
            tx: batch.tx,
        })
    }
}

async fn collect_subtree(
    conn: &mut sqlx::SqliteConnection,
    owner: UserId,
    root: RootId,
    entry: EntryId,
    path: &str,
    out: &mut Vec<SubtreeEntry>,
) -> Result<(), TrashStoreError> {
    let record = store_entries::entry_in_scope(&mut *conn, owner, entry)
        .await
        .map_err(trash_store_error)?
        .ok_or(TrashStoreError::EntryMissing { entry })?;
    if record.root != root || record.deleted_at.is_some() {
        return Err(TrashStoreError::EntryMissing { entry });
    }
    out.push(SubtreeEntry {
        entry,
        parent: record.parent,
        path: path.to_owned(),
        kind: entry_kind(record.kind),
        revision: record.revision,
        content_version: record.content_version,
    });
    if record.kind == store_entries::EntryKind::Directory {
        let children = store_entries::children(&mut *conn, owner, root, entry)
            .await
            .map_err(trash_store_error)?;
        for child in children {
            let child_path = format!("{path}/{}", child.name);
            Box::pin(collect_subtree(
                conn,
                owner,
                root,
                child.id,
                &child_path,
                out,
            ))
            .await?;
        }
    }
    Ok(())
}

/// 由核心给出的清单构造回收站清单（路径与 revision 直接取自冻结清单）。
#[must_use]
pub fn trash_manifest_of(batches: &[DeleteBatch], at: UtcTimestamp) -> Vec<TrashManifestEntry> {
    let expires_at = UtcTimestamp::from_unix_seconds(
        at.as_unix_seconds()
            .saturating_add(vaultsyncd_server_core::commit::delete::TRASH_RETENTION_SECONDS),
    )
    .unwrap_or(at);
    let mut manifest = Vec::new();
    for batch in batches {
        for item in &batch.entries {
            let Some(parent) = item.parent else {
                continue;
            };
            manifest.push(TrashManifestEntry {
                entry: item.entry,
                parent,
                path: item.path.clone(),
                kind: item.kind,
                revision: item.revision,
                content_version: item.content_version,
                deleted_at: at,
                expires_at,
            });
        }
    }
    manifest
}

// ---------------------------------------------------------------------------------------------
// 命令执行入口：create / move / delete
// ---------------------------------------------------------------------------------------------

/// 新建目录命令。
#[derive(Clone, Debug)]
pub struct CreateCommand {
    /// 目标父目录。
    pub parent: EntryId,
    /// 目标名称。
    pub name: String,
    /// 客户端本地代际（持久回显）。
    pub local_generation: Option<vaultsyncd_domain::version::Generation>,
}

/// 同 Root 移动命令。
#[derive(Clone, Debug)]
pub struct MoveCommand {
    /// 被移动的条目。
    pub entry: EntryId,
    /// 客户端计划时的条目 revision。
    pub base_revision: Revision,
    /// 目标父目录。
    pub parent: EntryId,
    /// 目标名称。
    pub name: String,
    /// 客户端本地代际（持久回显）。
    pub local_generation: Option<vaultsyncd_domain::version::Generation>,
}

/// 删除命令。
#[derive(Clone, Debug)]
pub struct DeleteCommand {
    /// 被删除的条目。
    pub entry: EntryId,
    /// 客户端计划时的条目 revision。
    pub base_revision: Revision,
    /// 目录删除的服务端子树 token。
    pub subtree_token: Option<[u8; 16]>,
    /// 客户端本地代际（持久回显）。
    pub local_generation: Option<vaultsyncd_domain::version::Generation>,
}

fn command_storage(detail: impl std::fmt::Display) -> OperationFailure {
    OperationFailure::Storage {
        detail: detail.to_string(),
    }
}

/// 核心失败码 → 操作端点失败（语义相同的错误码，细节原样保留）。
#[must_use]
pub fn map_core_failure(
    code: Option<vaultsyncd_domain::error::ErrorCode>,
    detail: String,
) -> OperationFailure {
    use vaultsyncd_domain::error::ErrorCode;
    match code {
        // 没有带类型的 epoch 事实时保守按需要协调处理；带类型的路径走下面的 *_failure 包装。
        Some(ErrorCode::EpochChanged) => OperationFailure::ReconcileRequired { detail },
        Some(ErrorCode::BaseMismatch) => OperationFailure::BaseMismatch { detail },
        Some(ErrorCode::SubtreeChanged) => OperationFailure::SubtreeChanged { detail },
        Some(ErrorCode::ReconcileRequired) => OperationFailure::ReconcileRequired { detail },
        Some(ErrorCode::OpIdReused) => OperationFailure::OpIdReused,
        Some(ErrorCode::InvalidName) => OperationFailure::InvalidName { detail },
        Some(ErrorCode::UnsupportedEntry) => OperationFailure::UnsupportedEntry { detail },
        Some(ErrorCode::VersionGone) => OperationFailure::VersionGone { detail },
        Some(ErrorCode::RootUnavailable) => OperationFailure::RootUnavailable,
        Some(ErrorCode::InsufficientStorage) => OperationFailure::NoSpace,
        Some(ErrorCode::NeedsRecovery) => OperationFailure::Storage { detail },
        _ => OperationFailure::Storage { detail },
    }
}

/// 命名空间失败 → 操作端点失败；epoch 变化保留请求与当前两个 epoch（客户端据此重建基线）。
#[must_use]
pub fn namespace_failure(
    failure: &vaultsyncd_server_core::commit::namespace::NamespaceFailure,
) -> OperationFailure {
    use vaultsyncd_server_core::commit::namespace::NamespaceFailureReason;
    match &failure.reason {
        NamespaceFailureReason::EpochChanged { expected, actual } => {
            OperationFailure::EpochChanged {
                expected: *expected,
                actual: *actual,
            }
        }
        _ => map_core_failure(failure.error_code(), failure.to_string()),
    }
}

/// 删除失败 → 操作端点失败（epoch 带类型保留）。
#[must_use]
pub fn delete_failure(
    failure: &vaultsyncd_server_core::commit::delete::DeleteFailure,
) -> OperationFailure {
    use vaultsyncd_server_core::commit::delete::DeleteFailureReason;
    match &failure.reason {
        DeleteFailureReason::EpochChanged { expected, actual } => OperationFailure::EpochChanged {
            expected: *expected,
            actual: *actual,
        },
        _ => map_core_failure(failure.error_code(), failure.to_string()),
    }
}

/// 恢复失败 → 操作端点失败（epoch 带类型保留，版本过期是 410 `VERSION_GONE`）。
#[must_use]
pub fn restore_failure(
    failure: &vaultsyncd_server_core::history::RestoreFailure,
) -> OperationFailure {
    use vaultsyncd_server_core::history::RestoreFailureReason;
    match &failure.reason {
        RestoreFailureReason::EpochChanged { expected, actual } => OperationFailure::EpochChanged {
            expected: *expected,
            actual: *actual,
        },
        _ => map_core_failure(failure.error_code(), failure.to_string()),
    }
}

/// 条目在 Root 内的相对路径（恢复命令也要用）。
///
/// # Errors
///
/// 条目不存在/越权或存储失败。
pub async fn path_of(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    entry: EntryId,
) -> Result<String, OperationFailure> {
    entry_path(executor, owner, entry).await
}

async fn entry_path(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    entry: EntryId,
) -> Result<String, OperationFailure> {
    let mut conn = executor
        .database()
        .pool()
        .acquire()
        .await
        .map_err(command_storage)?;
    store_entries::path_of(&mut conn, owner, entry)
        .await
        .map_err(command_storage)?
        .ok_or_else(|| OperationFailure::BaseMismatch {
            detail: format!("条目 {entry} 不存在或已删除"),
        })
}

async fn entry_facts(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    entry: EntryId,
) -> Result<store_entries::EntryRecord, OperationFailure> {
    let mut conn = executor
        .database()
        .pool()
        .acquire()
        .await
        .map_err(command_storage)?;
    store_entries::entry_in_scope(&mut conn, owner, entry)
        .await
        .map_err(command_storage)?
        .ok_or_else(|| OperationFailure::BaseMismatch {
            detail: format!("条目 {entry} 不存在或已删除"),
        })
}

fn child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_owned()
    } else {
        format!("{parent_path}/{name}")
    }
}

/// 执行 create：在父目录下新建空目录（只改命名空间，不碰内容）。
///
/// # Errors
///
/// 见 [`OperationFailure`]：形状/基线/占用/存储失败。
pub async fn execute_create(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    key: store_operations::OperationKey,
    digest: store_operations::RequestDigest,
    command: &CreateCommand,
    expected_epoch: vaultsyncd_domain::version::ServerEpoch,
    at: UtcTimestamp,
) -> Result<NamespaceRecord, OperationFailure> {
    let parent_path = entry_path(executor, owner, command.parent).await?;
    let target_path = child_path(&parent_path, &command.name);
    executor.begin_operation(owner, key, digest).await?;
    let handle = match executor.root_handle(owner, key.root).await {
        Ok(handle) => handle,
        Err(failure) => {
            executor.reject_failure(owner, key, &failure).await;
            return Err(failure);
        }
    };
    let mut store = SqlNamespaceStore::new(executor.database().clone(), owner, key, digest)
        .with_local_generation(command.local_generation);
    let mut ns = WorkTreeNamespace::new(handle);
    let mut log = SqlFsIntentLog::new(executor.database().clone(), owner).bound(
        crate::journal::Binding::new(
            key,
            "namespace",
            crate::journal::echo_field(command.local_generation),
        ),
    );
    let request = vaultsyncd_server_core::commit::namespace::NamespaceRequest::CreateDirectory(
        vaultsyncd_server_core::commit::namespace::CreateDirectoryRequest {
            intent_id: IntentId::from_bytes(random_bytes()),
            root: key.root,
            entry: EntryId::from_bytes(random_bytes()),
            expected_epoch,
            parent: command.parent,
            name: command.name.clone(),
            target_path,
        },
    );
    let mut gate = executor.commit_gate().lock().await;
    executor.ensure_no_unfinished(owner, key.root).await?;
    let outcome = vaultsyncd_server_core::commit::namespace::commit_namespace(
        &mut gate, &mut store, &mut log, &mut ns, at, &request,
    )
    .await;
    drop(gate);
    match outcome {
        Ok(committed) => Ok(committed.record),
        Err(failure) => {
            let mapped = namespace_failure(&failure);
            executor.reject_failure(owner, key, &mapped).await;
            Err(mapped)
        }
    }
}

/// 执行 move：同 Root 内改父关系与名称（EntryId 与内容不变）。
///
/// # Errors
///
/// 见 [`OperationFailure`]。
pub async fn execute_move(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    key: store_operations::OperationKey,
    digest: store_operations::RequestDigest,
    command: &MoveCommand,
    expected_epoch: vaultsyncd_domain::version::ServerEpoch,
    at: UtcTimestamp,
) -> Result<NamespaceRecord, OperationFailure> {
    let current = entry_facts(executor, owner, command.entry).await?;
    let Some(current_parent) = current.parent else {
        return Err(OperationFailure::UnsupportedEntry {
            detail: "Root 根条目不能移动".to_owned(),
        });
    };
    let from_path = entry_path(executor, owner, command.entry).await?;
    let parent_path = entry_path(executor, owner, command.parent).await?;
    let to_path = child_path(&parent_path, &command.name);
    executor.begin_operation(owner, key, digest).await?;
    let handle = match executor.root_handle(owner, key.root).await {
        Ok(handle) => handle,
        Err(failure) => {
            executor.reject_failure(owner, key, &failure).await;
            return Err(failure);
        }
    };
    let mut store = SqlNamespaceStore::new(executor.database().clone(), owner, key, digest)
        .with_local_generation(command.local_generation);
    let mut ns = WorkTreeNamespace::new(handle);
    let mut log = SqlFsIntentLog::new(executor.database().clone(), owner).bound(
        crate::journal::Binding::new(
            key,
            "namespace",
            crate::journal::echo_field(command.local_generation),
        ),
    );
    let request = vaultsyncd_server_core::commit::namespace::NamespaceRequest::Move(
        vaultsyncd_server_core::commit::namespace::MoveRequest {
            intent_id: IntentId::from_bytes(random_bytes()),
            root: key.root,
            entry: command.entry,
            expected_epoch,
            expected_revision: command.base_revision,
            from: vaultsyncd_server_core::commit::namespace::NamespaceEndpoint {
                parent: current_parent,
                name: current.name.clone(),
                path: from_path,
            },
            to: vaultsyncd_server_core::commit::namespace::NamespaceEndpoint {
                parent: command.parent,
                name: command.name.clone(),
                path: to_path,
            },
        },
    );
    let mut gate = executor.commit_gate().lock().await;
    executor.ensure_no_unfinished(owner, key.root).await?;
    let outcome = vaultsyncd_server_core::commit::namespace::commit_namespace(
        &mut gate, &mut store, &mut log, &mut ns, at, &request,
    )
    .await;
    drop(gate);
    match outcome {
        Ok(committed) => Ok(committed.record),
        Err(failure) => {
            let mapped = namespace_failure(&failure);
            executor.reject_failure(owner, key, &mapped).await;
            Err(mapped)
        }
    }
}

/// 执行 delete：有界批次删除工作目录内容，并在单一事务里写回收站清单、墓碑与事件。
///
/// # Errors
///
/// 见 [`OperationFailure`]。
pub async fn execute_delete(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    key: store_operations::OperationKey,
    digest: store_operations::RequestDigest,
    command: &DeleteCommand,
    expected_epoch: vaultsyncd_domain::version::ServerEpoch,
    at: UtcTimestamp,
) -> Result<DeleteRecord, OperationFailure> {
    let path = entry_path(executor, owner, command.entry).await?;
    executor.begin_operation(owner, key, digest).await?;
    let handle = match executor.root_handle(owner, key.root).await {
        Ok(handle) => handle,
        Err(failure) => {
            executor.reject_failure(owner, key, &failure).await;
            return Err(failure);
        }
    };
    let mut store = SqlDeleteStore::new(executor.database().clone(), owner, key, digest)
        .with_local_generation(command.local_generation);
    let mut log = SqlFsIntentLog::new(executor.database().clone(), owner).bound(
        crate::journal::Binding::new(
            key,
            "delete",
            crate::journal::echo_field(command.local_generation),
        ),
    );
    if let Err(detail) = verify_delete_scope(
        executor.database(),
        executor.state_dir(),
        owner,
        &handle,
        key.root,
        command.entry,
    )
    .await
    {
        let failure = OperationFailure::ReconcileRequired { detail };
        executor.reject_failure(owner, key, &failure).await;
        return Err(failure);
    }
    let mut ns = WorkTreeNamespace::new(handle);
    let request = vaultsyncd_server_core::commit::delete::DeleteRequest {
        intent_id: IntentId::from_bytes(random_bytes()),
        root: key.root,
        entry: command.entry,
        expected_epoch,
        expected_revision: command.base_revision,
        subtree_token: command
            .subtree_token
            .map(vaultsyncd_server_core::commit::delete::SubtreeToken::from_bytes),
        path,
    };
    let mut gate = executor.commit_gate().lock().await;
    executor.ensure_no_unfinished(owner, key.root).await?;
    let outcome = vaultsyncd_server_core::commit::delete::commit_delete(
        &mut gate, &mut store, &mut log, &mut ns, at, &request,
    )
    .await;
    drop(gate);
    match outcome {
        Ok(committed) => Ok(committed.record),
        Err(failure) => {
            let mapped = delete_failure(&failure);
            executor.reject_failure(owner, key, &mapped).await;
            Err(mapped)
        }
    }
}

/// 删除（及其重启补完）之前核对工作目录：现场不得含冻结清单之外的内容。
///
/// 删除会把工作目录里的东西移走，而回收站只保留**已确认**的版本。外部在观察循环采纳之前新写入的
/// 文件、或原地改写的内容，都是尚未确认的内容，删掉它们就丢数据。因此：
///
/// - 单个文件：现场内容哈希必须等于该条目当前版本；
/// - 目录：每个现场项都必须在冻结清单里且种类相同，文件长度必须等于当前版本长度；
/// - 清单里有而现场没有的项（已被部分删除、或外部删除尚未被采纳）不算问题——删除是幂等的。
///
/// # Errors
///
/// 返回原因文本；调用方把前向路径映射为 `RECONCILE_REQUIRED`、重启补完保持停写并保留证据。
pub async fn verify_delete_scope(
    database: &Database,
    state_dir: &Path,
    owner: UserId,
    handle: &Arc<RootHandle>,
    root: RootId,
    entry: EntryId,
) -> Result<(), String> {
    let mut store = SqlDeleteStore::new(
        database.clone(),
        owner,
        store_operations::OperationKey {
            root,
            device: vaultsyncd_domain::ids::DeviceId::from_bytes([0; 16]),
            op: vaultsyncd_domain::ids::OpId::from_bytes([0; 16]),
        },
        store_operations::request_digest(&[("kind", b"delete_scope")])
            .map_err(|error| error.to_string())?,
    );
    let image = store
        .subtree_image(root, entry)
        .await
        .map_err(|error| error.to_string())?;
    let mut known = std::collections::HashMap::new();
    for item in image.entries() {
        let expected = match item.content_version {
            Some(version) => {
                let mut conn = connect(database).await?;
                let facts = store_versions::version_facts(&mut conn, owner, root, version)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("版本 {version} 的事实不可读"))?;
                Some(facts)
            }
            None => None,
        };
        known.insert(item.path.clone(), (item.kind, expected));
    }
    let root_path = image
        .entries()
        .iter()
        .find(|item| item.entry == entry)
        .map(|item| item.path.clone())
        .ok_or_else(|| "冻结清单缺少根条目".to_owned())?;
    let mut ns = WorkTreeNamespace::new(Arc::clone(handle));
    let mut visited = 0usize;
    let mut pending = vec![root_path];
    while let Some(path) = pending.pop() {
        visited += 1;
        if visited > known.len() {
            return Err("现场项数超过冻结清单".to_owned());
        }
        let (kind, facts) = known
            .get(&path)
            .ok_or_else(|| format!("现场的 {path} 不在冻结清单里"))?;
        match (ns.inspect(&path).map_err(|error| error.to_string())?, kind) {
            (PathState::Absent, _) => {}
            (PathState::Directory { .. }, EntryKind::Directory) => {
                for child in handle
                    .list_dir(Path::new(&path))
                    .map_err(|error| error.to_string())?
                {
                    pending.push(format!("{path}/{}", child.name.to_string_lossy()));
                }
            }
            (PathState::File { identity }, EntryKind::File) => {
                let facts = facts
                    .as_ref()
                    .ok_or_else(|| format!("文件 {path} 的清单没有内容版本"))?;
                if identity.bytes != facts.size {
                    return Err(format!(
                        "文件 {path} 现在 {} 字节，当前版本是 {} 字节：存在尚未采纳的内容",
                        identity.bytes, facts.size
                    ));
                }
                if image.len() == 1 {
                    let layout = crate::object_layout::for_digest(
                        database,
                        state_dir,
                        root,
                        vaultsyncd_server_core::objects::ContentDigest::from_bytes(
                            facts.object_hash,
                        ),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    let mut target =
                        crate::executor::WorkTreeTarget::new(Arc::clone(handle), layout);
                    let observed = vaultsyncd_server_core::ports::target::TargetPort::observe(
                        &mut target,
                        &path,
                    )
                    .map_err(|error| error.to_string())?;
                    if let vaultsyncd_server_core::ports::target::TargetObservation::Present {
                        digest,
                        ..
                    } = observed
                        && digest.as_bytes() != &facts.object_hash
                    {
                        return Err(format!(
                            "文件 {path} 的内容与当前版本不同：存在尚未采纳的内容"
                        ));
                    }
                }
            }
            (other, _) => {
                return Err(format!("现场的 {path} 是 {other}，与冻结清单的种类不一致"));
            }
        }
    }
    Ok(())
}

/// 取一个目录条目的冻结子树清单（与删除时用于比较的 token 来自同一份事实）。
///
/// # Errors
///
/// 条目不存在/越权、不是目录（`UnsupportedEntry`）或存储失败。
pub async fn subtree_image_of(
    executor: &crate::executor::ExecutorContext,
    owner: UserId,
    root: RootId,
    entry: EntryId,
) -> Result<SubtreeImage, OperationFailure> {
    let mut conn = executor
        .database()
        .pool()
        .acquire()
        .await
        .map_err(command_storage)?;
    let record = store_entries::entry_in_scope(&mut conn, owner, entry)
        .await
        .map_err(command_storage)?
        .ok_or_else(|| OperationFailure::BaseMismatch {
            detail: format!("条目 {entry} 不存在或已删除"),
        })?;
    if record.kind != store_entries::EntryKind::Directory {
        return Err(OperationFailure::UnsupportedEntry {
            detail: "只有目录有子树 token".to_owned(),
        });
    }
    drop(conn);
    let mut store = SqlDeleteStore::new(
        executor.database().clone(),
        owner,
        store_operations::OperationKey {
            root,
            device: vaultsyncd_domain::ids::DeviceId::from_bytes([0; 16]),
            op: vaultsyncd_domain::ids::OpId::from_bytes([0; 16]),
        },
        store_operations::request_digest(&[("kind", b"subtree_token")]).map_err(command_storage)?,
    );
    store
        .subtree_image(root, entry)
        .await
        .map_err(command_storage)
}
