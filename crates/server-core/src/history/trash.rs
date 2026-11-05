//! 回收站恢复：把回收站清单里的一个条目（目录则连同全部后代）放回命名空间
//! （design §10.3、T13、I06）。
//!
//! 与历史版本恢复共用同一套提交协调（Root 票据、意图、受控发布、单一事务），差别在“内容从哪来”、
//! “实际位置怎么定”和“有多少个条目”：
//!
//! 1. **清单即事实**：恢复只按回收站清单里的原父目录、原路径、删除时 revision 与内容版本进行；
//!    清单已不在（被恢复过/内容过期回收）就报 `restore_not_in_trash`，不猜一条路径；
//! 2. **不覆盖现在的新文件**（T13）：根的名称已被**活跃**条目占用时，在同一父目录里另名恢复
//!    （`名称（恢复）.扩展名`、`名称（恢复 2）.扩展名`…），后代跟着根整体移位，实际名称与路径放进
//!    结果；事务里再核对一次占用，并发新建同样只会让本次恢复失败，不会覆盖；
//! 3. **目录放回位置、文件放回内容**：先按路径深度把目录 `mkdir` 回来（目标必须空缺，不把清单内容
//!    合并进别人的目录），再逐个文件走“内容租约 → 现场空缺 → 意图 `prepared` → 发布 → `published`”；
//! 4. **单一事务**：整棵子树清软删除、改回父目录与名称、推进 revision、删清单行、事件批次、操作
//!    结果与这批意图的收尾一次提交；事务没提交就没有 ACK。任一文件失败就整批停下：事务从未开始，
//!    旧条目与内容对象都还在，已建的目录、已发布的工作文件与已持久化的意图留作恢复证据
//!    （`fs_intents`），不返回部分成功；
//! 5. **服务端派生意图身份与暂存名**：每个被发布文件的意图身份由请求里的种子按发布序号派生
//!    （[`intent_for`]），暂存名由意图身份派生（[`staging_name`]）——同一 OpId 重试得到同一组身份与
//!    名字（可重入），不依赖客户端给暂存路径。
//!
//! 恢复不改条目身份：清单里的 `EntryId` 恢复后不变，客户端因此不会看到一个“新条目”。

use vaultsyncd_domain::ids::{EntryId, IntentId, OpId, RootId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use crate::commit::intent::{NewPutIntent, TargetAfter, TargetBefore, publish_put};
use crate::commit::put::{RootCommitGate, target_name, validate_name, validate_target_path};
use crate::history::{
    LeaseDisposition, RestoreFailure, RestoreFailureReason, reclassify_publish_failure,
    release_after_failure, release_all,
};
use crate::ports::conflict_store::{OperationKey, RequestDigest};
use crate::ports::content_lease::{
    ContentLease, ContentLeasePort, ContentLeasePurpose, ContentLeaseRequest,
};
use crate::ports::intent_log::IntentLog;
use crate::ports::namespace::{NamespacePort, PathState};
use crate::ports::put_store::EntryKind;
use crate::ports::restore_store::{
    RecordedTrashRestore, RestoreBegin, RestoreStoreError, TrashRestoreEntry, TrashRestoreStore,
    TrashRestoreWrite, TrashSubtree,
};
use crate::ports::target::TargetPort;

/// 另名恢复的尝试上限：`名称（恢复）`、`名称（恢复 2）`… 到此为止仍被占用就报
/// `restore_name_unavailable`，让客户端刷新事实后重新发起，不无限试下去。
pub const MAX_RESTORE_NAME_ATTEMPTS: u32 = 32;

/// 一次回收站恢复的请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashRestoreRequest {
    /// 意图种子：本次恢复为每个被发布的文件派生一个意图身份（[`intent_for`]），重试必须复用同一个
    /// 种子——派生出的意图身份与暂存名因此稳定可重入。目录没有内容，不占意图。
    pub intent_seed: IntentId,
    /// 幂等键：同一 `OpId` 与同一规范化请求只产生一次恢复（design I06）。
    pub op: OperationKey,
    /// 本次请求的规范化摘要（必须覆盖被恢复条目与清单 revision）。
    pub digest: RequestDigest,
    /// 回收站里的条目（文件或目录子树根）。
    pub entry: EntryId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端看到的清单 revision（删除时的 revision）；与清单不一致说明列的是旧事实。
    pub expected_revision: Revision,
}

/// 一次回收站恢复的业务结果（首次提交与重放共用，也是 `operations` 里的终态结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedTrashRestore {
    /// 产生该结果的逻辑操作身份。
    pub op: OpId,
    /// 所属 Root。
    pub root: RootId,
    /// 被恢复的条目（身份不变）。
    pub entry: EntryId,
    /// 恢复后的实际 Root 内路径（原位置被占用时与原路径不同，T13）。
    pub path: String,
    /// 恢复后的实际名称。
    pub name: String,
    /// 是否另名恢复。
    pub renamed: bool,
    /// 条目的新 revision（严格大于清单里的 revision）。
    pub revision: Revision,
    /// 该批次占用的 RootSeq。
    pub seq: RootSeq,
    /// 放回命名空间的条目数。
    pub entries: u64,
}

/// 一次成功的回收站恢复：业务结果 + 内容租约的处理方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredTrash {
    /// 业务结果。
    pub result: CommittedTrashRestore,
    /// 本批次全部内容租约的处理方式（目录恢复没有内容租约，恒为 [`LeaseDisposition::Released`]；
    /// 有租约没能解除时报告最晚的保护时刻）。
    pub lease: LeaseDisposition,
}

/// 回收站恢复要用的端口集合：恢复同时要动工作目录、持久意图、租约、幂等记录。
pub struct TrashRestorePorts<'a, S: ?Sized, L: ?Sized, T: ?Sized, N: ?Sized, C: ?Sized> {
    /// 回收站清单、占用检查与元数据事务。
    pub store: &'a mut S,
    /// 发布意图。
    pub log: &'a mut L,
    /// 工作目录文件发布。
    pub target: &'a mut T,
    /// 工作目录目录创建。
    pub namespace: &'a mut N,
    /// 内容租约。
    pub leases: &'a mut C,
}

/// 恢复一个回收站条目：文件放回内容，目录连同后代整棵放回位置。
///
/// 顺序：Root 票据 → epoch → `OpId` 记录 → 清单与保留期 → 实际名称（根被占用则另名）→ 整棵子树
/// 规划（结构不一致在这里停写）→ 登记 pending → 按深度建目录 → 逐文件租约/意图/发布 → 单一元数据
/// 事务 → 核对证据 → 解除租约 → ACK。
///
/// # Errors
///
/// 见 [`RestoreFailure`]：Root 忙、epoch 变化、同一 `OpId` 不同请求、记录仍是 pending、清单不存在或
/// 已过期、父目录不可用、没有可用另名、清单与事实不一致（`store_contract_violation`）、
/// 租约/意图/命名空间/发布/存储失败。失败都不是 committed：元数据事务从未开始（或整体未提交），
/// 已建的目录、已发布的工作文件与已持久化的意图保留为恢复证据。
pub async fn restore_trash<S, L, T, N, C>(
    gate: &mut RootCommitGate,
    ports: &mut TrashRestorePorts<'_, S, L, T, N, C>,
    at: UtcTimestamp,
    request: &TrashRestoreRequest,
) -> Result<RestoredTrash, Box<RestoreFailure>>
where
    S: TrashRestoreStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    N: NamespacePort + ?Sized,
    C: ContentLeasePort + ?Sized,
{
    let ticket = gate
        .acquire(request.op.root)
        .map_err(|error| Box::new(RestoreFailure::new(RestoreFailureReason::Gate(error))))?;
    let outcome = restore_under_ticket(ports, at, request).await;
    gate.release(ticket);
    outcome
}

