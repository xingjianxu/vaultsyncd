//! 旧设备的保守重建，不是首次导入。入口须按 binding 串行停下事件/执行器后调用。
//!
//! 先把旧操作隔离并持久化旧基线与事件，再访问网络/磁盘。固定快照分页可重启；真实内容
//! 核对复用 S059，unknown 不授权写入。此模块从不上传、删除或改名本地内容。
//! 有恢复候选/未核对条目时保留围栏及旧游标；只有无需本地写入的重建才能原子发布新游标。
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vaultsyncd_client_store::bindings::{self, BindingRecord};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::entries::{self, BaselineEntry, EntryKind, RemoteEntry};
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId};
use vaultsyncd_domain::state::{Dirty, Residency};
use vaultsyncd_domain::version::{Revision, ServerEpoch};
use vaultsyncd_protocol::generated::{
    EntryKind as WireKind, Snapshot, SnapshotEntry, SnapshotPage,
};
use vaultsyncd_transfer::ports::PortError;

use super::{ContentEvidence, inspect};
use crate::bindings::import::{ImportLocal, ImportSnapshot};
use crate::executor::apply::ControlledLocal;

mod store;
#[cfg(test)]
mod tests;

/// 必须由认证协议错误/服务器身份检查触发；不能用空 changes 猜测日志过期。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResyncReason {
    /// 同一 epoch，事件/历史已被回收。历史删除不是本地新增。
    CursorExpired,
    /// 恢复旧服务器备份；相同 seq 不再标识同一事件，旧操作不能重放。
    ServerRollback {
        /// 认证得到的新 epoch，不要求数值递增。
        new_epoch: u64,
    },
}

/// 每条本地路径的保守重建结论，不是可直接执行的命令。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Disposition {
    /// 本地真实内容与新快照相同，可以只更新元数据基线。
    Current,
    /// 历史条目远端已不存在，真实 clean 内容仅保留在本地，禁止作为新增上传。
    RetiredClean,
    /// 健康根已证明本地/远端均不存在，不推导远端 Delete。
    RetiredAbsent,
    /// 远端缺失的本地新内容，或服务器回退后失去确认的完整副本；不复用原 EntryId 上传。
    RecoveryCandidate,
    /// 服务器快照需要本地应用/三方合并；在此入口不覆盖本地。
    Reconcile,
    /// 来源、类型、使用状态或真实内容无法确认，保持原样。
    Review,
}
impl Disposition {
    fn needs_review(&self) -> bool {
        !matches!(
            self,
            Self::Current | Self::RetiredClean | Self::RetiredAbsent
        )
    }
}

/// 持久清单：候选不包含任何自动上传/删除指令。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResyncItem {
    /// 本地相对路径。
    pub path: String,
    /// 已知旧身份；无法证明来源的文件为 None。
    pub old_entry: Option<String>,
    /// 完整真实内容证据；目录/占用/读取故障为 None。
    pub evidence: Option<ContentEvidence>,
    /// 可解释的保留/合并结论。
    pub disposition: Disposition,
    // 与核对同一时刻的 DB 观察；发布时比较全部维度，不能丢失核对后的新 dirty/代际。
    local_stamp: Option<LocalStamp>,
}

/// 重建结果。prepared 不等于 applied；有候选时 complete=false、游标与旧基线不动。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResyncReport {
    /// 持久日志身份，重复调用复用同一日志。
    pub resync_id: i64,
    /// 区分日志过期与服务器备份回退。
    pub reason: ResyncReason,
    /// 完整固定快照高水位；尚未取到快照时为 None。
    pub high_water: Option<u64>,
    /// 已完成完整快照与本地核对。
    pub prepared: bool,
    /// 无待处理候选，已原子切换 epoch/remote/baseline/received/applied。
    pub complete: bool,
    /// 独立于队列的保留/恢复清单。
    pub items: Vec<ResyncItem>,
}

/// 重建编排；仅通过注入端口做平台 I/O，不提供 Linux 产品客户端。
pub struct Resync {
    database: Arc<Database>,
    local: Arc<dyn ImportLocal>,
    content: Arc<dyn ControlledLocal>,
    snapshot: Arc<dyn ImportSnapshot>,
}
impl Resync {
    /// ImportLocal 只用健康根准入与完整扫描；不会调用 preserve/install。
    pub fn new(
        database: Arc<Database>,
        local: Arc<dyn ImportLocal>,
        content: Arc<dyn ControlledLocal>,
        snapshot: Arc<dyn ImportSnapshot>,
    ) -> Self {
        Self {
            database,
            local,
            content,
            snapshot,
        }
    }

