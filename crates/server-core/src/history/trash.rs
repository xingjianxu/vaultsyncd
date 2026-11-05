//! 回收站恢复：把一个回收站条目放回命名空间（design §10.3、T13、I06）。
//!
//! 与历史版本恢复共用同一套提交协调（Root 票据、意图、受控发布、单一事务），差别在“内容从哪来”
//! 和“实际位置怎么定”：
//!
//! 1. **清单即事实**：恢复只按回收站清单里的原父目录、原路径、删除时 revision 与内容版本进行；
//!    清单已不在（被恢复过/内容过期回收）就报 `restore_not_in_trash`，不猜一条路径；
//! 2. **不覆盖现在的新文件**（T13）：原位置的名称已被**活跃**条目占用时，在同一父目录里另名恢复
//!    （`名称（恢复）.扩展名`、`名称（恢复 2）.扩展名`…），并把实际名称与路径放进结果；事务里再核对
//!    一次占用，并发新建同样只会让本次恢复失败，不会覆盖；
//! 3. **文件放回内容、目录放回位置**：文件按清单里的内容版本重新发布到实际路径（发布前建立内容
//!    租约），目录用 `mkdir` 建回——两者都不覆盖已存在的路径；
//! 4. **单一事务**：清软删除、改回父目录与名称、推进 revision、删清单行、事件批次、操作结果、
//!    意图收尾一次提交；事务没提交就没有 ACK。
//!
//! 恢复不改条目身份：清单里的 `EntryId` 恢复后不变，客户端因此不会看到一个“新条目”。
//!
//! 范围限制（明确不做假的实现）：带后代的大目录子树需要“按深度建目录 + 逐文件发布”的批量编排，
//! 本卡只覆盖单条目（文件、空目录）。带后代的清单返回 `restore_subtree_unsupported`，不部分恢复，
//! 也不假装成功；批量编排的下落见 `agent/tasks/S040.md` 的“限制”。

use vaultsyncd_domain::ids::{EntryId, IntentId, OpId, RootId};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use crate::commit::intent::{NewPutIntent, TargetAfter, TargetBefore, publish_put};
use crate::commit::put::{RootCommitGate, target_name, validate_name, validate_target_path};
use crate::history::{
    LeaseDisposition, RestoreFailure, RestoreFailureReason, reclassify_publish_failure,
    release_after_failure,
};
use crate::ports::conflict_store::{OperationKey, RequestDigest};
use crate::ports::content_lease::{ContentLeasePort, ContentLeasePurpose, ContentLeaseRequest};
use crate::ports::intent_log::IntentLog;
use crate::ports::namespace::NamespacePort;
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
    /// 意图身份（由调用方生成，重试必须复用同一个身份）。目录没有内容，不需要意图。
    pub intent_id: IntentId,
    /// 幂等键：同一 `OpId` 与同一规范化请求只产生一次恢复（design I06）。
    pub op: OperationKey,
    /// 本次请求的规范化摘要（必须覆盖被恢复条目与清单 revision）。
    pub digest: RequestDigest,
    /// 回收站里的条目（文件或目录）。
    pub entry: EntryId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端看到的清单 revision（删除时的 revision）；与清单不一致说明列的是旧事实。
    pub expected_revision: Revision,
    /// 文件恢复时目标所在子卷内的暂存文件名（单个文件名）；目录恢复不用。
    pub staging_path: String,
}