async fn restore_under_ticket<S, L, T, N, C>(
    ports: &mut TrashRestorePorts<'_, S, L, T, N, C>,
    at: UtcTimestamp,
    request: &TrashRestoreRequest,
) -> Result<RestoredTrash, Box<RestoreFailure>>
where
    S: TrashRestoreStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    N: NamespacePort + ?Sized,
    C: ContentLeasePort + ?Sized,
{
    // 1. epoch：恢复过的服务器不是同一套事实。
    let facts = ports
        .store
        .root_facts(request.op.root)
        .await
        .map_err(store_failure)?;
    if facts.epoch != request.expected_epoch {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::EpochChanged {
                expected: request.expected_epoch,
                actual: facts.epoch,
            },
        )));
    }

    // 2. 幂等记录先于清单判定：重放自己已提交的结果时清单早已被删掉。
    if let Some(recorded) = ports
        .store
        .operation(request.op)
        .await
        .map_err(store_failure)?
    {
        return resolve_recorded(recorded, request);
    }

    // 3. 清单：必须还在回收站里、未过期，且与客户端看到的 revision 一致。
    let subtree = ports
        .store
        .trash_subtree(request.op.root, request.entry)
        .await
        .map_err(store_failure)?
        .ok_or_else(|| {
            Box::new(RestoreFailure::new(RestoreFailureReason::NotInTrash {
                detail: format!(
                    "条目 {} 不在回收站清单里（已被恢复或内容已过期回收）",
                    request.entry
                ),
            }))
        })?;
    let root = subtree.root().clone();
    check_subtree(&subtree, at, request)?;

    // 4. 实际位置：原父目录必须存在且是目录；根的名称被活跃条目占用就另名（T13），后代跟着移位。
    let parent_kind = ports
        .store
        .parent_kind(request.op.root, root.parent)
        .await
        .map_err(store_failure)?;
    if parent_kind != Some(EntryKind::Directory) {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::SubtreeChanged {
                detail: format!(
                    "原父目录 {} 已不存在或不是目录，不能把条目放回原位",
                    root.parent
                ),
            },
        )));
    }
    let (parent_path, original_name) = split_path(&root.path);
    validate_name(&original_name).map_err(invalid_request)?;
    let (name, renamed) = choose_name(ports, request, root.parent, &original_name).await?;
    let path = join_path(&parent_path, &name);
    validate_target_path(&path).map_err(invalid_request)?;
    if target_name(&path) != Some(name.as_str()) {
        return Err(invalid_request(format!(
            "恢复路径 {path:?} 的最后一段不是实际名称 {name:?}"
        )));
    }

    // 5. 规划整棵子树在新位置下的路径与顺序：清单结构不一致在这里停写，不进入任何 I/O。
    let plan = plan_subtree(&subtree, &path)?;

    // 6. 先登记 pending：并发的同一 OpId 只有一个能继续。
    match ports
        .store
        .begin_restore(request.op, request.digest)
        .await
        .map_err(store_failure)?
    {
        RestoreBegin::Started => {}
        RestoreBegin::Recorded => {
            let recorded = ports
                .store
                .operation(request.op)
                .await
                .map_err(store_failure)?
                .ok_or_else(|| pending(request.op.op))?;
            return resolve_recorded(recorded, request);
        }
    }

    // 7. 工作目录：先按路径深度把目录建回来（恢复不合并进别的目录），再逐个文件发布。
    for planned in plan
        .iter()
        .filter(|planned| planned.kind == EntryKind::Directory)
    {
        create_restored_directory(ports.namespace, &planned.path)?;
    }
    let mut leases: Vec<ContentLease> = Vec::new();
    let mut intents: Vec<IntentId> = Vec::new();
    for (index, planned) in plan
        .iter()
        .filter(|planned| planned.kind == EntryKind::File)
        .enumerate()
    {
        let index = u64::try_from(index)
            .map_err(|_| invalid_request("本次恢复的文件数超出序号范围".to_owned()))?;
        match publish_restored_file(ports, at, request, planned, index).await {
            Ok((lease, intent)) => {
                leases.push(lease);
                intents.push(intent);
            }
            Err(failed) => {
                let mut held = std::mem::take(&mut leases);
                held.extend(failed.lease);
                let failure = release_after_failure(ports.leases, &held, failed.failure).await;
                return Err(Box::new(failure));
            }
        }
    }

    // 8. 单一事务：整棵子树清软删除、改位置、推进 revision、删清单行、事件批次、结果、意图收尾。
    let write = TrashRestoreWrite {
        intents: intents.clone(),
        key: request.op,
        digest: request.digest,
        entry: request.entry,
        expected_revision: root.revision,
        path: path.clone(),
        name: name.clone(),
        renamed,
        entries: plan
            .iter()
            .map(|planned| TrashRestoreEntry {
                entry: planned.entry,
                parent: planned.parent,
                name: planned.name.clone(),
                path: planned.path.clone(),
                kind: planned.kind,
                content_version: planned.content_version,
                expected_revision: planned.revision,
            })
            .collect(),
        at,
    };
    let record = match ports.store.apply_trash_restore(&write).await {
        Ok(record) => record,
        Err(error) => {
            let failure = RestoreFailure::new(trash_failure_after_transaction(error));
            let failure = release_after_failure(ports.leases, &leases, failure).await;
            return Err(Box::new(failure));
        }
    };
    if let Err(failure) = check_record(&record, request, &write) {
        let failure = release_after_failure(ports.leases, &leases, *failure).await;
        return Err(Box::new(failure));
    }

    let disposition = release_all(ports.leases, &leases).await;
    Ok(RestoredTrash {
        result: record.result,
        lease: disposition,
    })
}

/// 把一个恢复目标目录建回工作目录：目标必须空缺。
///
/// 已存在的目录不是“可以复用的位置”——那会把清单内容合并进别人的目录；一律按
/// `SUBTREE_CHANGED` 停写，让调用方重新观察现场（design §10.1 末行、§11.3）。
fn create_restored_directory<N>(namespace: &mut N, path: &str) -> Result<(), Box<RestoreFailure>>
where
    N: NamespacePort + ?Sized,
{
    let observed = namespace
        .inspect(path)
        .map_err(|error| Box::new(RestoreFailure::new(RestoreFailureReason::Namespace(error))))?;
    if !matches!(observed, PathState::Absent) {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::SubtreeChanged {
                detail: format!(
                    "恢复目标目录 {path:?} 已存在（{observed}），不把清单内容合并进现有目录"
                ),
            },
        )));
    }
    namespace
        .create_directory(path)
        .map(|_state| ())
        .map_err(|error| Box::new(RestoreFailure::new(RestoreFailureReason::Namespace(error))))
}

/// 单个文件的发布失败：主因 + 该文件已建立的租约（走到发布阶段才有）。
///
/// 把“租约是否已建”一起交回调用方，让批量恢复的清理路径不丢证据、也不假装解除成功。
struct FilePublishFailure {
    failure: RestoreFailure,
    lease: Option<ContentLease>,
}

impl FilePublishFailure {
    /// 还没建租约就失败（内容事实、内容地址、序号派生）。
    fn before_lease(failure: RestoreFailure) -> Self {
        FilePublishFailure {
            failure,
            lease: None,
        }
    }

    /// 租约已建立之后失败：交回租约，由调用方统一解除。
    fn after_lease(failure: RestoreFailure, lease: ContentLease) -> Self {
        FilePublishFailure {
            failure,
            lease: Some(lease),
        }
    }
}