    /// 从认证错误进入或续接开放重建。旧操作从第一笔事务起永久隔离；不得并发调度此 binding。
    /// 有待核对清单时重复调用重新读取真实内容，不信任上次的 clean 结论。
    pub async fn run(
        &self,
        binding: BindingId,
        reason: ResyncReason,
    ) -> Result<ResyncReport, PortError> {
        let mut journal = store::start(&self.database, binding, reason).await?;
        self.resume(&mut journal).await
    }

    /// 重启不必保存原始错误或内存参数；完成日志不重新执行。
    pub async fn resume_pending(
        &self,
        binding: BindingId,
    ) -> Result<Option<ResyncReport>, PortError> {
        let Some(mut journal) = store::pending(&self.database, binding).await? else {
            return Ok(None);
        };
        self.resume(&mut journal).await.map(Some)
    }

    /// 供状态/诊断入口读取最近一次持久结果，不重新扫描，不把 prepared 当作 complete。
    pub async fn latest_report(
        &self,
        binding: BindingId,
    ) -> Result<Option<ResyncReport>, PortError> {
        store::latest(&self.database, binding)
            .await
            .map(|journal| journal.map(|j| j.report()))
    }

    async fn resume(&self, journal: &mut Journal) -> Result<ResyncReport, PortError> {
        let mut conn = self.database.pool().acquire().await.map_err(error)?;
        let binding = bindings::binding(&mut conn, journal.binding()?)
            .await
            .map_err(error)?
            .ok_or_else(|| error("绑定不存在"))?;
        drop(conn);
        if binding.server_epoch.get() != journal.old_epoch {
            return Err(error("重建期间绑定 epoch 被修改"));
        }
        let other_paths: Vec<String> = sqlx::query_scalar("SELECT local_path FROM bindings WHERE id <> ? UNION ALL SELECT cache_path FROM bindings")
            .bind(binding.id.as_bytes().as_slice()).fetch_all(self.database.pool()).await.map_err(error)?;
        let admission = self.local.validate(&binding, &other_paths).await?;
        if !admission.healthy
            || !admission.directory
            || !admission.disjoint
            || !admission.representable
            || admission.identity.is_empty()
        {
            return Err(error("重建根准入失败，不能把挂载/扫描故障当删除"));
        }
        if journal
            .root_identity
            .as_ref()
            .is_some_and(|id| *id != admission.identity)
        {
            return Err(error("重建根身份改变"));
        }
        if journal.root_identity.is_none() {
            journal.root_identity = Some(admission.identity);
            store::save(&self.database, journal).await?;
        }
        if journal.snapshot.is_none() {
            let seed = self.snapshot.begin(binding.root).await?;
            validate_snapshot(&seed.metadata, &binding, journal)?;
            journal.snapshot = Some(seed.metadata);
            store::save(&self.database, journal).await?;
        }
        while !journal.snapshot_complete {
            let snapshot = journal.snapshot.as_ref().expect("已保存快照");
            let page = self
                .snapshot
                .page(
                    binding.root,
                    &snapshot.snapshot_id,
                    journal.cursor.as_deref(),
                )
                .await?;
            validate_page(journal, &page)?;
            journal.entries.extend(page.entries);
            journal.cursor = page.next_cursor;
            journal.snapshot_complete = journal.cursor.is_none();
            if journal.snapshot_complete
                && journal.entries.len() as u64 != number(&snapshot.entry_count)?
            {
                return Err(error("固定快照计数不符"));
            }
            store::save(&self.database, journal).await?;
        }
        // 所有文件必须有固定版本/摘要/长度，目录不得携带内容；即便需要人工核对也不能接受坏快照。
        for entry in &journal.entries {
            remote(entry)?;
        }
        paths(&journal.entries)?;
        let old_root = journal
            .old_baseline
            .iter()
            .find(|entry| entry.parent.is_none());
        let new_root = journal
            .entries
            .iter()
            .find(|entry| entry.parent_id.is_none());
        if old_root.map(|e| &e.entry) != new_root.map(|e| &e.entry_id) {
            return Err(error("合成根身份改变，需核对服务器 Root 身份"));
        }
        journal.items = Some(self.classify(&binding, journal).await?);
        store::save(&self.database, journal).await?;
        #[cfg(test)]
        tests::checkpoint("classified", 93);
        if !journal
            .items
            .as_ref()
            .expect("已核对")
            .iter()
            .any(|item| item.disposition.needs_review())
            && !store::has_intents(&self.database, binding.id).await?
        {
            store::finish(&self.database, journal).await?;
        }
        Ok(journal.report())
    }

