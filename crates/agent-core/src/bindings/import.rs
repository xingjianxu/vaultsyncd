//! 首次绑定的可恢复合并（design §9.2、T01）。
//!
//! 固定快照先于扫描，完整清单先于任何动作；每个动作的 OpId、改名目标和临时身份均持久化。
//! 上传复用 generation 上传处理器与 expected-absent CAS。恢复只读原日志，不重新分配身份、
//! 不重新取最新快照。没有共同基线的入口不产生删除；有旧基线/事件/队列的绑定拒绝进入。
//! 平台负责真实根准入、完整扫描、幂等保留改名与固定版本安装；这里不实现 Linux 客户端。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vaultsyncd_client_store::bindings::{BindingRecord, SyncMode};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::entries::{BaselineEntry, EntryKind as StoreKind};
use vaultsyncd_domain::ids::{BindingId, EntryId, IdSource, OpId, RootId};
use vaultsyncd_domain::version::{Generation, Revision, RootSeq, UtcTimestamp};
use vaultsyncd_fs_common::name::NameEquivalence;
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_planner::conflict::{
    ConflictNameRequest, NameBudget, ShortIdLen, conflict_entry_path,
};
use vaultsyncd_planner::first_sync::{BindingMode, plan_first_sync};
use vaultsyncd_planner::reconcile::{Action, ContentFingerprint, EntryKind, LocalFact, ServerFact};
use vaultsyncd_protocol::generated::{
    ConflictPolicy, EntryKind as WireKind, OperationKind, OperationRequest, OperationResult,
    Snapshot, SnapshotEntry, SnapshotPage,
};
use vaultsyncd_transfer::ports::{PortError, PortFuture};

use super::store::ImportStore;
use crate::executor::upload::{UploadActionHandler, UploadSpec};
use crate::executor::{ActionHandler, ActionOutcome};
use crate::ports::Ports;

/// 平台根准入证据。适配器必须用打开的根句柄检查真实身份、权限、重解析点与规范路径重叠；
/// 不能仅凭用户路径的字符串前缀宣称准入。失败返回错误，不返回空扫描。
#[derive(Clone, Debug)]
pub struct RootAdmission {
    /// 跨重启稳定的卷/根目录身份；挂载漂移时必须改变或直接报错。
    pub identity: String,
    /// 根健康且可完整扫描。
    pub healthy: bool,
    /// 是支持的普通目录，不是文件、符号链接或占位回源根。
    pub directory: bool,
    /// 与缓存、内部状态和其它绑定均不重叠（按平台规范分量比较）。
    pub disjoint: bool,
    /// 名称与路径能在当前平台无损表示。
    pub representable: bool,
}

/// 扫描完成后的一条事实；文件必须给出大小与 SHA-256，目录均为 None。
/// generation 必须由平台观察器稳定维护，不能用 mtime 代替。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalImportEntry {
    /// 已规范的 Root 内相对路径。
    pub path: String,
    /// 文件/目录。
    pub kind: WireKind,
    /// 本地代际。
    pub generation: u64,
    /// 文件大小。
    pub size: Option<u64>,
    /// 文件整份摘要。
    pub hash: Option<[u8; 32]>,
}

/// 固定版本安装请求。实现必须先持久化本地意图，核对目标仍为空，再下载/校验/原子发布；
/// OnDemand 才可建占位。重放按 op/版本/内容核对，不覆盖扫描之后新建的本地内容。
#[derive(Clone, Debug)]
pub struct ImportInstall {
    /// 幂等本地操作身份。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 目标相对位置。
    pub path: RelativePath,
    /// 固定的远端事实（不能下载当前 path head 替代固定版本）。
    pub remote: SnapshotEntry,
    /// 完整或按需模式。
    pub mode: SyncMode,
}

