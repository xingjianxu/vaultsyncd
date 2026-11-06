//! 持久绑定控制：暂停、模式任务、排除与无删除解绑（design §10.4、T30）。
//!
//! 每个绑定的入口必须串行调度。`execution_gate` 在启动新动作前调用；排除规则不能转成
//! Delete。已经在途的受控操作仍按 S056/S059 的 generation 围栏安全收尾。解绑只停止管理，
//! 全部基线、队列、缓存与本地字节均保留；清理是另一个显式操作。
use std::sync::Arc;

use vaultsyncd_client_store::bindings::SyncMode;
use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::entries::LocalEntry;
use vaultsyncd_domain::ids::{BindingId, EntryId, VersionId};
use vaultsyncd_domain::state::{Dirty, Residency};
use vaultsyncd_domain::version::Revision;
use vaultsyncd_fs_common::ignore::IgnoreRules;
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_transfer::ports::{PortError, PortFuture};

mod store;
use store::{Item, Store};

/// 启动新动作的门禁；非 Ready 时保留队列并停止启动。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionGate {
    /// 绑定与目标可参与同步。
    Ready,
    /// 用户暂停。
    Paused,
    /// 已解除绑定，不能用 Resume 隐式重新绑定。
    Unbound,
    /// 目标或其祖先被排除，保留本地与远端内容。
    Excluded,
    /// 取消排除或未知路径必须先重新协调，不能上传旧文件或推导删除。
    ReconcileRequired,
}

/// full 模式一次补齐/核对的固定目标。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FullFile {
    /// 所属绑定。
    pub binding: BindingId,
    /// 条目身份。
    pub entry: EntryId,
    /// 经纯路径规则验证的位置。
    pub path: RelativePath,
    /// 当前远端固定版本；本地独有内容为 None。
    pub version: Option<VersionId>,
    /// 当前远端修订；本地独有内容为 None。
    pub revision: Option<Revision>,
    /// 当前本地事实；脏内容必须保留，不能为了补齐覆盖它。
    pub local: Option<LocalEntry>,
}

/// 平台和 S059 下载/安装执行器的组装缝；核心不做平台 I/O。
pub trait FullMaterializer: Send + Sync {
    /// 缺失/部分内容必须用固定版本与 S059 受控安装；已有完整内容须实际读取并核对
    /// generation、稳定身份与摘要，不能只信数据库 clean。返回核对后的数据库本地行。
    /// dirty/snapshotting/awaiting_commit 仅能核对保留，不得覆盖或清脏；平台未知/占用须失败。
    fn ensure_full<'a>(
        &'a self,
        file: &'a FullFile,
    ) -> PortFuture<'a, Result<LocalEntry, PortError>>;
}

/// 模式任务进度；complete 只在所有未排除文件均有当前证据时为真。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeProgress {
    /// 目标模式。
    pub mode: SyncMode,
    /// 本次已核对的文件数。
    pub verified: usize,
    /// 当前仍需补齐/重新核对的文件数。
    pub remaining: usize,
    /// 已完成，可宣告 full 离线完整；full→on_demand 不回收内容。
    pub complete: bool,
}

/// 解绑时保留数据的恢复提示；不给出隐式删除/缓存清理选项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnbindReport {
    /// 有未确认内容的条目（包含 snapshotting/awaiting_commit）。
    pub unconfirmed: Vec<EntryId>,
    /// 非完整驻留条目，解绑后不能承诺离线可读。
    pub incomplete: Vec<EntryId>,
    /// 可以先 ExportRecovery，再保留数据停止管理。
    pub offer_export_recovery: bool,
}

/// 绑定生命周期应用服务，短事务状态与平台动作分离。
pub struct BindingControls {
    store: Store,
}
impl BindingControls {
    /// 使用客户端数据库组装；不会操作同步目录。
    pub fn new(database: Arc<Database>) -> Self {
        Self {
            store: Store { database },
        }
    }

    /// 持久暂停/恢复。已经解绑时恢复失败，队列与意图始终保留。
    pub async fn set_paused(&self, binding: BindingId, paused: bool) -> Result<(), PortError> {
        self.store.set_paused(binding, paused).await
    }

    /// 启动/替换模式任务。OnDemand 立即完成且保留全部已下载内容；Full 的任务先落库，
    /// 由 resume_mode 按预算补齐。旧证据不跨模式任务复用。
    pub async fn change_mode(&self, binding: BindingId, mode: SyncMode) -> Result<(), PortError> {
        self.store.change_mode(binding, mode).await
    }

    /// 每次启动新传输/命令前查询；entry=None 且存在排除规则时保守阻塞，调用方必须解析目标。
    pub async fn execution_gate(
        &self,
        binding: BindingId,
        entry: Option<EntryId>,
    ) -> Result<ExecutionGate, PortError> {
        let snapshot = self.store.snapshot(binding).await?;
        if snapshot.control.detached {
            return Ok(ExecutionGate::Unbound);
        }
        if snapshot.control.paused {
            return Ok(ExecutionGate::Paused);
        }
        let Some(entry) = entry else {
            return Ok(
                if snapshot.control.rules.is_empty() && snapshot.participation.is_empty() {
                    ExecutionGate::Ready
                } else {
                    ExecutionGate::ReconcileRequired
                },
            );
        };
        if let Some(item) = snapshot
            .items
            .iter()
            .find(|item| item.id == *entry.as_bytes())
        {
            if excluded(&snapshot.control.rules, item)? {
                return Ok(ExecutionGate::Excluded);
            }
            // A directory operation may act recursively. An excluded descendant is never
            // evidence that the whole remote directory should disappear.
            if !item.file {
                let prefix = if item.path.is_empty() {
                    String::new()
                } else {
                    format!("{}/", item.path)
                };
                for child in snapshot
                    .items
                    .iter()
                    .filter(|child| child.id != item.id && child.path.starts_with(&prefix))
                {
                    if excluded(&snapshot.control.rules, child)?
                        || snapshot.participation.contains_key(&child.id)
                    {
                        return Ok(ExecutionGate::ReconcileRequired);
                    }
                }
            }
        } else if !snapshot.control.rules.is_empty() {
            return Ok(ExecutionGate::ReconcileRequired);
        }
        Ok(match snapshot.participation.get(entry.as_bytes()) {
            Some(state) if state == "excluded" => ExecutionGate::ReconcileRequired,
            Some(_) => ExecutionGate::ReconcileRequired,
            None => ExecutionGate::Ready,
        })
    }

