//! 固定版本下载 → 整份校验 → 持久意图 → 条件平台替换 → 基线事务。
//!
//! `ControlledApply` 本身不访问文件系统。平台必须在不可并发写入的保护内比较完整 before
//! 证据并替换；普通 observe + rename 不能实现该端口。失败/未知保留意图、暂存与旧基线。
//! 首次导入的 Full install 使用 expected=Absent；事件应用使用已确认版本的内容证据。
//! 上传 ACK 不调用这里，继续使用 S056 的 generation 条件提交，pending 永不成为基线。
use std::sync::Arc;

use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::entries::BaselineEntry;
use vaultsyncd_domain::ids::{BindingId, OpId};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_transfer::download::{self, DownloadOptions};
use vaultsyncd_transfer::ports::{
    DownloadProgressStore, PortError, PortFuture, SnapshotSource, TempContent, VersionPin,
    VersionedContent,
};

use crate::ports::local::{ApplyReceipt, ApplyRequest, LocalFacts};
use crate::recovery::{self, ContentEvidence, changed};

mod store;
use store::{ApplyStore, Journal};

/// 一次真实内容观察。读不到（权限、挂载、扫描中）必须返回 Err，不能伪装 Absent。
pub enum ContentObservation {
    /// 健康根下确认不存在。
    Absent,
    /// 固定快照与对应活动文件事实。
    File {
        /// 同一内容代际的事实。
        facts: LocalFacts,
        /// 不受后续外部写入影响的固定内容。
        snapshot: Arc<dyn SnapshotSource>,
    },
}

/// 平台受控内容端口，由 Windows 应用入口实现；本 crate 不生成 Linux 客户端。
pub trait ControlledLocal: Send + Sync {
    /// 读取真实内容快照；代际必须覆盖外部写入，不依赖数据库 clean 或仅 mtime。
    fn observe_content(
        &self,
        path: &RelativePath,
    ) -> PortFuture<'_, Result<ContentObservation, PortError>>;

    /// 在使用保护/独占句柄内重新检查完整 before（包括实际内容），再原子替换并持久化文件及目录。
    /// Absent 必须使用不覆盖创建；本地再次修改返回 LOCAL_CHANGED，不能先检查后无保护 rename。
    /// 暂存须在保护内重新核对 request.expected_digest，不能安装校验后被篡改的暂存。
    /// receipt 的身份/代际须对应发布后内容；重放同一意图不得无条件再次替换。
    fn replace_verified<'a>(
        &'a self,
        request: &'a ApplyRequest,
        before: &'a ContentEvidence,
    ) -> PortFuture<'a, Result<ApplyReceipt, PortError>>;
}

/// 一次文件内容应用的固定输入。只允许已提交的远端版本，不接受上传 finalize 的内容引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplySpec {
    /// 持久、可重放的动作身份。
    pub op: OpId,
    /// 绑定身份。
    pub binding: BindingId,
    /// 目标相对路径。
    pub target: RelativePath,
    /// 共同基线的比较并写前提；首次安装为 None。
    pub baseline_before: Option<BaselineEntry>,
    /// 完整已提交远端事实。
    pub baseline_after: BaselineEntry,
    /// 固定版本、大小、摘要（从服务器版本事实取得）。
    pub pin: VersionPin,
    /// 前一已确认内容证据；不能仅根据 dirty=clean 构造摘要。
    pub expected: ContentEvidence,
}

impl ApplySpec {
    /// 将 S058 Full 文件安装转换为同一持久应用输入。
    /// ImportLocal::install 适配器可直接调用；目录/按需占位仍走各自显式平台能力，不假装成功。
    pub fn for_import_file(
        request: &crate::bindings::import::ImportInstall,
    ) -> Result<Self, PortError> {
        use vaultsyncd_client_store::bindings::SyncMode;
        use vaultsyncd_client_store::entries::EntryKind;
        use vaultsyncd_domain::ids::{EntryId, VersionId};
        use vaultsyncd_domain::version::Revision;
        use vaultsyncd_protocol::generated::EntryKind as WireKind;
        let invalid = |error: &dyn std::fmt::Display| {
            PortError::rejected("INVALID_ARGUMENT", error.to_string())
        };
        if request.mode != SyncMode::Full || request.remote.kind != WireKind::File {
            return Err(PortError::rejected(
                "UNSUPPORTED",
                "本入口仅安装 Full 固定版本文件",
            ));
        }
        let remote = &request.remote;
        let missing = || PortError::rejected("INVALID_ARGUMENT", "固定版本事实不完整");
        let entry = EntryId::from_hex(&remote.entry_id).map_err(|e| invalid(&e))?;
        let version = VersionId::from_hex(remote.version_id.as_deref().ok_or_else(missing)?)
            .map_err(|e| invalid(&e))?;
        let spec = Self {
            op: request.op,
            binding: request.binding,
            target: request.path.clone(),
            baseline_before: None,
            baseline_after: BaselineEntry {
                entry,
                parent: Some(
                    EntryId::from_hex(remote.parent_id.as_deref().ok_or_else(missing)?)
                        .map_err(|e| invalid(&e))?,
                ),
                name: remote.name.clone(),
                kind: EntryKind::File,
                revision: Revision::new(remote.revision.parse().map_err(|e| invalid(&e))?)
                    .map_err(|e| invalid(&e))?,
                content_version: Some(version),
                deleted: false,
            },
            pin: VersionPin::new(
                request.binding.to_string(),
                entry.to_string(),
                version.to_string(),
                vaultsyncd_transfer::hash::from_hex(remote.hash.as_deref().ok_or_else(missing)?)
                    .map_err(|e| invalid(&e))?,
                remote
                    .size
                    .as_deref()
                    .ok_or_else(missing)?
                    .parse()
                    .map_err(|e| invalid(&e))?,
            )
            .map_err(|e| invalid(&e))?,
            expected: ContentEvidence::Absent,
        };
        store::validate(&spec)?;
        Ok(spec)
    }
}