/// 平台首次导入端口。所有破坏性本地动作都有已持久化身份，适配器不能把故障当成缺失。
pub trait ImportLocal: NameEquivalence + Send + Sync {
    /// 检查根/缓存/内部状态及已绑定路径；other_paths 来自数据库而不是调用者猜测。
    fn validate<'a>(
        &'a self,
        binding: &'a BindingRecord,
        other_paths: &'a [String],
    ) -> PortFuture<'a, Result<RootAdmission, PortError>>;
    /// 完整扫描；任一目录扫描不完整必须返回错误。先元数据、再必要哈希由适配器执行。
    fn scan(&self, binding: BindingId) -> PortFuture<'_, Result<Vec<LocalImportEntry>, PortError>>;
    /// 保留本地内容到另名；须先落意图并按 generation/身份核对，目标存在则不覆盖。
    /// 重放时仅在已记录的同一 op 确认目标确为原内容时返回成功。
    fn preserve<'a>(
        &'a self,
        binding: BindingId,
        op: OpId,
        from: &'a RelativePath,
        to: &'a RelativePath,
        generation: Generation,
    ) -> PortFuture<'a, Result<(), PortError>>;
    /// 安装固定远端版本；只有完整校验/发布（或按需占位注册）完成才能返回本地 generation。
    fn install(&self, request: &ImportInstall) -> PortFuture<'_, Result<Generation, PortError>>;
}

/// 固定快照及经过认证的响应所报告的服务器时间（例如 HTTPS Date）。
/// 时间仅用于首次导入的稳定冲突标签，不冒充尚未发生的提交时间，也不参与内容决胜。
#[derive(Clone, Debug)]
pub struct SnapshotSeed {
    /// 直接复用生成的快照 DTO。
    pub metadata: Snapshot,
    /// 服务器观察时间；不得用客户端时钟或 expires_at 猜测。
    pub server_time: UtcTimestamp,
}

/// 一致性快照端口。每页必须来自 begin 返回的同一个 snapshot_id。
pub trait ImportSnapshot: Send + Sync {
    /// 固定快照，不能用不断变化的目录列表代替。
    fn begin(&self, root: RootId) -> PortFuture<'_, Result<SnapshotSeed, PortError>>;
    /// 读取固定快照的一页；过期必须报错，不悄悄换快照。
    fn page<'a>(
        &'a self,
        root: RootId,
        snapshot: &'a str,
        cursor: Option<&'a str>,
    ) -> PortFuture<'a, Result<SnapshotPage, PortError>>;
}

/// 一次有界推进的状态。Complete 后可启动 S057，从持久化的 high_water 接 changes。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    /// 本次新完成的路径数。
    pub completed: usize,
    /// 原始快照高水位，绝不是上传回执中的最新 root_seq。
    pub high_water: RootSeq,
    /// 清单是否全部完成。
    pub complete: bool,
}

/// 首次绑定编排器。数据库只做短事务，不跨网络/平台 I/O 持写锁。
pub struct FirstImport {
    store: Arc<ImportStore>,
    ports: Ports,
    local: Arc<dyn ImportLocal>,
    snapshot: Arc<dyn ImportSnapshot>,
}

impl FirstImport {
    /// 应用入口组装；不新建平台或 HTTP 实现。
    #[must_use]
    pub fn new(
        database: Arc<Database>,
        ports: Ports,
        local: Arc<dyn ImportLocal>,
        snapshot: Arc<dyn ImportSnapshot>,
    ) -> Self {
        Self {
            store: Arc::new(ImportStore::new(database)),
            ports,
            local,
            snapshot,
        }
    }