    /// 替换完整的绑定级规则文本（可编辑默认规则由调用方显式传入）。先解析再事务保存；
    /// 取消排除标记 reconcile_required，必须通过最新远端/本地/基线三方核对才能清除。
    pub async fn change_rules(
        &self,
        binding: BindingId,
        version: &str,
        text: &str,
    ) -> Result<(), PortError> {
        IgnoreRules::parse(text).map_err(db)?;
        if version.is_empty() || version.chars().any(char::is_control) {
            return Err(rejected("INVALID_RULES", "规则版本必须非空且不含控制字符"));
        }
        self.store.change_rules(binding, version, text).await
    }

    /// 仅在调用方已完成三方重协调后确认。版本 CAS 防止旧协调结果放开新规则。
    pub async fn acknowledge_reconciled(
        &self,
        binding: BindingId,
        entry: EntryId,
        rules_version: &str,
    ) -> Result<(), PortError> {
        self.store.reconciled(binding, entry, rules_version).await
    }

    /// 读取尚未清理的字节与驻留风险，用于显式解绑前的恢复选择提示。
    pub async fn preview_unbind(&self, binding: BindingId) -> Result<UnbindReport, PortError> {
        self.store.unbind_report(binding).await
    }

    /// 默认保留数据解绑；只写 detached/paused，不删绑定行、不改本地/服务器内容、不生成 Delete。
    /// 返回与停止管理同一事务中读取的恢复提示。缓存/占位清理不属于本入口。
    pub async fn unbind_keep_data(&self, binding: BindingId) -> Result<UnbindReport, PortError> {
        self.store.unbind(binding).await
    }

    /// 读取当前任务；即使曾完成，远端版本/本地代际变化也会使 full 完整性失效。
    pub async fn mode_progress(&self, binding: BindingId) -> Result<ModeProgress, PortError> {
        self.store.progress(binding).await
    }

    /// 有界执行 full 任务；每个成功文件后保存证据，重启不会丢失已完成项。平台失败保留任务。
    /// 每次外部动作后 CAS 规则/暂停/模式/namespace 与本地事实，变化时拒绝完成。
    pub async fn resume_mode(
        &self,
        binding: BindingId,
        materializer: &dyn FullMaterializer,
        budget: usize,
    ) -> Result<ModeProgress, PortError> {
        let snapshot = self.store.snapshot(binding).await?;
        snapshot.ensure_active()?;
        if snapshot.mode == SyncMode::OnDemand {
            return self.store.progress(binding).await;
        }
        let mut completed = 0;
        for item in &snapshot.items {
            if !item.file || item.deleted || excluded(&snapshot.control.rules, item)? {
                continue;
            }
            if snapshot.participation.contains_key(&item.id) {
                continue;
            }
            let stamp = item.stamp()?;
            if snapshot.proofs.get(&item.id) == Some(&stamp) {
                continue;
            }
            if completed >= budget {
                break;
            }
            let file = item.full_file(binding)?;
            // Never give an adapter permission to replace unconfirmed content.
            if file.local.as_ref().is_some_and(|local| {
                local.dirty != Dirty::Clean && local.residency != Residency::Materialized
            }) {
                return Err(rejected(
                    "LOCAL_CHANGED",
                    "未确认的非完整内容必须先恢复，不能用远端覆盖",
                ));
            }
            let verified = materializer.ensure_full(&file).await?;
            if verified.entry != file.entry
                || verified.residency != Residency::Materialized
                || verified.fingerprint.as_ref().is_none_or(Vec::is_empty)
                || verified.file_identity.as_ref().is_none_or(Vec::is_empty)
            {
                return Err(rejected("LOCAL_CHANGED", "未取得完整内容与稳定身份的证明"));
            }
            if let Some(before) = &file.local {
                if before.dirty != Dirty::Clean && *before != verified {
                    return Err(rejected("LOCAL_CHANGED", "模式任务不能修改未确认本地事实"));
                }
            }
            self.store
                .certify(binding, &snapshot, item, &verified)
                .await?;
            completed += 1;
        }
        self.store
            .finish_mode(binding, snapshot.control.revision)
            .await
    }
}

fn excluded(rules: &str, item: &Item) -> Result<bool, PortError> {
    if item.path.is_empty() {
        return Ok(false);
    }
    let path = RelativePath::parse(&item.path).map_err(db)?;
    Ok(IgnoreRules::parse(rules)
        .map_err(db)?
        .is_excluded(&path, !item.file))
}
fn rejected(code: &str, detail: &str) -> PortError {
    PortError::rejected(code, detail)
}
fn db(error: impl std::fmt::Display) -> PortError {
    PortError::local(
        vaultsyncd_transfer::ports::LocalErrorKind::Read,
        error.to_string(),
    )
}

#[cfg(test)]
mod tests;