/// 一个文件的工作目录部分：内容租约 → 现场必须空缺 → 意图 → 发布。
///
/// 返回已建立的租约与本文件的意图身份；失败时把租约一起交回调用方。
async fn publish_restored_file<S, L, T, N, C>(
    ports: &mut TrashRestorePorts<'_, S, L, T, N, C>,
    at: UtcTimestamp,
    request: &TrashRestoreRequest,
    planned: &PlannedEntry,
    index: u64,
) -> Result<(ContentLease, IntentId), Box<FilePublishFailure>>
where
    S: TrashRestoreStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    N: NamespacePort + ?Sized,
    C: ContentLeasePort + ?Sized,
{
    let Some(version) = planned.content_version else {
        return Err(Box::new(FilePublishFailure::before_lease(
            RestoreFailure::new(RestoreFailureReason::StoreContract {
                detail: format!("普通文件 {} 在清单里没有内容版本", planned.entry),
            }),
        )));
    };
    let content = match ports.store.content_facts(request.op.root, version).await {
        Ok(Some(content)) => content,
        Ok(None) => {
            return Err(Box::new(FilePublishFailure::before_lease(
                RestoreFailure::new(RestoreFailureReason::VersionGone {
                    detail: format!(
                        "版本 {version} 的内容已不可读（清单里的文件 {:?}）",
                        planned.path
                    ),
                }),
            )));
        }
        Err(error) => {
            return Err(Box::new(FilePublishFailure::before_lease(restore_failure(
                error,
            ))));
        }
    };
    let intent = match intent_for(request.intent_seed, index) {
        Ok(intent) => intent,
        Err(failure) => return Err(Box::new(FilePublishFailure::before_lease(*failure))),
    };
    let expires_at = match lease_expiry(at) {
        Ok(expires_at) => expires_at,
        Err(failure) => return Err(Box::new(FilePublishFailure::before_lease(*failure))),
    };

    // 内容租约：把“读到内容版本”与“新位置引用同一对象”之间的窗口保护起来。
    let lease = match ports
        .leases
        .acquire(&ContentLeaseRequest {
            root: request.op.root,
            digest: content.digest,
            purpose: ContentLeasePurpose::Download,
            expires_at,
        })
        .await
    {
        Ok(lease) => lease,
        Err(error) => {
            return Err(Box::new(FilePublishFailure::before_lease(
                RestoreFailure::new(RestoreFailureReason::Lease(error)),
            )));
        }
    };

    // 现场必须空缺：恢复不覆盖现在的新文件（T13）。
    let observed = match ports.target.observe(&planned.path) {
        Ok(observed) => observed,
        Err(error) => {
            return Err(Box::new(FilePublishFailure::after_lease(
                RestoreFailure::new(RestoreFailureReason::TargetUnreadable(error)),
                lease,
            )));
        }
    };
    let vacancy = TargetBefore::Absent;
    if !vacancy.satisfied_by(&observed) {
        return Err(Box::new(FilePublishFailure::after_lease(
            RestoreFailure::new(RestoreFailureReason::Reconcile {
                expected: vacancy.to_json(),
                observed,
            }),
            lease,
        )));
    }

    let new = NewPutIntent {
        intent_id: intent,
        root: request.op.root,
        target_path: planned.path.clone(),
        staging_path: staging_name(intent),
        before: TargetBefore::Absent,
        after: TargetAfter {
            digest: content.digest,
            size: content.bytes,
        },
    };
    match publish_put(ports.log, ports.target, at, &new).await {
        Ok(_) => Ok((lease, intent)),
        Err(failure) => Err(Box::new(FilePublishFailure::after_lease(
            reclassify_publish_failure(*failure),
            lease,
        ))),
    }
}

/// 另名规则：原位置被活跃条目占用时按 `名称（恢复）`、`名称（恢复 2）`… 逐个试，扩展名保留。
async fn choose_name<S, L, T, N, C>(
    ports: &mut TrashRestorePorts<'_, S, L, T, N, C>,
    request: &TrashRestoreRequest,
    parent: EntryId,
    original: &str,
) -> Result<(String, bool), Box<RestoreFailure>>
where
    S: TrashRestoreStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    N: NamespacePort + ?Sized,
    C: ContentLeasePort + ?Sized,
{
    let occupied = ports
        .store
        .name_occupant(request.op.root, parent, original)
        .await
        .map_err(store_failure)?;
    if occupied.is_none() {
        // 没被占用就用原名（`name_occupant` 只算活跃条目，软删除的条目本身不算占用）。
        return Ok((original.to_owned(), false));
    }
    for attempt in 1..=MAX_RESTORE_NAME_ATTEMPTS {
        let candidate = alternate_name(original, attempt);
        if validate_name(&candidate).is_err() {
            continue;
        }
        let occupied = ports
            .store
            .name_occupant(request.op.root, parent, &candidate)
            .await
            .map_err(store_failure)?;
        if occupied.is_none() {
            return Ok((candidate, true));
        }
    }
    Err(Box::new(RestoreFailure::new(
        RestoreFailureReason::NameUnavailable {
            detail: format!("原父目录 {parent} 下试过 {MAX_RESTORE_NAME_ATTEMPTS} 个另名都被占用"),
        },
    )))
}

/// 第 `attempt` 个另名：`原干（恢复）扩展名`、`原干（恢复 2）扩展名`…（design §10.2 的命名风格，
/// 扩展名与原名干保留，只追加恢复后缀）。
#[must_use]
pub fn alternate_name(original: &str, attempt: u32) -> String {
    let (stem, extension) = match original.rfind('.') {
        Some(index) if index > 0 => original.split_at(index),
        _ => (original, ""),
    };
    if attempt <= 1 {
        format!("{stem}（恢复）{extension}")
    } else {
        format!("{stem}（恢复 {attempt}）{extension}")
    }
}

/// 恢复清单里的一条在**新位置**下的形态：路径与名称可能因为根另名而整体移位。
///
/// 只在核心做纯计算（不碰端口）：它是“清单即事实”与“工作目录动作”之间唯一的映射。
#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedEntry {
    /// 条目身份（恢复后不变）。
    entry: EntryId,
    /// 恢复后的父目录（根用清单里的父目录，后代用清单里的父目录）。
    parent: EntryId,
    /// 恢复后的 Root 内相对路径。
    path: String,
    /// 恢复后的名称（只有根可能被另名）。
    name: String,
    /// 条目种类。
    kind: EntryKind,
    /// 恢复后的当前内容版本；目录为 `None`。
    content_version: Option<VersionId>,
    /// 清单里的 revision（事务内比较并写）。
    revision: Revision,
}

impl PlannedEntry {
    /// Root 内相对路径的深度（`a` 为 0、`a/b.txt` 为 1）。
    fn depth(&self) -> usize {
        self.path.matches('/').count()
    }
}