    /// 推进已有绑定的首次导入。max_items 限制完整路径数；0 可只固定快照并保存扫描清单。
    /// ids 只在首次构建清单时使用；恢复不依赖其之前的内存状态。
    pub async fn run(
        &self,
        binding_id: BindingId,
        ids: &mut (impl IdSource + Send),
        max_items: usize,
    ) -> Result<ImportReport, PortError> {
        self.store.ensure_not_resyncing(binding_id).await?;
        let binding = self.store.binding(binding_id).await?;
        if binding.mode == SyncMode::OnDemand && !self.ports.local.capabilities().on_demand_read {
            return Err(invalid("当前平台不支持按需模式"));
        }
        let paths = self.store.other_paths(binding_id).await?;
        let admission = self.local.validate(&binding, &paths).await?;
        if admission.identity.is_empty()
            || !admission.healthy
            || !admission.directory
            || !admission.disjoint
            || !admission.representable
        {
            return Err(invalid("本地根不健康、非普通目录、存在重叠或名称不可表示"));
        }
        let mut journal = match self.store.load(binding_id).await? {
            Some(journal) => journal,
            None => {
                self.store.ensure_fresh(binding_id).await?;
                let seed = self.snapshot.begin(binding.root).await?;
                validate_snapshot(&seed.metadata, &binding)?;
                let journal = Journal {
                    format: 1,
                    revision: 0,
                    root_identity: admission.identity.clone(),
                    snapshot: seed.metadata,
                    server_time: seed.server_time.as_unix_seconds(),
                    entries: Vec::new(),
                    cursor: None,
                    snapshot_complete: false,
                    items: None,
                    complete: false,
                };
                self.store.create(binding_id, &journal).await?;
                journal
            }
        };
        validate_snapshot(&journal.snapshot, &binding)?;
        if journal.format != 1 || journal.root_identity != admission.identity {
            return Err(invalid("导入日志版本或本地根身份变化，需恢复现场"));
        }
        if journal.complete {
            return report(&journal, 0);
        }
        while !journal.snapshot_complete {
            let page = self
                .snapshot
                .page(
                    binding.root,
                    &journal.snapshot.snapshot_id,
                    journal.cursor.as_deref(),
                )
                .await?;
            validate_page(&journal, &page)?;
            journal.entries.extend(page.entries);
            journal.cursor = page.next_cursor;
            journal.snapshot_complete = journal.cursor.is_none();
            if journal.snapshot_complete
                && journal.entries.len() as u64 != number(&journal.snapshot.entry_count)?
            {
                return Err(invalid("固定快照条目数不符"));
            }
            self.store.save(binding_id, &mut journal).await?;
        }
        if journal.items.is_none() {
            let local = self.local.scan(binding_id).await?;
            journal.items = Some(plan_manifest(
                &binding,
                &journal.entries,
                local,
                UtcTimestamp::from_unix_seconds(journal.server_time)
                    .map_err(|e| invalid(e.to_string()))?,
                self.local.as_ref(),
                ids,
            )?);
            self.store.prepare(&binding, &mut journal).await?;
        }
        let mut completed = 0;
        let count = journal.items.as_ref().expect("已准备清单").len();
        for index in 0..count {
            if journal.items.as_ref().expect("清单")[index].complete {
                continue;
            }
            if completed >= max_items {
                break;
            }
            let item = journal.items.as_ref().expect("清单")[index].clone();
            if let Some(local) = &item.local {
                if item.source != item.target && !item.preserved {
                    self.local
                        .preserve(
                            binding_id,
                            op_id(&item.op)?,
                            &path(&item.source)?,
                            &path(&item.target)?,
                            generation(local.generation)?,
                        )
                        .await?;
                    journal.items.as_mut().expect("清单")[index].preserved = true;
                    self.store.save(binding_id, &mut journal).await?;
                }
                if item.upload {
                    self.upload(&binding, &item).await?;
                    // ACK 事务可能已由上传处理器提交；重新读持久日志，不能用旧内存覆盖 ACK。
                    journal = self
                        .store
                        .load(binding_id)
                        .await?
                        .ok_or_else(|| invalid("导入日志消失"))?;
                } else {
                    let observed = self.ports.local.observe(&path(&item.target)?).await?;
                    if observed.generation != generation(local.generation)? {
                        return Err(PortError::rejected(
                            "LOCAL_CHANGED",
                            "绑定相同内容前本地已变化",
                        ));
                    }
                }
            }
            let installed = if item.remote.is_some() && (item.local.is_none() || item.upload) {
                Some(
                    self.local
                        .install(&ImportInstall {
                            op: op_id(&item.install_op)?,
                            binding: binding_id,
                            path: path(&item.path)?,
                            remote: item.remote.clone().expect("存在远端"),
                            mode: binding.mode,
                        })
                        .await?,
                )
            } else {
                None
            };
            self.store
                .finish_item(&binding, &mut journal, index, installed)
                .await?;
            completed += 1;
        }
        if journal
            .items
            .as_ref()
            .expect("清单")
            .iter()
            .all(|item| item.complete)
        {
            self.store.finish(&binding, &mut journal).await?;
        }
        report(&journal, completed)
    }