/// 可恢复的本地受控内容执行器。每个绑定应由应用入口串行调度。
pub struct ControlledApply {
    store: ApplyStore,
    local: Arc<dyn ControlledLocal>,
}
impl ControlledApply {
    /// SQLite 状态与平台受控端口在入口组装。
    pub fn new(database: Arc<Database>, local: Arc<dyn ControlledLocal>) -> Self {
        Self {
            store: ApplyStore::new(database),
            local,
        }
    }

    /// 下载并安装，或核对同一 OpId 的已完成结果。重复请求的全部输入必须相同。
    /// 下载/哈希/平台 I/O 均不持有 SQLite 事务。失败时不出队、不推进事件游标。
    pub async fn apply(
        &self,
        spec: &ApplySpec,
        content: &dyn VersionedContent,
        temp: &dyn TempContent,
        progress: &dyn DownloadProgressStore,
        options: &DownloadOptions,
    ) -> Result<ApplyReceipt, PortError> {
        store::validate(spec)?;
        if let Some(journal) = self.store.load(spec.op).await? {
            if journal.spec()? != *spec {
                return Err(PortError::rejected("OP_ID_REUSED", "应用操作输入已变化"));
            }
            return self.resume(journal).await;
        }
        // clean 标记不授权替换；下载前与发布前都重新核对真实内容。
        if recovery::inspect(&*self.local, &spec.target).await? != spec.expected {
            return Err(changed());
        }
        let downloaded = download::run(content, temp, progress, &spec.pin, options)
            .await
            .map_err(|error| match error {
                download::DownloadError::Port(error) => error,
                other => PortError::rejected("DOWNLOAD_FAILED", other.to_string()),
            })?;
        let journal = self.store.prepare(spec, downloaded.temp_path).await?;
        self.resume(journal).await
    }

    /// 启动先恢复开放意图；返回错误的路径保持 unknown/blocked，不能放开覆盖、删除或释放。
    /// 不需要网络或原调用栈：固定版本、前后摘要、基线和暂存位置均来自 SQLite。
    pub async fn recover(
        &self,
        binding: BindingId,
    ) -> Result<Vec<(OpId, Result<ApplyReceipt, PortError>)>, PortError> {
        let journals = self.store.pending(binding).await?;
        let mut results = Vec::with_capacity(journals.len());
        for journal in journals {
            let op = journal.op();
            results.push((op, self.resume(journal).await));
        }
        Ok(results)
    }

    async fn resume(&self, journal: Journal) -> Result<ApplyReceipt, PortError> {
        let spec = journal.spec()?;
        self.store.check(&journal).await?;
        let mut actual = recovery::inspect(&*self.local, &spec.target).await?;
        let receipt = if actual.has_content(&spec.pin.digest, spec.pin.size) {
            // 文件已换但 DB 尚未更新：只补事务，绝不再次覆盖。
            store::receipt(&actual)?
        } else if journal.completed || actual != spec.expected {
            return Err(changed());
        } else {
            let receipt = self
                .local
                .replace_verified(&journal.request()?, &actual)
                .await?;
            actual = recovery::inspect(&*self.local, &spec.target).await?;
            if !actual.has_content(&spec.pin.digest, spec.pin.size)
                || receipt.generation != store::receipt(&actual)?.generation
                || receipt.file_identity != store::receipt(&actual)?.file_identity
            {
                return Err(changed());
            }
            receipt
        };
        if !journal.completed {
            self.store.finish(&journal, &actual).await?;
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests;