/// 把清单映射到恢复后的新位置，并定下处理顺序（目录先于其内容，同深度按路径）。
///
/// 根用调用方决定的实际路径（可能已另名），后代一律“根的新路径 + 清单里的相对后缀”，因此另名
/// 只改变位置前缀、不改动子树内部结构。
///
/// 任何结构不一致都按 `store_contract_violation`（wire `NEEDS_RECOVERY`）停写人工核对，不猜一个
/// “差不多”的布局：后代路径不在根之下、同一个条目出现两次、两个条目落到同一路径、父目录不在
/// 清单里、父目录是文件、普通文件带后代。
///
/// # Errors
///
/// 清单与事实不一致时返回 [`RestoreFailureReason::StoreContract`]。
fn plan_subtree(
    subtree: &TrashSubtree,
    root_path: &str,
) -> Result<Vec<PlannedEntry>, Box<RestoreFailure>> {
    let root = subtree.root();
    let under_root = format!("{}/", root.path);
    let mut planned: Vec<PlannedEntry> = Vec::with_capacity(subtree.entries.len());
    for facts in &subtree.entries {
        let path = if facts.entry == root.entry {
            root_path.to_owned()
        } else {
            let Some(rest) = facts.path.strip_prefix(&under_root) else {
                return Err(contract(format!(
                    "清单里的后代 {}（路径 {:?}）不在根路径 {:?} 之下",
                    facts.entry, facts.path, root.path
                )));
            };
            join_path(root_path, rest)
        };
        let Some(name) = target_name(&path).map(str::to_owned) else {
            return Err(contract(format!("恢复路径 {path:?} 没有名称")));
        };
        validate_name(&name).map_err(|error| contract(format!("清单里的名称不合法：{error}")))?;
        validate_target_path(&path)
            .map_err(|error| contract(format!("清单里的恢复路径不合法：{error}")))?;
        if planned.iter().any(|existing| existing.entry == facts.entry) {
            return Err(contract(format!(
                "清单里条目 {} 出现了不止一次",
                facts.entry
            )));
        }
        if planned.iter().any(|existing| existing.path == path) {
            return Err(contract(format!("清单把两个条目放到同一路径 {path:?}")));
        }
        planned.push(PlannedEntry {
            entry: facts.entry,
            parent: facts.parent,
            path,
            name,
            kind: facts.kind,
            content_version: facts.content_version,
            revision: facts.revision,
        });
    }
    if root.kind == EntryKind::File && planned.len() > 1 {
        return Err(contract(format!(
            "普通文件 {} 在清单里带了 {} 个后代",
            root.entry,
            planned.len() - 1
        )));
    }
    for item in planned.iter().skip(1) {
        let Some(parent) = planned
            .iter()
            .find(|candidate| candidate.entry == item.parent)
        else {
            return Err(contract(format!(
                "清单缺少条目 {} 的父目录 {}",
                item.entry, item.parent
            )));
        };
        if parent.kind != EntryKind::Directory {
            return Err(contract(format!(
                "清单把条目 {} 挂在非目录父项 {} 下",
                item.entry, item.parent
            )));
        }
    }
    planned.sort_by(|left, right| {
        left.depth()
            .cmp(&right.depth())
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(planned)
}

/// 本次恢复里第 `index` 个被发布文件的意图身份，由请求里的意图种子派生。
///
/// 第 0 个就是种子本身；其后把种子最后 8 字节当大端 u64 递增——同一操作内逐个文件互不相同，同一
/// `OpId` 重试（同一清单、同一顺序）得到同一组身份，`fs_intents` 里的恢复证据因此可重入。溢出报
/// `invalid_request`，不回绕到别的操作的身份上。
///
/// # Errors
///
/// 种子最后 8 字节加上 `index` 溢出时返回 [`RestoreFailureReason::InvalidRequest`]。
pub fn intent_for(seed: IntentId, index: u64) -> Result<IntentId, Box<RestoreFailure>> {
    let mut bytes = *seed.as_bytes();
    let split = bytes.len() - 8;
    let mut tail = [0u8; 8];
    tail.copy_from_slice(&bytes[split..]);
    let next = u64::from_be_bytes(tail)
        .checked_add(index)
        .ok_or_else(|| invalid_request("意图种子序号溢出".to_owned()))?;
    bytes[split..].copy_from_slice(&next.to_be_bytes());
    Ok(IntentId::from_bytes(bytes))
}

/// 服务端派生的暂存文件名：`.restore-<意图十六进制>`。
///
/// 由意图身份派生而不是客户端给：稳定、可重入（同一 `OpId` 重试得到同一组暂存名），操作内互不
/// 相同，形态是单个文件名（不含路径分隔符），因此不会指向别的路径；内容区 `staging/` 目录按它
/// 定位每个文件的暂存副本（design §11.2 步骤 6）。
#[must_use]
pub fn staging_name(intent: IntentId) -> String {
    format!(".restore-{}", intent.to_hex())
}

/// 清单事实核对：根必须是本次请求的条目与基线、未过保留期，且每行的种类与内容引用自洽。
fn check_subtree(
    subtree: &TrashSubtree,
    at: UtcTimestamp,
    request: &TrashRestoreRequest,
) -> Result<(), Box<RestoreFailure>> {
    let root = subtree.root();
    if root.entry != request.entry {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::StoreContract {
                detail: format!(
                    "清单根是 {}，本次恢复请求的是 {}",
                    root.entry, request.entry
                ),
            },
        )));
    }
    if root.revision != request.expected_revision {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::BaseMismatch {
                detail: format!(
                    "清单里的 revision 是 {}，请求基线是 {}（回收站事实已变化）",
                    root.revision, request.expected_revision
                ),
            },
        )));
    }
    if root.expires_at < at {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::VersionGone {
                detail: format!("条目 {} 的保留期已在 {} 结束", root.entry, root.expires_at),
            },
        )));
    }
    // 每一行都必须在保留期内、种类与内容引用自洽：批量恢复不要在发布到一半才发现某行已过期
    // （那时前面的文件已经发布、意图已经持久化）。
    for facts in &subtree.entries {
        if facts.expires_at < at {
            return Err(Box::new(RestoreFailure::new(
                RestoreFailureReason::VersionGone {
                    detail: format!(
                        "条目 {}（路径 {:?}）的保留期已在 {} 结束",
                        facts.entry, facts.path, facts.expires_at
                    ),
                },
            )));
        }
        match facts.kind {
            EntryKind::File if facts.content_version.is_none() => {
                return Err(contract(format!(
                    "普通文件 {} 在清单里没有内容版本",
                    facts.entry
                )));
            }
            EntryKind::Directory if facts.content_version.is_some() => {
                return Err(contract(format!(
                    "目录 {} 在清单里带了内容版本",
                    facts.entry
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// 清单/事务与事实不一致：适配器缺陷或坏数据，停写人工核对。
fn contract(detail: String) -> Box<RestoreFailure> {
    Box::new(RestoreFailure::new(RestoreFailureReason::StoreContract {
        detail,
    }))
}

/// 事务声称成功时核对证据：不一致说明适配器违反端口契约，必须停写人工核对。
fn check_record(
    record: &crate::ports::restore_store::TrashRestoreRecord,
    request: &TrashRestoreRequest,
    write: &TrashRestoreWrite,
) -> Result<(), Box<RestoreFailure>> {
    let problem = if !record.phase.is_finished() {
        Some(format!("意图阶段是 {} 而不是 finalized", record.phase))
    } else if record.intents != write.intents {
        Some(format!(
            "事务收尾的意图 {:?} 不是本次请求的 {:?}",
            record.intents, write.intents
        ))
    } else if record.result.op != request.op.op {
        Some(format!(
            "事务记录的操作 {} 不是请求操作 {}",
            record.result.op, request.op.op
        ))
    } else if record.result.entry != request.entry {
        Some(format!(
            "事务恢复的条目 {} 不是请求条目 {}",
            record.result.entry, request.entry
        ))
    } else if record.result.path != write.path || record.result.name != write.name {
        Some(format!(
            "事务把条目恢复到 {:?}（名称 {:?}），不是决定的 {:?}（名称 {:?}）",
            record.result.path, record.result.name, write.path, write.name
        ))
    } else if record.result.renamed != write.renamed {
        Some(format!(
            "事务记录的另名标记是 {}，本次决定是 {}",
            record.result.renamed, write.renamed
        ))
    } else if record.result.revision <= write.expected_revision {
        Some(format!(
            "条目 revision 没有推进：{} 不大于清单里的 {}",
            record.result.revision, write.expected_revision
        ))
    } else if record.result.entries != write.entries.len() as u64 {
        Some(format!(
            "事务回报恢复了 {} 个条目，本次请求是 {} 个",
            record.result.entries,
            write.entries.len()
        ))
    } else {
        None
    };
    match problem {
        Some(detail) => Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::StoreContract { detail },
        ))),
        None => Ok(()),
    }
}

/// 按已记录的 `OpId` 状态回答：重放原结果、拒绝不同请求，或拒绝重做未完成的记录。
fn resolve_recorded(
    recorded: RecordedTrashRestore,
    request: &TrashRestoreRequest,
) -> Result<RestoredTrash, Box<RestoreFailure>> {
    if recorded.digest != request.digest {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::OperationReused {
                op: request.op.op,
                recorded: recorded.digest,
                requested: request.digest,
            },
        )));
    }
    let Some(result) = recorded.result else {
        return Err(pending(request.op.op));
    };
    if result.entry != request.entry {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::StoreContract {
                detail: format!(
                    "同一 OpId {} 记录的是条目 {} 的恢复，本次请求是条目 {}",
                    request.op.op, result.entry, request.entry
                ),
            },
        )));
    }
    Ok(RestoredTrash {
        result,
        // 重放不再建立租约：首次提交已经把内容放回原位置，新位置本身就是引用。
        lease: LeaseDisposition::Released,
    })
}