impl TrashRestoreRequest {
    /// 在调用任何端口之前检查请求自身的形状。
    fn validate(&self) -> Result<(), Box<RestoreFailure>> {
        if self.staging_path.is_empty() {
            return Err(invalid_request("暂存文件名不能为空".to_owned()));
        }
        crate::objects::ObjectLayout::validate_staging_name(&self.staging_path)
            .map_err(|error| invalid_request(error.to_string()))?;
        Ok(())
    }
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
    /// 内容租约的处理方式（目录恢复没有内容租约，恒为 [`LeaseDisposition::Released`]）。
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

/// 恢复一个回收站条目（文件或目录）。
///
/// 顺序：请求形状 → Root 票据 → epoch → `OpId` 记录 → 清单与保留期 → 实际名称（占用则另名）→
/// 登记 pending → 目录创建或文件租约/意图/发布 → 元数据事务 → 核对证据 → 解除租约 → ACK。
///
/// # Errors
///
/// 见 [`RestoreFailure`]：请求不成立、Root 忙、epoch 变化、同一 `OpId` 不同请求、记录仍是 pending、
/// 清单不存在或已过期、父目录不可用、没有可用另名、带后代子树暂不支持、租约/意图/发布/存储失败、
/// 存储契约不一致。失败都不是 committed，已持久化的意图与 `pending` 记录保留为恢复证据。
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
    request.validate()?;
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

    // 4. 实际位置：原父目录必须存在且是目录；名称被活跃条目占用就另名（T13）。
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

    // 5. 先登记 pending：并发的同一 OpId 只有一个能继续。
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

    // 6. 工作目录：目录建回位置，文件按清单里的内容版本发布回位置。
    let mut lease = None;
    let mut intents: Vec<IntentId> = Vec::new();
    match root.kind {
        EntryKind::Directory => {
            if let Err(error) = ports.namespace.create_directory(&path) {
                let failure = RestoreFailure::new(RestoreFailureReason::Namespace(error));
                return Err(Box::new(failure));
            }
        }
        EntryKind::File => {
            let restored = publish_restored_file(ports, at, request, &root, &path).await;
            match restored {
                Ok((established, intent)) => {
                    lease = Some(established);
                    intents.push(intent);
                }
                Err(boxed) => {
                    let (failure, position) = *boxed;
                    return Err(Box::new(match position {
                        Some(established) => {
                            release_after_failure(ports.leases, &established, failure).await
                        }
                        None => failure,
                    }));
                }
            }
        }
    }

    // 7. 单一事务：清软删除、改位置、推进 revision、删清单行、事件批次、结果、意图收尾。
    let write = TrashRestoreWrite {
        intents,
        key: request.op,
        digest: request.digest,
        entry: request.entry,
        expected_revision: root.revision,
        path: path.clone(),
        name: name.clone(),
        renamed,
        entries: vec![TrashRestoreEntry {
            entry: root.entry,
            parent: root.parent,
            name: name.clone(),
            path: path.clone(),
            kind: root.kind,
            content_version: root.content_version,
            expected_revision: root.revision,
        }],
        at,
    };
    let record = match ports.store.apply_trash_restore(&write).await {
        Ok(record) => record,
        Err(error) => {
            let failure = RestoreFailure::new(trash_failure_after_transaction(error));
            return Err(Box::new(match &lease {
                Some(established) => {
                    release_after_failure(ports.leases, established, failure).await
                }
                None => failure,
            }));
        }
    };
    check_record(&record, request, &write)?;

    let disposition = match &lease {
        Some(established) => release_lease(ports.leases, established).await,
        None => LeaseDisposition::Released,
    };
    Ok(RestoredTrash {
        result: record.result,
        lease: disposition,
    })
}

/// 文件恢复的工作目录部分：内容租约 → 现场必须空缺 → 意图 → 发布。
///
/// 返回已建立的租约与新意图；失败时把“租约是否已建”一起交回调用方，让清理路径不丢证据。
async fn publish_restored_file<S, L, T, N, C>(
    ports: &mut TrashRestorePorts<'_, S, L, T, N, C>,
    at: UtcTimestamp,
    request: &TrashRestoreRequest,
    root: &crate::ports::restore_store::TrashEntryFacts,
    path: &str,
) -> Result<
    (crate::ports::content_lease::ContentLease, IntentId),
    Box<(
        RestoreFailure,
        Option<crate::ports::content_lease::ContentLease>,
    )>,