    async fn classify(
        &self,
        binding: &BindingRecord,
        journal: &Journal,
    ) -> Result<Vec<ResyncItem>, PortError> {
        // 先完整扫描；失败整体报错，不能用空目录代替。单文件核对错误只产生 Review。
        let scanned = self.local.scan(binding.id).await?;
        let mut local_paths = BTreeSet::new();
        for entry in &scanned {
            relative(&entry.path)?;
            if !local_paths.insert(entry.path.clone()) {
                return Err(error("扫描路径重复"));
            }
        }
        for entry in &scanned {
            if let Some((parent, _)) = entry.path.rsplit_once('/') {
                if !scanned
                    .iter()
                    .any(|e| e.path == parent && e.kind == WireKind::Directory)
                {
                    return Err(error("扫描缺父目录，不能作为完整扫描"));
                }
            }
        }
        let remote_paths = paths(&journal.entries)?;
        let all_paths: BTreeSet<_> = local_paths
            .iter()
            .chain(remote_paths.keys())
            .cloned()
            .collect();
        for left in &all_paths {
            let (parent, name) = left.rsplit_once('/').unwrap_or(("", left));
            for right in all_paths.range((
                std::ops::Bound::Excluded(left.clone()),
                std::ops::Bound::Unbounded,
            )) {
                let (other_parent, other_name) = right.rsplit_once('/').unwrap_or(("", right));
                if parent == other_parent && self.local.equivalent(name, other_name) {
                    return Err(error("名称等价冲突，保持重建围栏"));
                }
            }
        }
        let remote_by_id: BTreeMap<_, _> = remote_paths
            .iter()
            .map(|(p, e)| (e.entry_id.as_str(), (p, e)))
            .collect();
        let old_entries: Vec<_> = journal
            .old_baseline
            .iter()
            .map(SavedBase::snapshot)
            .collect();
        // 历史 tombstone 与新身份可以同名，不能按路径把旧身份折叠成“新增”。
        let old_paths = locations(&old_entries)?;
        let mut items = Vec::new();
        for (path, old) in &old_paths {
            local_paths.remove(path);
            let mut conn = self.database.pool().acquire().await.map_err(error)?;
            let local = entries::local_entry(&mut conn, binding.id, id(&old.entry_id)?)
                .await
                .map_err(error)?;
            drop(conn);
            let actual = if old.kind == WireKind::File {
                inspect(&*self.content, &relative(path)?).await.ok()
            } else {
                None
            };
            // 目录只重建元数据，不删除其内容；完整扫描中的未知子项仍独立进入 Review。
            let directory_clean = old.kind == WireKind::Directory
                && local.as_ref().is_some_and(|row| {
                    row.dirty == Dirty::Clean
                        && scanned.iter().any(|entry| {
                            entry.path == *path
                                && entry.kind == WireKind::Directory
                                && entry.generation == row.generation.get()
                        })
                });
            let clean = match (&local, &actual) {
                (
                    Some(local),
                    Some(ContentEvidence::File {
                        generation,
                        identity,
                        digest,
                        ..
                    }),
                ) => {
                    local.dirty == Dirty::Clean
                        && local.residency == Residency::Materialized
                        && local.generation.get() == *generation
                        && local.file_identity.as_ref() == Some(identity)
                        && local.fingerprint.as_deref() == Some(digest.as_slice())
                }
                _ => false,
            };
            let dirty = local.as_ref().is_some_and(|row| row.dirty != Dirty::Clean)
                || matches!((&local, &actual), (Some(row), Some(ContentEvidence::File { digest, .. })) if row.fingerprint.as_deref().is_some_and(|hash| hash != digest.as_slice()));
            let disposition = match remote_by_id.get(old.entry_id.as_str()) {
                None if (clean || directory_clean)
                    && matches!(journal.reason, ResyncReason::ServerRollback { .. }) =>
                {
                    Disposition::RecoveryCandidate
                }
                None if directory_clean => Disposition::RetiredClean,
                None => match &actual {
                    Some(ContentEvidence::File { .. }) if dirty => Disposition::RecoveryCandidate,
                    Some(ContentEvidence::File { .. }) if clean => Disposition::RetiredClean,
                    Some(ContentEvidence::Absent)
                        if local.as_ref().is_none_or(|row| row.dirty == Dirty::Clean) =>
                    {
                        Disposition::RetiredAbsent
                    }
                    _ => Disposition::Review,
                },
                Some((remote_path, remote))
                    if directory_clean
                        && *remote_path == path
                        && remote.kind == WireKind::Directory =>
                {
                    Disposition::Current
                }
                Some((remote_path, remote))
                    if clean
                        && *remote_path == path
                        && matches!(&actual,
                    Some(ContentEvidence::File { digest, size, .. }) if remote.hash.as_deref() == Some(hex(digest).as_str()) && remote.size.as_deref().map(number).transpose()? == Some(*size)) =>
                {
                    Disposition::Current
                }
                Some(_) => Disposition::Reconcile,
            };
            items.push(ResyncItem {
                path: path.clone(),
                old_entry: Some(old.entry_id.clone()),
                evidence: actual,
                disposition,
                local_stamp: local.map(LocalStamp::from),
            });
        }
        // 本地无旧来源，哪怕同路径同内容也不能当作首次导入自动上传。
        for path in local_paths {
            items.push(ResyncItem {
                path,
                old_entry: None,
                evidence: None,
                disposition: Disposition::Review,
                local_stamp: None,
            });
        }
        for (path, remote) in remote_paths {
            if !items
                .iter()
                .any(|item| item.old_entry.as_deref() == Some(&remote.entry_id))
            {
                items.push(ResyncItem {
                    path,
                    old_entry: None,
                    evidence: None,
                    disposition: Disposition::Reconcile,
                    local_stamp: None,
                });
            }
        }
        Ok(items)
    }
}