fn store_failure(error: RestoreStoreError) -> Box<RestoreFailure> {
    Box::new(restore_failure(error))
}

fn restore_failure(error: RestoreStoreError) -> RestoreFailure {
    match error {
        RestoreStoreError::EntryMissing { entry } => {
            RestoreFailure::new(RestoreFailureReason::BaseMismatch {
                detail: format!("条目 {entry} 已不存在"),
            })
        }
        RestoreStoreError::UnknownRoot { root } => {
            RestoreFailure::new(RestoreFailureReason::BaseMismatch {
                detail: format!("Root {root} 已不存在"),
            })
        }
        error => RestoreFailure::new(RestoreFailureReason::Store(error)),
    }
}

fn trash_failure_after_transaction(error: RestoreStoreError) -> RestoreFailureReason {
    match error {
        RestoreStoreError::RevisionChanged { expected, actual } => {
            RestoreFailureReason::BaseMismatch {
                detail: format!("提交时条目 revision 已从 {expected} 变为 {actual}"),
            }
        }
        RestoreStoreError::EntryMissing { entry } => RestoreFailureReason::BaseMismatch {
            detail: format!("提交时条目 {entry} 已不存在"),
        },
        RestoreStoreError::NameOccupied { path } => RestoreFailureReason::NameUnavailable {
            detail: format!("提交时恢复目标 {path:?} 已被占用"),
        },
        error => RestoreFailureReason::Store(error),
    }
}

fn pending(op: OpId) -> Box<RestoreFailure> {
    Box::new(RestoreFailure::new(
        RestoreFailureReason::OperationPending { op },
    ))
}

fn invalid_request(detail: String) -> Box<RestoreFailure> {
    Box::new(RestoreFailure::new(RestoreFailureReason::InvalidRequest {
        detail,
    }))
}