>
where
    S: TrashRestoreStore + ?Sized,
    L: IntentLog + ?Sized,
    T: TargetPort + ?Sized,
    N: NamespacePort + ?Sized,
    C: ContentLeasePort + ?Sized,
{
    let Some(version) = root.content_version else {
        return Err(Box::new((
            RestoreFailure::new(RestoreFailureReason::StoreContract {
                detail: format!("普通文件 {} 在清单里没有内容版本", root.entry),
            }),
            None,
        )));
    };
    let content = match ports.store.content_facts(request.op.root, version).await {
        Ok(Some(content)) => content,
        Ok(None) => {
            return Err(Box::new((
                RestoreFailure::new(RestoreFailureReason::VersionGone {
                    detail: format!("版本 {version} 的内容已不可读"),
                }),
                None,
            )));
        }
        Err(error) => return Err(Box::new((restore_failure(error), None))),
    };

    // 内容租约：把“读到内容版本”与“新位置引用同一对象”之间的窗口保护起来。
    let lease = ports
        .leases
        .acquire(&ContentLeaseRequest {
            root: request.op.root,
            digest: content.digest,
            purpose: ContentLeasePurpose::Download,
            expires_at: lease_expiry(at).map_err(|failure| Box::new((*failure, None)))?,
        })
        .await
        .map_err(|error| {
            Box::new((
                RestoreFailure::new(RestoreFailureReason::Lease(error)),
                None,
            ))
        })?;

    // 现场必须空缺：恢复不覆盖现在的新文件（T13）。
    let observed = match ports.target.observe(path) {
        Ok(observed) => observed,
        Err(error) => {
            return Err(Box::new((
                RestoreFailure::new(RestoreFailureReason::TargetUnreadable(error)),
                Some(lease),
            )));
        }
    };
    let vacancy = TargetBefore::Absent;
    if !vacancy.satisfied_by(&observed) {
        return Err(Box::new((
            RestoreFailure::new(RestoreFailureReason::Reconcile {
                expected: vacancy.to_json(),
                observed,
            }),
            Some(lease),
        )));
    }

    let new = NewPutIntent {
        intent_id: request.intent_id,
        root: request.op.root,
        target_path: path.to_owned(),
        staging_path: request.staging_path.clone(),
        before: TargetBefore::Absent,
        after: TargetAfter {
            digest: content.digest,
            size: content.bytes,
        },
    };
    match publish_put(ports.log, ports.target, at, &new).await {
        Ok(_) => Ok((lease, request.intent_id)),
        Err(failure) => Err(Box::new((
            reclassify_publish_failure(*failure),
            Some(lease),
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
    if subtree.entries.len() > 1 {
        return Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::UnsupportedSubtree {
                detail: format!(
                    "条目 {} 的清单有 {} 行，带后代的大目录恢复尚未接入",
                    root.entry,
                    subtree.entries.len()
                ),
            },
        )));
    }
    match root.kind {
        EntryKind::File if root.content_version.is_none() => Err(Box::new(RestoreFailure::new(
            RestoreFailureReason::StoreContract {
                detail: format!("普通文件 {} 在清单里没有内容版本", root.entry),
            },
        ))),
        EntryKind::Directory if root.content_version.is_some() => Err(Box::new(
            RestoreFailure::new(RestoreFailureReason::StoreContract {
                detail: format!("目录 {} 在清单里带了内容版本", root.entry),
            }),
        )),
        _ => Ok(()),
    }
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

async fn release_lease<C>(
    leases: &mut C,
    lease: &crate::ports::content_lease::ContentLease,
) -> LeaseDisposition
where
    C: ContentLeasePort + ?Sized,
{
    match leases.release(lease).await {
        Ok(()) => LeaseDisposition::Released,
        Err(_) => LeaseDisposition::Expires {
            at: lease.expires_at,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vaultsyncd_domain::error::ErrorCode;

    use crate::ports::put_store::EntryKind;
    use crate::ports::restore_store::TrashEntryFacts;
    use crate::test_support::{
        FakeContentLease, FakeIntentLog, FakeNamespace, FakeTarget, FakeTrashRestoreStore, Trace,
        device_id, digest, entry_id, epoch, intent_id, op_id, request_digest, revision, root_id,
        user_time, version_id,
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

    fn request() -> TrashRestoreRequest {
        TrashRestoreRequest {
            intent_id: intent_id(0x42),
            op: OperationKey {
                root: root_id(0x21),
                device: device_id(0x61),
                op: op_id(0x72),
            },
            digest: request_digest(0x03),
            entry: entry_id(0x51),
            expected_epoch: epoch(3),
            expected_revision: revision(7),
            staging_path: ".staging-restore".to_owned(),
        }
    }

    fn fixture(facts: TrashEntryFacts) -> Fixture {
        let trace = Trace::default();
        let mut store = FakeTrashRestoreStore::new().with_trashed(facts, entry_id(0x31));
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

        // 事务：同一意图收尾，条目身份不变。
        let write = &fixture.store.writes[0];
        assert_eq!(write.intents, vec![request.intent_id]);
        assert_eq!(write.entries.len(), 1);
        assert_eq!(write.entries[0].entry, request.entry);
        assert_eq!(write.entries[0].expected_revision, revision(7));
    }

    #[tokio::test]
    async fn occupied_name_restores_under_an_alternate_name() {
        let mut fixture = fixture(trashed(EntryKind::File));
        fixture.store = fixture
            .store
            .occupying(entry_id(0x31), "report.docx", entry_id(0x99));
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
        assert!(
            fixture
                .trace
                .entries()
                .contains(&"namespace mkdir docs/report.docx".to_owned()),
            "目录要在工作目录建回来：{:?}",
            fixture.trace.entries()
        );
    }

    #[tokio::test]
    async fn subtree_with_descendants_is_refused_instead_of_partially_restored() {
        let mut fixture = fixture(trashed(EntryKind::Directory));
        let mut subtree = fixture.store.subtrees.remove(&entry_id(0x51)).unwrap();
        subtree.entries.push(TrashEntryFacts {
            entry: entry_id(0x52),
            parent: entry_id(0x51),
            path: "docs/report.docx/inner.txt".to_owned(),
            kind: EntryKind::File,
            revision: revision(4),
            content_version: Some(version_id(0x82)),
            deleted_at: user_time(900),
            expires_at: user_time(2_000_000),
        });
        fixture.store.subtrees.insert(entry_id(0x51), subtree);

        let failure = run(&mut fixture, &request())
            .await
            .expect_err("批量子树尚未接入");
        assert_eq!(failure.code(), "restore_subtree_unsupported");
        assert_eq!(failure.error_code(), Some(ErrorCode::UnsupportedEntry));
        assert!(fixture.store.writes.is_empty());
        assert!(fixture.target.publishes.is_empty());
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

    #[tokio::test]
    async fn repeated_op_id_replays_the_first_result() {
        let mut fixture = fixture(trashed(EntryKind::File));
        let request = request();
        let first = run(&mut fixture, &request).await.expect("首次恢复成功");
        let second = run(&mut fixture, &request).await.expect("重放应当成功");
        assert_eq!(first.result, second.result);
        assert_eq!(fixture.store.writes.len(), 1, "重放不得再做一次事务");
        assert_eq!(fixture.target.publishes.len(), 1, "重放不得再发布一次");
    }

    #[test]
    fn alternate_names_keep_the_extension() {
        assert_eq!(alternate_name("report.docx", 1), "report（恢复）.docx");
        assert_eq!(alternate_name("report.docx", 2), "report（恢复 2）.docx");
        assert_eq!(alternate_name(".gitignore", 1), ".gitignore（恢复）");
        assert_eq!(alternate_name("README", 3), "README（恢复 3）");
    }
}