/// true 表示存在尚未完成的旧基线重建。装配据此跳过旧操作的启动恢复，把续接留给准入代码；
/// 围栏本身仍由 [`ensure_unblocked`] 施加，队列/事件/破坏性动作不会被放行。
pub async fn rebuild_open(database: &Database, binding: BindingId) -> Result<bool, PortError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM binding_resyncs WHERE binding_id = ? AND complete = 0",
    )
    .bind(binding.as_bytes().as_slice())
    .fetch_one(database.pool())
    .await
    .map_err(error)?;
    Ok(count != 0)
}

/// 通用队列/事件/受控应用的持久围栏。不是返回空队列假装成功。
pub(crate) async fn ensure_unblocked(
    database: &Database,
    binding: BindingId,
) -> Result<(), PortError> {
    if rebuild_open(database, binding).await? {
        return Err(error("旧基线重建尚未完成，旧操作与破坏性动作保持隔离"));
    }
    Ok(())
}

pub(crate) async fn ensure_operation(
    database: &Database,
    binding: BindingId,
    op: OpId,
) -> Result<(), PortError> {
    ensure_unblocked(database, binding).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM resync_quarantined_ops WHERE op_id = ?")
            .bind(op.as_bytes().as_slice())
            .fetch_one(database.pool())
            .await
            .map_err(error)?;
    if count != 0 {
        return Err(error("旧设备操作已隔离，不能在新基线重放"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LocalStamp {
    generation: u64,
    dirty: String,
    residency: String,
    fingerprint: Option<Vec<u8>>,
    identity: Option<Vec<u8>>,
}
impl From<entries::LocalEntry> for LocalStamp {
    fn from(row: entries::LocalEntry) -> Self {
        Self {
            generation: row.generation.get(),
            dirty: match row.dirty {
                Dirty::Clean => "clean",
                Dirty::Dirty => "dirty",
                Dirty::Snapshotting => "snapshotting",
                Dirty::AwaitingCommit => "awaiting_commit",
            }
            .to_owned(),
            residency: match row.residency {
                Residency::Materialized => "materialized",
                Residency::Partial => "partial",
                Residency::MetadataOnly => "metadata_only",
            }
            .to_owned(),
            fingerprint: row.fingerprint,
            identity: row.file_identity,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedBase {
    entry: String,
    parent: Option<String>,
    name: String,
    kind: WireKind,
    revision: u64,
    version: Option<String>,
    deleted: bool,
}
impl SavedBase {
    fn new(row: BaselineEntry) -> Self {
        Self {
            entry: row.entry.to_string(),
            parent: row.parent.map(|v| v.to_string()),
            name: row.name,
            kind: if row.kind == EntryKind::File {
                WireKind::File
            } else {
                WireKind::Directory
            },
            revision: row.revision.get(),
            version: row.content_version.map(|v| v.to_string()),
            deleted: row.deleted,
        }
    }
    fn snapshot(&self) -> SnapshotEntry {
        SnapshotEntry {
            modified_at: None,
            entry_id: self.entry.clone(),
            parent_id: self.parent.clone(),
            name: self.name.clone(),
            kind: self.kind,
            revision: self.revision.to_string(),
            version_id: self.version.clone(),
            hash: None,
            size: None,
        }
    }
    fn baseline(&self) -> Result<BaselineEntry, PortError> {
        Ok(BaselineEntry {
            entry: id(&self.entry)?,
            parent: self.parent.as_deref().map(id).transpose()?,
            name: self.name.clone(),
            kind: if self.kind == WireKind::File {
                EntryKind::File
            } else {
                EntryKind::Directory
            },
            revision: Revision::new(self.revision).map_err(error)?,
            content_version: self
                .version
                .as_deref()
                .map(vaultsyncd_domain::ids::VersionId::from_hex)
                .transpose()
                .map_err(error)?,
            deleted: self.deleted,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
struct SavedEvent {
    seq: i64,
    tx: Vec<u8>,
    events: String,
    state: String,
    last_error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Journal {
    format: u32,
    #[serde(skip)]
    resync_id: i64,
    #[serde(skip)]
    revision: i64,
    binding: String,
    reason: ResyncReason,
    old_epoch: u64,
    old_received: i64,
    old_applied: i64,
    old_baseline: Vec<SavedBase>,
    old_remote: Vec<SavedBase>,
    old_events: Vec<SavedEvent>,
    root_identity: Option<String>,
    snapshot: Option<Snapshot>,
    entries: Vec<SnapshotEntry>,
    cursor: Option<String>,
    snapshot_complete: bool,
    items: Option<Vec<ResyncItem>>,
    complete: bool,
}
impl Journal {
    fn binding(&self) -> Result<BindingId, PortError> {
        BindingId::from_hex(&self.binding).map_err(error)
    }
    fn epoch(&self) -> u64 {
        match self.reason {
            ResyncReason::CursorExpired => self.old_epoch,
            ResyncReason::ServerRollback { new_epoch } => new_epoch,
        }
    }
    fn report(&self) -> ResyncReport {
        ResyncReport {
            resync_id: self.resync_id,
            reason: self.reason.clone(),
            high_water: self
                .snapshot
                .as_ref()
                .and_then(|s| s.high_water_seq.parse().ok()),
            prepared: self.items.is_some(),
            complete: self.complete,
            items: self.items.clone().unwrap_or_default(),
        }
    }
}
fn validate_snapshot(
    snapshot: &Snapshot,
    binding: &BindingRecord,
    journal: &Journal,
) -> Result<(), PortError> {
    id(&snapshot.snapshot_id)?;
    if snapshot.root_id != binding.root.to_string()
        || number(&snapshot.server_epoch)? != journal.epoch()
    {
        return Err(error("快照 Root/epoch 不符，不能混合恢复代际"));
    }
    let high_water = number(&snapshot.high_water_seq)?;
    if journal.reason == ResyncReason::CursorExpired && high_water < journal.old_received as u64 {
        return Err(error("同 epoch 快照高水位倒退，不能解释成日志过期"));
    }
    number(&snapshot.entry_count)?;
    number(&snapshot.expires_at)?;
    Ok(())
}
fn validate_page(journal: &Journal, page: &SnapshotPage) -> Result<(), PortError> {
    let snapshot = journal.snapshot.as_ref().expect("快照");
    if page.snapshot_id != snapshot.snapshot_id || page.high_water_seq != snapshot.high_water_seq {
        return Err(error("分页快照身份或高水位变化"));
    }
    let cursor = journal
        .cursor
        .as_deref()
        .map(number)
        .transpose()?
        .unwrap_or(0);
    if page
        .next_cursor
        .as_deref()
        .map(number)
        .transpose()?
        .is_some_and(|next| next <= cursor || page.entries.is_empty())
    {
        return Err(error("分页游标未推进"));
    }
    let mut previous = journal.entries.last().map(|e| e.entry_id.as_str());
    for entry in &page.entries {
        id(&entry.entry_id)?;
        if previous.is_some_and(|old| old >= entry.entry_id.as_str()) {
            return Err(error("分页重复或乱序"));
        }
        previous = Some(&entry.entry_id);
    }
    if journal.entries.len() as u64 + page.entries.len() as u64 > number(&snapshot.entry_count)? {
        return Err(error("快照条目超出计数"));
    }
    Ok(())
}
// 旧基线不含摘要，因此不能用首次导入的内容校验函数；这里只验证身份/路径树，内容另行真实核对。
fn paths(entries: &[SnapshotEntry]) -> Result<BTreeMap<String, SnapshotEntry>, PortError> {
    let mut paths = BTreeMap::new();
    for (path, entry) in locations(entries)? {
        if paths.insert(path, entry).is_some() {
            return Err(error("快照路径重复"));
        }
    }
    Ok(paths)
}
fn locations(entries: &[SnapshotEntry]) -> Result<Vec<(String, SnapshotEntry)>, PortError> {
    let mut by_id = BTreeMap::new();
    for entry in entries {
        id(&entry.entry_id)?;
        Revision::new(number(&entry.revision)?).map_err(error)?;
        if entry.parent_id.is_some() && (entry.name.is_empty() || entry.name.contains(['/', '\\']))
        {
            return Err(error("条目名称不是单一分量"));
        }
        if by_id.insert(entry.entry_id.as_str(), entry).is_some() {
            return Err(error("条目身份重复"));
        }
    }
    let roots: Vec<_> = entries.iter().filter(|e| e.parent_id.is_none()).collect();
    if roots.len() != 1 || roots[0].kind != WireKind::Directory || !roots[0].name.is_empty() {
        return Err(error("重建缺唯一合成根"));
    }
    let mut paths = Vec::new();
    for entry in entries.iter().filter(|e| e.parent_id.is_some()) {
        let mut current = entry;
        let mut names = vec![entry.name.as_str()];
        let mut seen = BTreeSet::from([entry.entry_id.as_str()]);
        while let Some(parent) = current.parent_id.as_deref() {
            if !seen.insert(parent) {
                return Err(error("父目录成环"));
            }
            current = by_id.get(parent).ok_or_else(|| error("缺父目录"))?;
            if current.kind != WireKind::Directory {
                return Err(error("父条目不是目录"));
            }
            if current.parent_id.is_some() {
                names.push(&current.name);
            }
        }
        names.reverse();
        let path = names.join("/");
        relative(&path)?;
        paths.push((path, entry.clone()));
    }
    Ok(paths)
}
fn remote(entry: &SnapshotEntry) -> Result<RemoteEntry, PortError> {
    let base = SavedBase {
        entry: entry.entry_id.clone(),
        parent: entry.parent_id.clone(),
        name: entry.name.clone(),
        kind: entry.kind,
        revision: number(&entry.revision)?,
        version: entry.version_id.clone(),
        deleted: false,
    }
    .baseline()?;
    match entry.kind {
        WireKind::File => {
            if entry.hash.as_deref().is_none_or(|hash| {
                hash.len() != 64
                    || !hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) || entry.size.as_deref().map(number).transpose()?.is_none()
                || base.content_version.is_none()
            {
                return Err(error("快照文件缺完整版本/摘要/大小"));
            }
        }
        WireKind::Directory => {
            if entry.hash.is_some() || entry.size.is_some() || base.content_version.is_some() {
                return Err(error("目录携带文件内容"));
            }
        }
    }
    Ok(RemoteEntry {
        entry: base.entry,
        parent: base.parent,
        name: base.name,
        kind: base.kind,
        revision: base.revision,
        content_version: base.content_version,
        deleted: false,
    })
}
fn id(value: &str) -> Result<EntryId, PortError> {
    EntryId::from_hex(value).map_err(error)
}
fn relative(value: &str) -> Result<vaultsyncd_fs_common::path::RelativePath, PortError> {
    vaultsyncd_fs_common::path::RelativePath::parse(value).map_err(error)
}
fn number(value: &str) -> Result<u64, PortError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(error("非规范计数"));
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| error("计数越界"))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn error(value: impl std::fmt::Display) -> PortError {
    PortError::rejected("NEEDS_RECOVERY", value.to_string())
}