    async fn upload(&self, binding: &BindingRecord, item: &Item) -> Result<(), PortError> {
        if let Some(result) = &item.result {
            if result.conflict.is_some() {
                return Err(invalid("冲突 ACK 已保留，需取得最终路径后恢复"));
            }
            return Ok(());
        }
        let local = item.local.as_ref().expect("上传有本地事实");
        let parent = self.store.resolve_parent(binding.id, &item.target).await?;
        let op = op_id(&item.op)?;
        if local.kind == WireKind::Directory {
            let observed = self.ports.local.observe(&path(&item.target)?).await?;
            if observed.generation != generation(local.generation)? {
                return Err(PortError::rejected(
                    "LOCAL_CHANGED",
                    "新建目录前本地代际变化",
                ));
            }
            let request = OperationRequest {
                base_revision: None,
                base_version_id: None,
                conflict_policy: Some(ConflictPolicy::PreserveBoth),
                device_id: binding.device.to_string(),
                entry_id: None,
                expected_absent: Some(true),
                kind: OperationKind::Create,
                local_generation: Some(local.generation.to_string()),
                name: Some(name(&item.target)?.to_owned()),
                op_id: op.to_string(),
                parent_id: Some(parent.to_string()),
                server_epoch: binding.server_epoch.to_decimal(),
                subtree_token: None,
                upload_id: None,
                version_id: None,
            };
            let result = match self.ports.remote.submit(&request).await {
                Ok(result) => result,
                Err(PortError::Unavailable { .. }) => {
                    self.ports.remote.operation(binding.id, op).await?
                }
                Err(error) => return Err(error),
            };
            self.store.ack(binding.id, op, &result).await?;
        } else {
            let handler = UploadActionHandler::new(
                binding.device.to_string(),
                binding.server_epoch,
                self.store.clone(),
            );
            handler.register(
                op,
                UploadSpec::new(path(&item.target)?, generation(local.generation)?)
                    .with_target(parent, name(&item.target)?)
                    .with_expected_absent(true),
            );
            let queued = self.store.queued(op).await?;
            match handler
                .execute(&self.ports, &queued, None)
                .await
                .map_err(|error| PortError::rejected(error.code.as_str(), error.detail))?
            {
                ActionOutcome::Completed => {}
                ActionOutcome::Pending { .. } => {
                    return Err(PortError::unavailable("首次导入上传仍 pending", None));
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Journal {
    pub format: u32,
    #[serde(skip)]
    pub revision: i64,
    pub root_identity: String,
    pub snapshot: Snapshot,
    pub server_time: i64,
    pub entries: Vec<SnapshotEntry>,
    pub cursor: Option<String>,
    pub snapshot_complete: bool,
    pub items: Option<Vec<Item>>,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Item {
    pub path: String,
    pub source: String,
    pub target: String,
    pub temporary: Option<String>,
    pub op: String,
    pub install_op: String,
    pub local: Option<LocalImportEntry>,
    pub remote: Option<SnapshotEntry>,
    pub upload: bool,
    pub preserved: bool,
    pub result: Option<OperationResult>,
    pub complete: bool,
}

fn plan_manifest(
    binding: &BindingRecord,
    entries: &[SnapshotEntry],
    local: Vec<LocalImportEntry>,
    server_time: UtcTimestamp,
    names: &dyn NameEquivalence,
    ids: &mut impl IdSource,
) -> Result<Vec<Item>, PortError> {
    let remote = remote_paths(entries)?;
    let mut locals = BTreeMap::new();
    for entry in local {
        local_fact(&entry)?;
        if locals.insert(entry.path.clone(), entry).is_some() {
            return Err(invalid("本地扫描路径重复"));
        }
    }
    validate_tree(locals.iter().map(|(p, e)| (p.as_str(), e.kind)))?;
    let mut occupied: BTreeSet<String> = remote.keys().chain(locals.keys()).cloned().collect();
    validate_names(&occupied, names)?;
    let mut paths: Vec<String> = occupied.iter().cloned().collect();
    paths.sort_by_key(|value| (value.split('/').count(), value.clone()));
    let mut remaps: Vec<(String, String)> = Vec::new();
    let mut items = Vec::new();
    let mut identities = BTreeSet::new();
    for original in paths {
        let local = locals.get(&original).cloned();
        let source = remap(&original, &remaps);
        let matching_remote = remote
            .get(&original)
            .filter(|_| local.is_none() || source == original)
            .cloned();
        if local.is_none() && matching_remote.is_none() {
            continue;
        }
        let lf = local.as_ref().map(local_fact).transpose()?;
        let rf = matching_remote
            .as_ref()
            .map(|entry| remote_fact(&original, entry))
            .transpose()?;
        let plan = plan_first_sync(
            lf.as_ref(),
            rf.as_ref(),
            match binding.mode {
                SyncMode::Full => BindingMode::Full,
                SyncMode::OnDemand => BindingMode::OnDemand,
            },
        )
        .map_err(|error| invalid(error.to_string()))?;
        let op = OpId::generate(ids);
        let install_op = OpId::generate(ids);
        let temporary = local.as_ref().map(|_| EntryId::generate(ids).to_string());
        for identity in [
            Some(op.to_string()),
            Some(install_op.to_string()),
            temporary.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if !identities.insert(identity) {
                return Err(invalid("身份源产生重复身份"));
            }
        }
        let mut target = source.clone();
        let upload = match plan.action {
            Action::UploadNewEntry { .. } => true,
            Action::PreserveBoth(_) => {
                // 首次导入提前固定服务器观察时间作为标签；真正的去重身份是完整 OpId。
                // 检查扫描/快照中的所有名称，仍有并发撞名则由 create CAS 拒绝。
                target = conflict_entry_path(
                    &path(&source)?,
                    &ConflictNameRequest {
                        device_label: &binding.device.to_string()[..8],
                        op_id: op,
                        server_time,
                        budget: NameBudget::CROSS_PLATFORM,
                        short_id_len: ShortIdLen::new(32).map_err(|e| invalid(e.to_string()))?,
                    },
                )
                .map_err(|error| invalid(error.to_string()))?
                .to_string();
                if !occupied.insert(target.clone()) {
                    return Err(invalid("冲突保留名称已占用"));
                }
                if local
                    .as_ref()
                    .is_some_and(|e| e.kind == WireKind::Directory)
                {
                    remaps.push((original.clone(), target.clone()));
                }
                true
            }
            Action::BindRemoteEntry { .. } | Action::CreateFromRemote { .. } => false,
            _ => return Err(invalid("首次合并事实不足，拒绝把未执行的计划标为完成")),
        };
        items.push(Item {
            path: original,
            source,
            target,
            temporary,
            op: op.to_string(),
            install_op: install_op.to_string(),
            local,
            remote: matching_remote,
            upload,
            preserved: false,
            result: None,
            complete: false,
        });
    }
    for item in &items {
        occupied.insert(item.target.clone());
    }
    validate_names(&occupied, names)?;
    Ok(items)
}

fn validate_names(paths: &BTreeSet<String>, names: &dyn NameEquivalence) -> Result<(), PortError> {
    let mut siblings: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for value in paths {
        let (parent, name) = value.rsplit_once('/').unwrap_or(("", value.as_str()));
        let previous = siblings.entry(parent).or_default();
        if previous.iter().any(|other| names.equivalent(other, name)) {
            return Err(PortError::rejected(
                "INVALID_NAME",
                "清单含平台等价但拼写不同的名称，不能静默改名或覆盖",
            ));
        }
        previous.push(name);
    }
    Ok(())
}

fn remap(original: &str, remaps: &[(String, String)]) -> String {
    for (from, to) in remaps.iter().rev() {
        if let Some(suffix) = original.strip_prefix(&format!("{from}/")) {
            return format!("{to}/{suffix}");
        }
    }
    original.to_owned()
}

pub(super) fn remote_paths(
    entries: &[SnapshotEntry],
) -> Result<BTreeMap<String, SnapshotEntry>, PortError> {
    let mut by_id = BTreeMap::new();
    for entry in entries {
        EntryId::from_hex(&entry.entry_id).map_err(|e| invalid(e.to_string()))?;
        Revision::new(number(&entry.revision)?).map_err(|e| invalid(e.to_string()))?;
        if by_id.insert(entry.entry_id.clone(), entry).is_some() {
            return Err(invalid("快照条目身份重复"));
        }
    }
    let roots: Vec<_> = entries
        .iter()
        .filter(|entry| entry.parent_id.is_none())
        .collect();
    if roots.len() != 1 || roots[0].kind != WireKind::Directory || !roots[0].name.is_empty() {
        return Err(invalid("快照必须有唯一合成根目录"));
    }
    if roots[0].hash.is_some() || roots[0].size.is_some() || roots[0].version_id.is_some() {
        return Err(invalid("合成根目录不得携带文件内容"));
    }
    let mut paths = BTreeMap::new();
    for entry in entries.iter().filter(|entry| entry.parent_id.is_some()) {
        let mut names = vec![entry.name.clone()];
        let mut current = entry;
        let mut seen = BTreeSet::new();
        seen.insert(entry.entry_id.as_str());
        while let Some(parent) = current.parent_id.as_ref() {
            if !seen.insert(parent.as_str()) {
                return Err(invalid("快照父目录成环"));
            }
            current = by_id.get(parent).ok_or_else(|| invalid("快照缺父目录"))?;
            if current.kind != WireKind::Directory {
                return Err(invalid("快照父条目不是目录"));
            }
            if current.parent_id.is_some() {
                names.push(current.name.clone());
            }
        }
        names.reverse();
        let value = names.join("/");
        remote_fact(&value, entry)?;
        if paths.insert(value, entry.clone()).is_some() {
            return Err(invalid("快照同路径重复"));
        }
    }
    Ok(paths)
}

fn validate_tree<'a>(entries: impl Iterator<Item = (&'a str, WireKind)>) -> Result<(), PortError> {
    let map: BTreeMap<_, _> = entries.collect();
    for value in map.keys() {
        if let Some((parent, _)) = value.rsplit_once('/') {
            if map.get(parent) != Some(&WireKind::Directory) {
                return Err(invalid("本地扫描缺父目录或类型错误"));
            }
        }
    }
    Ok(())
}

fn local_fact(entry: &LocalImportEntry) -> Result<LocalFact, PortError> {
    Ok(LocalFact {
        path: path(&entry.path)?,
        kind: fact_kind(entry.kind, entry.size, entry.hash)?,
        generation: generation(entry.generation)?,
        observed_mtime: None,
    })
}
fn remote_fact(value: &str, entry: &SnapshotEntry) -> Result<ServerFact, PortError> {
    let kind = fact_kind(
        entry.kind,
        entry.size.as_deref().map(number).transpose()?,
        entry.hash.as_deref().map(parse_hash).transpose()?,
    )?;
    let version = entry
        .version_id
        .as_deref()
        .map(vaultsyncd_domain::ids::VersionId::from_hex)
        .transpose()
        .map_err(|e| invalid(e.to_string()))?;
    if (entry.kind == WireKind::File) != version.is_some() {
        return Err(invalid("快照文件必须有固定版本，目录不得有版本"));
    }
    Ok(ServerFact {
        path: path(value)?,
        kind,
        entry_id: entry_id(&entry.entry_id)?,
        revision: Revision::new(number(&entry.revision)?).map_err(|e| invalid(e.to_string()))?,
        version,
        observed_mtime: None,
    })
}
fn fact_kind(
    kind: WireKind,
    size: Option<u64>,
    hash: Option<[u8; 32]>,
) -> Result<EntryKind, PortError> {
    match (kind, size, hash) {
        (WireKind::Directory, None, None) => Ok(EntryKind::Directory),
        (WireKind::File, Some(size), Some(hash)) => {
            Ok(EntryKind::File(ContentFingerprint::new(size, hash)))
        }
        _ => Err(invalid("文件缺大小/摘要，或目录带文件内容")),
    }
}
fn validate_snapshot(snapshot: &Snapshot, binding: &BindingRecord) -> Result<(), PortError> {
    entry_id(&snapshot.snapshot_id)?;
    if snapshot.root_id != binding.root.to_string()
        || snapshot.server_epoch != binding.server_epoch.to_decimal()
    {
        return Err(invalid("快照 Root 或 epoch 与绑定不符"));
    }
    number(&snapshot.entry_count)?;
    number(&snapshot.expires_at)?;
    RootSeq::new(number(&snapshot.high_water_seq)?).map_err(|e| invalid(e.to_string()))?;
    Ok(())
}
fn validate_page(journal: &Journal, page: &SnapshotPage) -> Result<(), PortError> {
    if page.snapshot_id != journal.snapshot.snapshot_id
        || page.high_water_seq != journal.snapshot.high_water_seq
    {
        return Err(invalid("快照分页混入不同身份或高水位"));
    }
    let current = journal
        .cursor
        .as_deref()
        .map(number)
        .transpose()?
        .unwrap_or(0);
    let next = page.next_cursor.as_deref().map(number).transpose()?;
    if next.is_some_and(|next| next <= current || page.entries.is_empty()) {
        return Err(invalid("快照分页游标未推进"));
    }
    let mut previous = journal.entries.last().map(|entry| entry.entry_id.as_str());
    for entry in &page.entries {
        entry_id(&entry.entry_id)?;
        if previous.is_some_and(|id| id >= entry.entry_id.as_str()) {
            return Err(invalid("快照分页身份乱序或重复"));
        }
        previous = Some(&entry.entry_id);
    }
    if journal.entries.len() as u64 + page.entries.len() as u64
        > number(&journal.snapshot.entry_count)?
    {
        return Err(invalid("快照条目超出固定计数"));
    }
    Ok(())
}
fn report(journal: &Journal, completed: usize) -> Result<ImportReport, PortError> {
    Ok(ImportReport {
        completed,
        high_water: RootSeq::new(number(&journal.snapshot.high_water_seq)?)
            .map_err(|e| invalid(e.to_string()))?,
        complete: journal.complete,
    })
}

pub(super) fn baseline(entry: &SnapshotEntry) -> Result<BaselineEntry, PortError> {
    Ok(BaselineEntry {
        entry: entry_id(&entry.entry_id)?,
        parent: entry.parent_id.as_deref().map(entry_id).transpose()?,
        name: entry.name.clone(),
        kind: match entry.kind {
            WireKind::File => StoreKind::File,
            WireKind::Directory => StoreKind::Directory,
        },
        revision: Revision::new(number(&entry.revision)?).map_err(|e| invalid(e.to_string()))?,
        content_version: entry
            .version_id
            .as_deref()
            .map(vaultsyncd_domain::ids::VersionId::from_hex)
            .transpose()
            .map_err(|e| invalid(e.to_string()))?,
        deleted: false,
    })
}
pub(super) fn path(value: &str) -> Result<RelativePath, PortError> {
    RelativePath::parse(value).map_err(|e| invalid(e.to_string()))
}
pub(super) fn name(value: &str) -> Result<&str, PortError> {
    value
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("路径缺名称"))
}
pub(super) fn entry_id(value: &str) -> Result<EntryId, PortError> {
    EntryId::from_hex(value).map_err(|e| invalid(e.to_string()))
}
pub(super) fn op_id(value: &str) -> Result<OpId, PortError> {
    OpId::from_hex(value).map_err(|e| invalid(e.to_string()))
}
pub(super) fn generation(value: u64) -> Result<Generation, PortError> {
    Generation::new(value).map_err(|e| invalid(e.to_string()))
}
pub(super) fn number(value: &str) -> Result<u64, PortError> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid("非规范十进制计数"));
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| invalid("计数越界"))
}
fn parse_hash(value: &str) -> Result<[u8; 32], PortError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("非法 SHA-256"));
    }
    let mut digest = [0; 32];
    for (i, byte) in digest.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|e| invalid(e.to_string()))?;
    }
    Ok(digest)
}
pub(super) fn invalid(detail: impl Into<String>) -> PortError {
    PortError::rejected("NEEDS_RECOVERY", detail)
}

#[cfg(test)]
mod tests;