fn split_path(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent.to_owned(), name.to_owned()),
        None => (String::new(), path.to_owned()),
    }
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn lease_expiry(at: UtcTimestamp) -> Result<UtcTimestamp, Box<RestoreFailure>> {
    at.as_unix_seconds()
        .checked_add(crate::history::version::RESTORE_LEASE_TTL_SECONDS)
        .and_then(|value| UtcTimestamp::from_unix_seconds(value).ok())
        .ok_or_else(|| invalid_request("租约到期时间超出可表示范围".to_owned()))
}
#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::error::ErrorCode;
    use vaultsyncd_domain::ids::ID_LEN;

    use crate::commit::intent::PutPhase;
    use crate::objects::{ExpectedContent, ObjectLayout};
    use crate::ports::namespace::NamespaceError;
    use crate::ports::object_store::StorageFailure;
    use crate::ports::restore_store::{TrashEntryFacts, TrashRestoreRecord};
    use crate::ports::target::TargetError;
    use crate::test_support::{
        FakeContentLease, FakeIntentLog, FakeNamespace, FakeTarget, FakeTrashRestoreStore, Trace,
        device_id, digest, entry_id, epoch, intent_id, op_id, request_digest, revision, root_id,
        root_seq, tx_id, user_time, version_id,
    };

    struct Fixture {
        gate: RootCommitGate,
        store: FakeTrashRestoreStore,
        log: FakeIntentLog,
        target: FakeTarget,
        namespace: FakeNamespace,
        leases: FakeContentLease,
        trace: Trace,
    }

    fn key() -> OperationKey {
        OperationKey {
            root: root_id(0x21),
            device: device_id(0x61),
            op: op_id(0x72),
        }
    }

    fn request() -> TrashRestoreRequest {
        TrashRestoreRequest {
            intent_seed: intent_id(0x42),
            op: key(),
            digest: request_digest(0x03),
            entry: entry_id(0x51),
            expected_epoch: epoch(3),
            expected_revision: revision(7),
        }
    }

    /// 三文件两目录的清单：`projects` 里有一个文件和一个子目录，子目录里有两个文件。
    fn projects_subtree() -> Vec<TrashEntryFacts> {
        let root = entry_id(0x50);
        let sub = entry_id(0x52);
        vec![
            directory(root, entry_id(0x31), "projects", 7),
            file(0x51, root, "projects/plan.md", 3, 0x81),
            directory(sub, root, "projects/sub", 4),
            file(0x53, sub, "projects/sub/a.txt", 5, 0x82),
            file(0x54, sub, "projects/sub/b.txt", 6, 0x83),
        ]
    }

    fn directory(entry: EntryId, parent: EntryId, path: &str, rev: u64) -> TrashEntryFacts {
        TrashEntryFacts {
            entry,
            parent,
            path: path.to_owned(),
            kind: EntryKind::Directory,
            revision: revision(rev),
            content_version: None,
            deleted_at: user_time(900),
            expires_at: user_time(2_000_000),
        }
    }

    fn file(entry: u8, parent: EntryId, path: &str, rev: u64, version: u8) -> TrashEntryFacts {
        TrashEntryFacts {
            entry: entry_id(entry),
            parent,
            path: path.to_owned(),
            kind: EntryKind::File,
            revision: revision(rev),
            content_version: Some(version_id(version)),
            deleted_at: user_time(900),
            expires_at: user_time(2_000_000),
        }
    }

    fn trashed(kind: EntryKind) -> TrashEntryFacts {
        TrashEntryFacts {
            entry: entry_id(0x51),
            parent: entry_id(0x31),
            path: "docs/report.docx".to_owned(),
            kind,
            revision: revision(7),
            content_version: match kind {
                EntryKind::File => Some(version_id(0x81)),
                EntryKind::Directory => None,
            },
            deleted_at: user_time(900),
            expires_at: user_time(2_000_000),
        }
    }

    fn fixture_with(mut store: FakeTrashRestoreStore) -> Fixture {
        let trace = Trace::default();
        store.trace = trace.clone();
        Fixture {
            gate: RootCommitGate::new(),
            store,
            log: FakeIntentLog {
                trace: trace.clone(),
                ..FakeIntentLog::default()
            },
            target: FakeTarget::absent().with_trace(trace.clone()),
            namespace: FakeNamespace::new().with_trace(trace.clone()),
            leases: FakeContentLease::new().with_trace(trace.clone()),
            trace,
        }
    }

    fn fixture(facts: TrashEntryFacts) -> Fixture {
        fixture_with(FakeTrashRestoreStore::new().with_trashed(facts, entry_id(0x31)))
    }

    fn projects_fixture() -> Fixture {
        fixture_with(
            FakeTrashRestoreStore::new()
                .with_subtree(projects_subtree(), entry_id(0x31))
                .with_content(version_id(0x81), ExpectedContent::new(digest(0x11), 5))
                .with_content(version_id(0x82), ExpectedContent::new(digest(0x22), 6))
                .with_content(version_id(0x83), ExpectedContent::new(digest(0x33), 7)),
        )
    }

    fn subtree_request() -> TrashRestoreRequest {
        TrashRestoreRequest {
            entry: entry_id(0x50),
            ..request()
        }
    }

    async fn run(
        fixture: &mut Fixture,
        request: &TrashRestoreRequest,
    ) -> Result<RestoredTrash, Box<RestoreFailure>> {
        let mut ports = TrashRestorePorts {
            store: &mut fixture.store,
            log: &mut fixture.log,
            target: &mut fixture.target,
            namespace: &mut fixture.namespace,
            leases: &mut fixture.leases,
        };
        restore_trash(&mut fixture.gate, &mut ports, user_time(1_000), request).await
    }

    /// 轨迹里以 `prefix` 开头的行去掉前缀，按出现顺序返回（断言跨端口顺序用）。
    fn collected(fixture: &Fixture, prefix: &str) -> Vec<String> {
        fixture
            .trace
            .entries()
            .into_iter()
            .filter_map(|line| line.strip_prefix(prefix).map(str::to_owned))
            .collect()
    }

    fn published_targets(fixture: &Fixture) -> Vec<&str> {
        fixture
            .target
            .publishes
            .iter()
            .map(|publish| publish.target_path.as_str())
            .collect()
    }

    fn written_paths(write: &TrashRestoreWrite) -> Vec<&str> {
        write
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect()
    }

    fn expected_intents(seed: IntentId) -> Vec<IntentId> {
        (0..3)
            .map(|index| intent_for(seed, index).expect("测试用序号可表示"))
            .collect()
    }

    #[tokio::test]
    async fn file_restores_to_its_original_path() {
        let mut fixture = fixture(trashed(EntryKind::File));
        let request = request();
        let restored = run(&mut fixture, &request).await.expect("恢复应当成功");

        assert_eq!(restored.result.path, "docs/report.docx");
        assert_eq!(restored.result.name, "report.docx");
        assert!(!restored.result.renamed);
        assert_eq!(restored.result.revision, revision(8));
        assert_eq!(restored.result.entries, 1);
        assert_eq!(restored.lease, LeaseDisposition::Released);

        // 清单里的内容版本被重新发布到原路径，发布前有内容租约。
        assert_eq!(fixture.target.publishes.len(), 1);
        assert_eq!(fixture.target.publishes[0].target_path, "docs/report.docx");
        assert_eq!(fixture.target.publishes[0].digest, digest(0x0d));
        assert_eq!(fixture.target.publishes[0].size, 11);
        assert_eq!(fixture.leases.acquired.len(), 1);
        assert_eq!(
            fixture.leases.acquired[0].purpose,
            ContentLeasePurpose::Download
        );
        assert_eq!(fixture.leases.released.len(), 1);

        // 暂存名由服务端从意图身份派生，不来自客户端。
        assert_eq!(
            fixture.log.intents[0].staging_path,
            staging_name(request.intent_seed)
        );
        assert!(ObjectLayout::validate_staging_name(&fixture.log.intents[0].staging_path).is_ok());

        // 事务：同一意图收尾，条目身份不变。
        let write = &fixture.store.writes[0];
        assert_eq!(write.intents, vec![request.intent_seed]);
        assert_eq!(write.entries.len(), 1);
        assert_eq!(write.entries[0].entry, request.entry);
        assert_eq!(write.entries[0].expected_revision, revision(7));
    }

    #[tokio::test]
    async fn occupied_name_restores_under_an_alternate_name() {
        let mut fixture = fixture(trashed(EntryKind::File));
        fixture
            .store
            .occupy(entry_id(0x31), "report.docx", entry_id(0x99));
        let restored = run(&mut fixture, &request())
            .await
            .expect("另名恢复应当成功");

        // T13：原名称已被占用时不覆盖它，另名恢复并回报实际路径。
        assert!(restored.result.renamed);
        assert_eq!(restored.result.name, "report（恢复）.docx");
        assert_eq!(restored.result.path, "docs/report（恢复）.docx");
        assert_eq!(
            fixture.target.publishes[0].target_path,
            "docs/report（恢复）.docx"
        );
        assert_ne!(fixture.target.publishes[0].target_path, "docs/report.docx");
        assert_eq!(fixture.store.writes[0].path, "docs/report（恢复）.docx");
        assert!(fixture.store.writes[0].renamed);
    }

    #[tokio::test]
    async fn directory_restores_its_position_without_content() {
        let mut fixture = fixture(trashed(EntryKind::Directory));
        let restored = run(&mut fixture, &request())
            .await
            .expect("目录恢复应当成功");

        assert_eq!(restored.result.path, "docs/report.docx");
        assert!(fixture.target.publishes.is_empty(), "目录没有内容要发布");
        assert!(fixture.log.intents.is_empty(), "目录没有内容就没有发布意图");
        assert!(fixture.store.writes[0].intents.is_empty());
        assert_eq!(
            collected(&fixture, "namespace mkdir "),
            vec!["docs/report.docx".to_owned()],
            "目录要在工作目录建回来"
        );
    }

    #[tokio::test]
    async fn subtree_with_descendants_restores_directories_then_files() {
        let mut fixture = projects_fixture();
        let request = subtree_request();
        let restored = run(&mut fixture, &request).await.expect("整棵子树应当恢复");

        // 结果：根的实际位置 + 整棵子树的条目数。
        assert_eq!(restored.result.entries, 5);
        assert_eq!(restored.result.path, "projects");
        assert_eq!(restored.result.name, "projects");
        assert!(!restored.result.renamed);
        assert_eq!(restored.result.revision, revision(8));
        assert_eq!(restored.lease, LeaseDisposition::Released);

        // 目录按路径深度建回，根先于子目录。
        assert_eq!(
            collected(&fixture, "namespace mkdir "),
            vec!["projects".to_owned(), "projects/sub".to_owned()]
        );

        // 文件按（深度，路径）逐个发布，内容取清单里的内容版本。
        assert_eq!(
            published_targets(&fixture),
            vec![
                "projects/plan.md",
                "projects/sub/a.txt",
                "projects/sub/b.txt"
            ]
        );
        assert_eq!(
            fixture
                .target
                .publishes
                .iter()
                .map(|publish| publish.digest)
                .collect::<Vec<_>>(),
            vec![digest(0x11), digest(0x22), digest(0x33)]
        );
        assert_eq!(
            fixture
                .target
                .publishes
                .iter()
                .map(|publish| publish.size)
                .collect::<Vec<_>>(),
            vec![5, 6, 7]
        );

        // 每个文件都有自己的内容租约（download 用途、同一 TTL），都已在提交后解除。
        assert_eq!(fixture.leases.acquired.len(), 3);
        assert!(fixture.leases.acquired.iter().all(|lease| {
            lease.purpose == ContentLeasePurpose::Download && lease.expires_at == user_time(1_900)
        }));
        assert_eq!(fixture.leases.released.len(), 3);

        // 意图身份从种子按发布序号派生，暂存名由意图身份派生，三者互不相同。
        let expected = expected_intents(request.intent_seed);
        let intents = fixture
            .log
            .intents
            .iter()
            .map(|intent| intent.intent_id)
            .collect::<Vec<_>>();
        assert_eq!(intents, expected);
        assert_eq!(
            fixture
                .log
                .intents
                .iter()
                .map(|intent| intent.staging_path.clone())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|intent| staging_name(*intent))
                .collect::<Vec<_>>()
        );
        assert!(
            fixture
                .log
                .intents
                .iter()
                .all(|intent| intent.phase == PutPhase::Published)
        );

        // 单一事务：整棵子树一次写回，意图一次收尾。
        assert_eq!(fixture.store.writes.len(), 1);
        let write = &fixture.store.writes[0];
        assert_eq!(write.intents, expected);
        assert_eq!(
            written_paths(write),
            vec![
                "projects",
                "projects/plan.md",
                "projects/sub",
                "projects/sub/a.txt",
                "projects/sub/b.txt"
            ]
        );
        assert_eq!(write.entries[0].parent, entry_id(0x31));
        assert_eq!(
            write
                .entries
                .iter()
                .map(|entry| entry.expected_revision)
                .collect::<Vec<_>>(),
            vec![
                revision(7),
                revision(3),
                revision(4),
                revision(5),
                revision(6)
            ]
        );
        assert_eq!(write.entries[3].parent, entry_id(0x52));
        assert_eq!(write.entries[3].content_version, Some(version_id(0x82)));
        assert!(!fixture.gate.is_held(key().root));
    }

    #[tokio::test]
    async fn deep_nesting_creates_every_level_before_its_contents() {
        let root = entry_id(0x40);
        let year = entry_id(0x41);
        let trip = entry_id(0x42);
        let raw = entry_id(0x43);
        let entries = vec![
            directory(root, entry_id(0x31), "photos", 9),
            directory(year, root, "photos/2024", 3),
            directory(trip, year, "photos/2024/trip", 4),
            file(0x44, trip, "photos/2024/trip/cover.jpg", 5, 0x81),
            directory(raw, trip, "photos/2024/trip/raw", 6),
            file(0x45, raw, "photos/2024/trip/raw/one.dng", 7, 0x82),
        ];
        let mut fixture =
            fixture_with(FakeTrashRestoreStore::new().with_subtree(entries, entry_id(0x31)));
        let request = TrashRestoreRequest {
            entry: root,
            expected_revision: revision(9),
            op: OperationKey {
                root: root_id(0x21),
                device: device_id(0x61),
                op: op_id(0x77),
            },
            digest: request_digest(0x04),
            ..request()
        };
        let restored = run(&mut fixture, &request).await.expect("深层子树应当恢复");

        assert_eq!(restored.result.entries, 6);
        assert_eq!(
            collected(&fixture, "namespace mkdir "),
            vec![
                "photos".to_owned(),
                "photos/2024".to_owned(),
                "photos/2024/trip".to_owned(),
                "photos/2024/trip/raw".to_owned()
            ]
        );
        assert_eq!(
            published_targets(&fixture),
            vec!["photos/2024/trip/cover.jpg", "photos/2024/trip/raw/one.dng"]
        );

        // 每个文件的发布都在其父目录的 mkdir 之后（发布不替别人建目录）。
        let trace = fixture.trace.entries();
        let mkdir_raw = trace
            .iter()
            .position(|line| line == "namespace mkdir photos/2024/trip/raw")
            .expect("raw 目录被创建");
        let publish_one = trace
            .iter()
            .position(|line| line == "target publish photos/2024/trip/raw/one.dng")
            .expect("one.dng 被发布");
        assert!(mkdir_raw < publish_one, "发布必须在父目录存在之后");
        assert_eq!(
            written_paths(&fixture.store.writes[0]),
            vec![
                "photos",
                "photos/2024",
                "photos/2024/trip",
                "photos/2024/trip/cover.jpg",
                "photos/2024/trip/raw",
                "photos/2024/trip/raw/one.dng"
            ]
        );
    }

    #[tokio::test]
    async fn failed_publish_aborts_the_whole_subtree_without_partial_restore() {
        let mut fixture = projects_fixture();
        fixture.target.fail_publish_at = Some((
            "projects/sub/a.txt".to_owned(),
            TargetError::Storage {
                stage: "rename",
                failure: StorageFailure::NoSpace,
                detail: "磁盘已满".to_owned(),
            },
        ));
        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("中间文件发布失败就整批停下");

        // 稳定错误码来自底层原因，失败带已持久化意图作证据，且不是 committed。
        assert_eq!(failure.code(), "no_space");
        assert_eq!(failure.error_code(), None);
        assert!(!failure.is_committed());
        assert_eq!(
            failure.intent_id,
            Some(intent_for(request().intent_seed, 1).expect("序号可表示"))
        );
        assert_eq!(failure.phase, Some(PutPhase::Prepared));

        // 没有元数据事务：任何一个条目都没有被标成已恢复。
        assert!(fixture.store.writes.is_empty());
        assert!(!fixture.gate.is_held(key().root));

        // 旧事实仍在：清单五条一条都没被拿走，条目与内容对象都还在。
        let subtree = fixture
            .store
            .subtrees
            .get(&entry_id(0x50))
            .expect("清单仍然完整");
        assert_eq!(subtree.entries.len(), 5);
        assert!(fixture.store.contents.contains_key(&version_id(0x83)));

        // 已经落地的部分留作恢复证据：目录、已发布文件与意图（一条 published、一条 prepared）。
        assert_eq!(
            collected(&fixture, "namespace mkdir "),
            vec!["projects".to_owned(), "projects/sub".to_owned()]
        );
        assert_eq!(
            published_targets(&fixture),
            vec!["projects/plan.md", "projects/sub/a.txt"]
        );
        assert_eq!(fixture.log.intents.len(), 2);
        assert_eq!(fixture.log.intents[0].phase, PutPhase::Published);
        assert_eq!(fixture.log.intents[1].phase, PutPhase::Prepared);

        // 失败路径同样解除租约：不留“仍保护到某时刻”的租约。
        assert_eq!(fixture.leases.acquired.len(), 2);
        assert_eq!(fixture.leases.released.len(), 2);
        assert!(failure.lease_expires_at.is_none());
    }

    #[tokio::test]
    async fn existing_target_directory_stops_before_publishing_anything() {
        let mut fixture = projects_fixture();
        let namespace = std::mem::take(&mut fixture.namespace);
        fixture.namespace = namespace.with_directory("projects/sub");

        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("目标目录已存在时不能把清单内容合并进去");
        assert_eq!(failure.code(), "subtree_changed");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert_eq!(
            collected(&fixture, "namespace mkdir "),
            vec!["projects".to_owned()]
        );
        assert!(fixture.target.publishes.is_empty());
        assert!(fixture.leases.acquired.is_empty());
        assert!(fixture.store.writes.is_empty());
        assert!(fixture.store.subtrees.contains_key(&entry_id(0x50)));
    }

    #[tokio::test]
    async fn directory_storage_failure_aborts_before_any_publish() {
        let mut fixture = projects_fixture();
        fixture.namespace.create_error = Some(NamespaceError::Storage {
            stage: "mkdir",
            failure: StorageFailure::NoSpace,
            detail: "磁盘已满".to_owned(),
        });

        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("建目录失败就整批停下");
        assert_eq!(failure.code(), "no_space");
        assert!(fixture.target.publishes.is_empty());
        assert!(fixture.leases.acquired.is_empty());
        assert!(fixture.store.writes.is_empty());
        assert!(fixture.store.subtrees.contains_key(&entry_id(0x50)));
    }

    #[tokio::test]
    async fn repeated_op_id_replays_the_whole_subtree_once() {
        let mut fixture = projects_fixture();
        let request = subtree_request();
        let first = run(&mut fixture, &request).await.expect("首次恢复成功");
        let second = run(&mut fixture, &request).await.expect("重放应当成功");

        assert_eq!(first.result, second.result);
        assert_eq!(first.result.entries, 5);
        assert_eq!(fixture.store.writes.len(), 1, "重放不得再做一次事务");
        assert_eq!(fixture.target.publishes.len(), 3, "重放不得再发布一次");
        assert_eq!(fixture.leases.acquired.len(), 3, "重放不得再建一批租约");
        assert_eq!(fixture.log.intents.len(), 3);
    }

    #[tokio::test]
    async fn occupied_root_name_renames_the_whole_subtree() {
        let mut fixture = projects_fixture();
        fixture
            .store
            .occupy(entry_id(0x31), "projects", entry_id(0x99));

        let restored = run(&mut fixture, &subtree_request())
            .await
            .expect("另名恢复整棵子树");
        assert!(restored.result.renamed);
        assert_eq!(restored.result.name, "projects（恢复）");
        assert_eq!(restored.result.path, "projects（恢复）");
        assert_eq!(restored.result.entries, 5);

        // 后代跟着根整体移位，各自名称不变。
        let write = &fixture.store.writes[0];
        assert_eq!(
            written_paths(write),
            vec![
                "projects（恢复）",
                "projects（恢复）/plan.md",
                "projects（恢复）/sub",
                "projects（恢复）/sub/a.txt",
                "projects（恢复）/sub/b.txt"
            ]
        );
        assert_eq!(write.entries[2].name, "sub");
        assert_eq!(
            published_targets(&fixture),
            vec![
                "projects（恢复）/plan.md",
                "projects（恢复）/sub/a.txt",
                "projects（恢复）/sub/b.txt"
            ]
        );
        assert!(
            fixture
                .target
                .publishes
                .iter()
                .all(|publish| !publish.target_path.starts_with("projects/")),
            "没有任何调用指向被占用的原路径"
        );
    }

    #[tokio::test]
    async fn manifest_rows_outside_the_root_path_stop_the_restore() {
        let mut entries = projects_subtree();
        entries[1].path = "other/plan.md".to_owned();
        let mut fixture =
            fixture_with(FakeTrashRestoreStore::new().with_subtree(entries, entry_id(0x31)));

        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("清单与事实不一致就停写");
        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(!failure.is_committed());
        assert!(fixture.namespace.paths.is_empty());
        assert!(fixture.target.publishes.is_empty());
        assert!(fixture.leases.acquired.is_empty());
        assert!(fixture.store.writes.is_empty());
        assert!(
            fixture.store.recorded.is_empty(),
            "结构不一致时连 pending 都不登记"
        );
    }

    #[tokio::test]
    async fn duplicated_and_orphan_manifest_rows_stop_the_restore() {
        let mut duplicated = projects_subtree();
        duplicated.push(duplicated[1].clone());
        let mut fixture =
            fixture_with(FakeTrashRestoreStore::new().with_subtree(duplicated, entry_id(0x31)));
        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("同一条目出现两次");
        assert_eq!(failure.code(), "store_contract_violation");
        assert!(fixture.store.writes.is_empty());

        let mut orphan = projects_subtree();
        orphan[3].parent = entry_id(0x77);
        let mut fixture =
            fixture_with(FakeTrashRestoreStore::new().with_subtree(orphan, entry_id(0x31)));
        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("父目录不在清单里");
        assert_eq!(failure.code(), "store_contract_violation");
        assert!(fixture.target.publishes.is_empty());
        assert!(fixture.store.writes.is_empty());
    }

    #[tokio::test]
    async fn transaction_reporting_fewer_entries_stops_the_restore() {
        let mut fixture = projects_fixture();
        fixture.store.record = Some(TrashRestoreRecord {
            intents: expected_intents(request().intent_seed),
            phase: PutPhase::Finalized,
            tx: tx_id(0x74),
            result: CommittedTrashRestore {
                op: key().op,
                root: key().root,
                entry: entry_id(0x50),
                path: "projects".to_owned(),
                name: "projects".to_owned(),
                renamed: false,
                revision: revision(8),
                seq: root_seq(14),
                entries: 1,
            },
        });

        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("事务回报的条目数与清单不符");
        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert_eq!(fixture.leases.released.len(), 3, "证据不一致时也要解除租约");
    }

    #[tokio::test]
    async fn entry_no_longer_in_trash_is_reported() {
        let mut fixture = fixture(trashed(EntryKind::File));
        fixture.store.subtrees.clear();
        let failure = run(&mut fixture, &request()).await.expect_err("清单不存在");
        assert_eq!(failure.code(), "restore_not_in_trash");
        assert!(fixture.store.writes.is_empty());
    }

    #[tokio::test]
    async fn expired_trash_entry_reports_version_gone() {
        let mut facts = trashed(EntryKind::File);
        facts.expires_at = user_time(950);
        let mut fixture = fixture(facts);
        let failure = run(&mut fixture, &request()).await.expect_err("保留期已过");
        assert_eq!(failure.code(), "version_gone");
        assert_eq!(failure.error_code(), Some(ErrorCode::VersionGone));
    }

    #[tokio::test]
    async fn one_expired_manifest_row_stops_the_whole_subtree() {
        let mut entries = projects_subtree();
        entries[4].expires_at = user_time(950);
        let mut fixture =
            fixture_with(FakeTrashRestoreStore::new().with_subtree(entries, entry_id(0x31)));
        let failure = run(&mut fixture, &subtree_request())
            .await
            .expect_err("清单里有一行已过期");
        assert_eq!(failure.code(), "version_gone");
        assert!(fixture.namespace.paths.is_empty());
        assert!(fixture.target.publishes.is_empty());
        assert!(fixture.store.writes.is_empty());
    }

    #[tokio::test]
    async fn stale_revision_is_a_base_mismatch() {
        let mut fixture = fixture(trashed(EntryKind::File));
        let mut stale = request();
        stale.expected_revision = revision(6);
        let failure = run(&mut fixture, &stale)
            .await
            .expect_err("清单 revision 不符");
        assert_eq!(failure.code(), "base_mismatch");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
    }

    #[tokio::test]
    async fn missing_parent_is_a_subtree_change() {
        let mut fixture = fixture(trashed(EntryKind::File));
        fixture.store.parents.clear();
        let failure = run(&mut fixture, &request())
            .await
            .expect_err("父目录不存在");
        assert_eq!(failure.code(), "subtree_changed");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
    }

    #[test]
    fn alternate_names_keep_the_extension() {
        assert_eq!(alternate_name("report.docx", 1), "report（恢复）.docx");
        assert_eq!(alternate_name("report.docx", 2), "report（恢复 2）.docx");
        assert_eq!(alternate_name(".gitignore", 1), ".gitignore（恢复）");
        assert_eq!(alternate_name("README", 3), "README（恢复 3）");
    }

    #[test]
    fn intent_identities_are_stable_and_distinct_per_file() {
        let seed = intent_id(0x42);
        assert_eq!(intent_for(seed, 0).expect("序号可表示"), seed);
        assert_eq!(
            intent_for(seed, 2).expect("序号可表示"),
            intent_for(seed, 2).expect("序号可表示"),
            "同一序号派生同一身份（可重入）"
        );
        assert_ne!(
            intent_for(seed, 1).expect("序号可表示"),
            intent_for(seed, 2).expect("序号可表示")
        );
        assert_eq!(
            &intent_for(seed, 1).expect("序号可表示").as_bytes()[..8],
            &seed.as_bytes()[..8],
            "只动最后 8 字节，保留种子前缀"
        );

        let saturated = IntentId::from_bytes([0xff; ID_LEN]);
        let failure = intent_for(saturated, 1).expect_err("序号溢出");
        assert_eq!(failure.code(), "invalid_request");
    }

    #[test]
    fn staging_names_are_derived_from_the_intent_identity() {
        let intent = intent_id(0x42);
        assert_eq!(
            staging_name(intent),
            format!(".restore-{}", intent.to_hex())
        );
        assert!(ObjectLayout::validate_staging_name(&staging_name(intent)).is_ok());
        assert_ne!(
            staging_name(intent),
            staging_name(intent_for(intent, 1).expect("序号可表示"))
        );
    }

    #[test]
    fn planning_orders_directories_before_their_contents() {
        let subtree = TrashSubtree {
            entries: projects_subtree(),
        };
        let plan = plan_subtree(&subtree, "projects").expect("清单结构自洽");
        assert_eq!(
            plan.iter()
                .map(|planned| planned.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "projects",
                "projects/plan.md",
                "projects/sub",
                "projects/sub/a.txt",
                "projects/sub/b.txt"
            ]
        );
        assert_eq!(
            plan.iter().map(PlannedEntry::depth).collect::<Vec<_>>(),
            vec![0, 1, 1, 2, 2]
        );

        // 另名只改变位置前缀，不改子树内部结构。
        let renamed = plan_subtree(&subtree, "projects（恢复）").expect("清单结构自洽");
        assert_eq!(renamed[3].path, "projects（恢复）/sub/a.txt");
        assert_eq!(renamed[3].name, "a.txt");
    }
}
