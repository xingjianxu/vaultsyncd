//! 移动冲突规划（`docs/design.md` §9.5、§10.1 的移动行、T09/T10、I03）。
//!
//! 本模块只接“至少一侧改了位置、且类型没变”的输入：同 Root 内移动保持 [`EntryId`]（路径改变不影响
//! 身份，I03），移动目录只改父目录关系、不给子文件重新传输内容（design §9.5）。输出是一份**主条目
//! 计划**加可选的迟到分支：
//!
//! | 输入 | 结果 |
//! | --- | --- |
//! | 仅远端移动（本地未变） | [`Action::ApplyRemoteMove`]：本地把该身份放到已提交位置，不下载内容（T09） |
//! | 仅本地移动（远端就是基线） | [`Action::UploadMove`] / [`Action::UploadMoveWithContent`]：提交目标位置与源修订 |
//! | 双方到达同一位置且内容相同 | [`Action::MergeSameResult`]：幂等归并，不来回移动（design §10.1） |
//! | 双方移动到不同位置 | 主条目保留已提交位置，迟到目标成为稳定身份的冲突副本 |
//! | 一端移动、另一端修改 | 保守双副本：已提交的位置/内容为主，迟到分支保留其内容与意图位置 |
//!
//! [`Action::ApplyRemoteMove`]: crate::reconcile::Action::ApplyRemoteMove
//! [`Action::UploadMove`]: crate::reconcile::Action::UploadMove
//! [`Action::UploadMoveWithContent`]: crate::reconcile::Action::UploadMoveWithContent
//! [`Action::MergeSameResult`]: crate::reconcile::Action::MergeSameResult
//!
//! 三条判定立场：
//!
//! 1. **胜者只看服务器提交序**：远端事实就是已提交状态，主条目保留它；`observed_mtime` 只随事实记录，
//!    交换两台设备的时钟不改变计划（I04）。
//! 2. **`EntryId` 不随路径变**：主条目的动作一律携带基线身份，只有迟到分支拿新的冲突身份；跨 Root 移动
//!    是明确例外（design §9.5 把它当“目标创建 + 源删除”的可恢复任务，见 [`plan_cross_root_move`]）。
//! 3. **前提不成立就返回重规划结果**：目录环、目标位置已有别的条目、目标祖先已删除、远端身份被替换都给出
//!    [`MoveOutcome::Replan`]，不生成可执行动作，也不覆盖任何东西（design §10.1 末两行）。
//!
//! 本模块不做 I/O：不枚举同级条目（目标位置的观察由 [`MoveTargets`] 传入）、不读时钟、不分配身份、不做
//! CAS。远端已删除时的移动意图不在这里裁决：那是删除与恢复冲突（[`crate::delete`]，design §10.1）。

use core::fmt;

use vaultsyncd_domain::ids::{EntryId, RootId};
use vaultsyncd_domain::version::Generation;
use vaultsyncd_fs_common::path::{PathRelation, RelativePath, relation};

use crate::conflict::{
    CommittedConflict, ConflictCopy, ConflictNameError, ConflictNameRequest, conflict_entry_path,
    create_copy_plan,
};
use crate::delete::SubtreeToken;
use crate::reconcile::{
    Action, Dependency, EntryKind, LocalFact, LocalProtection, Plan, Prerequisite, ServerFact,
    apply_remote, merge_same_result,
};

/// 移动目标位置的现场观察（design §10.1 末两行）。
///
/// planner 不枚举同级条目，也看不到目录树：目标是否被占用、祖先是否还在，由扫描或执行器观察后以值传入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetState {
    /// 目标位置为空且父目录已同步存在：可以使用这个名称。
    Vacant,
    /// 目标位置已有别的条目：不覆盖，另名重新规划（design §10.1）。
    Occupied,
    /// 目标祖先目录已删除或缺失：不覆盖，先恢复目录或另选位置（design §10.1）。
    AncestorMissing,
}

/// 两侧目标位置的观察。
///
/// 一边的移动计划分两类执行：本地要落位的计划按 [`MoveTargets::local`] 判定，要提交到服务器的计划
/// （上传移动、开迟到分支）按 [`MoveTargets::server`] 判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveTargets {
    /// 本地工作目录对“本地要落到哪个位置”的观察。
    pub local: TargetState,
    /// 服务端对“提交目标/新建迟到分支的位置”的观察。
    pub server: TargetState,
}

/// 迟到分支另名与幂等所需的输入，复用内容冲突的身份与命名规则（design §10.1—10.2、I06）。
#[derive(Clone, Copy, Debug)]
pub struct LateBranch<'a> {
    /// 服务端为本次操作首次分配的冲突条目身份；重试沿用同一值。
    pub entry_id: EntryId,
    /// 冲突名称的组成输入（`OpId`、设备短标识、服务器时间、预算）。
    pub naming: ConflictNameRequest<'a>,
    /// 同一 `OpId` 已记录的提交结果；首次提交为 `None`。
    pub committed: Option<&'a CommittedConflict>,
}

/// 同 Root 移动的规划输入：三方事实都已绑定到同一 `EntryId`（本地身份关联由客户端数据库保证）。
#[derive(Clone, Copy, Debug)]
pub struct MoveRequest<'a> {
    /// 共同基线 B：身份与已提交位置。
    pub base: &'a ServerFact,
    /// 本地当前观察；路径相对基线变化即“本地移动”。
    pub local: &'a LocalFact,
    /// 远端当前事实 R：已提交的位置与内容。
    pub remote: &'a ServerFact,
    /// 两侧目标位置的现场观察。
    pub targets: MoveTargets,
    /// 迟到分支的身份与命名：只有需要保留本地内容/意图位置时才用；单侧移动可以传 `None`。
    pub late: Option<&'a LateBranch<'a>>,
}

/// 需要重新规划的原因（不生成可执行动作）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplanReason {
    /// 目标落在源目录自己的子树内（目录环），任何平台都不允许。
    DirectoryCycle,
    /// 目标位置已有别的条目：不覆盖目标，另名重新规划（design §10.1）。
    TargetOccupied,
    /// 目标祖先已删除或缺失：先恢复目录或另选位置（design §10.1）。
    TargetAncestorMissing,
    /// 远端该路径上的身份已不是基线身份：原条目被删除后新建属于新身份，按新事实重新规划（I03）。
    IdentityReplaced,
    /// 目录的并发移动需要目录级冲突记录（design §10.1“目录冲突记录”），本卡不给文件副本计划。
    DirectoryConflictRequired,
}

/// 移动计划：主条目保留已提交的位置与内容，迟到分支（若有）保留其内容与意图位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovePlan {
    /// 主条目计划。`Plan::path` 是该身份的基线位置；目标位置在动作里（归并计划两者相同，不带动作目标）。
    pub main: Plan,
    /// 迟到分支：新冲突条目或同一 `OpId` 的既有结果；单侧移动为 `None`。
    pub late: Option<ConflictCopy>,
}

/// 移动规划的完整结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveOutcome {
    /// 可执行计划（装箱：计划值远大于重规划原因）。
    Planned(Box<MovePlan>),
    /// 输入前提不成立：按新事实重新规划，不生成可执行动作，也不改动任何内容。
    Replan(ReplanReason),
}

/// 移动规划被拒绝的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveError {
    /// 两侧路径都没变：不是移动输入，应走 [`crate::reconcile`] 的基本分支；
    /// 类型变化（文件↔目录）同样不归本模块。
    NotAMove,
    /// 需要保留迟到分支的内容/意图位置，但没有给出冲突身份与命名。
    LateBranchMissing,
    /// 同一 `OpId` 已记录的冲突结果与本次请求不同（身份、操作或内容不一致）：按 I06 拒绝。
    OpIdRequestChanged,
    /// 自称内容变化却给不出文件内容版本：事实不完整，不生成缺少下载前提的计划。
    IncompleteFacts,
    /// 跨 Root 移动的源是目录，却没有携带删除所需的子树 token（design §10.3）。
    MissingSubtreeToken,
    /// 跨 Root 移动的源是文件，却带了目录的子树 token：删除语义不成立。
    UnexpectedSubtreeToken,
    /// 跨 Root 移动入口收到了同一个 Root 的两侧：同 Root 移动保持 `EntryId`，应走 [`plan_move`]。
    SameRoot,
    /// 冲突名称无法在预算内生成。
    Name(ConflictNameError),
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAMove => f.write_str("输入不是移动（位置与类型都没变），不生成移动计划"),
            Self::LateBranchMissing => f.write_str("需要保留迟到分支，但缺少冲突条目身份与命名"),
            Self::OpIdRequestChanged => {
                f.write_str("同一 OpId 已记录的冲突结果与本次请求不同，按幂等规则拒绝")
            }
            Self::IncompleteFacts => f.write_str("内容变化缺少文件版本，事实不完整"),
            Self::MissingSubtreeToken => f.write_str("跨 Root 移动的目录源缺少子树 token"),
            Self::UnexpectedSubtreeToken => f.write_str("跨 Root 移动的文件源不应携带子树 token"),
            Self::SameRoot => f.write_str("跨 Root 移动要求源与目标 Root 不同"),
            Self::Name(error) => write!(f, "冲突名称生成失败：{error}"),
        }
    }
}

impl core::error::Error for MoveError {}

/// 规划一次同 Root 移动（design §9.5、§10.1 的移动行）。
///
/// 主条目永远是已提交的远端事实；本地相对基线改过位置或内容时，它的内容与意图位置通过 [`MovePlan::late`]
/// 保留下来（design §10.1“首发保守保留两种候选”）。
///
/// # Errors
///
/// 见 [`MoveError`]：输入不是移动、缺少迟到分支输入、同一 `OpId` 请求不一致、内容缺少版本、
/// 或冲突名称无法在预算内生成。
pub fn plan_move(request: &MoveRequest<'_>) -> Result<MoveOutcome, MoveError> {
    let MoveRequest {
        base,
        local,
        remote,
        targets,
        late,
    } = *request;

    if remote.entry_id != base.entry_id {
        return Ok(MoveOutcome::Replan(ReplanReason::IdentityReplaced));
    }
    if !same_type(local.kind, base.kind) || !same_type(remote.kind, base.kind) {
        // 类型互换（文件↔目录）由首次合并的保留双方规则裁决，不是移动。
        return Err(MoveError::NotAMove);
    }

    let local_moved = local.path != base.path;
    let remote_moved = remote.path != base.path;
    if !local_moved && !remote_moved {
        return Err(MoveError::NotAMove);
    }

    if let Some(reason) = directory_cycle(
        base,
        local_moved.then_some(&local.path),
        remote_moved.then_some(&remote.path),
    ) {
        return Ok(MoveOutcome::Replan(reason));
    }

    let local_changed = local_moved || local.kind != base.kind;
    let remote_changed = remote_moved || remote.kind != base.kind;
    match (local_changed, remote_changed) {
        // 只有远端变了：应用已提交的移动（内容没变就不下载）。
        (false, true) if remote_moved => {
            if let Some(reason) = replan(targets.local) {
                return Ok(MoveOutcome::Replan(reason));
            }
            Ok(MoveOutcome::Planned(Box::new(MovePlan {
                main: apply_committed(base, local, remote)?,
                late: None,
            })))
        }
        // 只有本地变了：提交移动（内容也变了才带快照）。
        (true, false) => {
            if let Some(reason) = replan(targets.server) {
                return Ok(MoveOutcome::Replan(reason));
            }
            Ok(MoveOutcome::Planned(Box::new(MovePlan {
                main: upload_move(base, local),
                late: None,
            })))
        }
        (true, true) => plan_both_changed(base, local, remote, targets, late),
        // 两侧都没变（或只有内容变化、没有移动）：不是移动输入。
        _ => Err(MoveError::NotAMove),
    }
}

/// 双方都变了：能证明同一结果就归并，否则保守保留双方（design §10.1）。
fn plan_both_changed(
    base: &ServerFact,
    local: &LocalFact,
    remote: &ServerFact,
    targets: MoveTargets,
    late: Option<&LateBranch<'_>>,
) -> Result<MoveOutcome, MoveError> {
    if local.path == remote.path && local.kind == remote.kind {
        // 两端到达同一位置且内容相同：幂等归并，不再移动（design §10.1）。
        return Ok(MoveOutcome::Planned(Box::new(MovePlan {
            main: merge_same_result(remote.path.clone(), base, local, remote),
            late: None,
        })));
    }

    let late = late.ok_or(MoveError::LateBranchMissing)?;
    if !matches!(local.kind, EntryKind::File(_)) {
        // 目录迟到分支需要目录级冲突记录，不是一份文件副本计划。
        return Ok(MoveOutcome::Replan(ReplanReason::DirectoryConflictRequired));
    }
    if let Some(reason) = replan(targets.local).or_else(|| replan(targets.server)) {
        return Ok(MoveOutcome::Replan(reason));
    }

    let mut main = apply_committed(base, local, remote)?;
    let copy = late_copy(local, remote, late)?;
    main.dependencies.push(Dependency::ConflictCopyCommitted {
        entry_id: late.entry_id,
    });
    Ok(MoveOutcome::Planned(Box::new(MovePlan {
        main,
        late: Some(copy),
    })))
}

/// 应用远端已提交的位置与内容：同一位置时复用基本分支，位置不同时先移动本地身份。
fn apply_committed(
    base: &ServerFact,
    local: &LocalFact,
    remote: &ServerFact,
) -> Result<Plan, MoveError> {
    if local.path == remote.path {
        let plan = apply_remote(remote.path.clone(), base, local, remote);
        return match plan.action {
            Action::ApplyRemote { .. } => Ok(plan),
            // 内容变化却给不出内容版本：不降级成“只推进元数据”。
            _ => Err(MoveError::IncompleteFacts),
        };
    }

    let content_changed = remote.kind != local.kind;
    let version = if content_changed {
        Some(remote.version.ok_or(MoveError::IncompleteFacts)?)
    } else {
        None
    };
    let dependencies = version
        .map(|version| vec![Dependency::DownloadVersion { version }])
        .unwrap_or_default();
    let local_protection = if content_changed {
        // 替换本地字节前先固定快照，失败时保留旧内容（I02）。
        LocalProtection::SnapshotBeforeReplace {
            generation: local.generation,
        }
    } else {
        // 只改位置、不换内容：本地字节保持原样（T09）。
        LocalProtection::KeepLocalContent
    };
    Ok(Plan {
        path: base.path.clone(),
        action: Action::ApplyRemoteMove {
            entry_id: base.entry_id,
            revision: remote.revision,
            version,
            target: remote.path.clone(),
        },
        prerequisites: vec![
            Prerequisite::BaseRevisionCurrent {
                expected: base.revision,
            },
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
            Prerequisite::UsagePermitsChange,
            Prerequisite::TargetPathUsable,
        ],
        dependencies,
        local_protection,
    })
}

/// 本地移动待提交：携带源修订与目标位置，`EntryId` 不变（design §9.5、§15.2）。
fn upload_move(base: &ServerFact, local: &LocalFact) -> Plan {
    let content_changed = local.kind != base.kind;
    let prerequisites = vec![
        Prerequisite::BaseRevisionCurrent {
            expected: base.revision,
        },
        Prerequisite::GenerationUnchanged {
            expected: local.generation,
        },
        Prerequisite::TargetPathUsable,
    ];
    let (action, dependencies, local_protection) = if content_changed {
        (
            Action::UploadMoveWithContent {
                entry_id: base.entry_id,
                base_revision: base.revision,
                target: local.path.clone(),
                generation: local.generation,
            },
            vec![
                Dependency::ParentDirectoryReady,
                Dependency::SnapshotLocalContent {
                    generation: local.generation,
                },
            ],
            LocalProtection::UploadSnapshot {
                generation: local.generation,
            },
        )
    } else {
        (
            Action::UploadMove {
                entry_id: base.entry_id,
                base_revision: base.revision,
                target: local.path.clone(),
            },
            vec![Dependency::ParentDirectoryReady],
            LocalProtection::KeepLocalContent,
        )
    };
    Plan {
        path: base.path.clone(),
        action,
        prerequisites,
        dependencies,
        local_protection,
    }
}

/// 迟到分支：保留本地内容与意图位置（与主条目同位置时才加冲突后缀，design §10.1—10.2）。
fn late_copy(
    local: &LocalFact,
    remote: &ServerFact,
    late: &LateBranch<'_>,
) -> Result<ConflictCopy, MoveError> {
    let EntryKind::File(content) = local.kind else {
        return Err(MoveError::NotAMove);
    };
    if let Some(record) = late.committed {
        if record.op_id != late.naming.op_id
            || record.entry_id != late.entry_id
            || record.content != content
        {
            return Err(MoveError::OpIdRequestChanged);
        }
        return Ok(ConflictCopy::Reuse(record.clone()));
    }
    let path = if local.path == remote.path {
        // 目标就是已提交位置：主条目保留名称，迟到分支另名（design §10.1）。
        conflict_entry_path(&local.path, &late.naming).map_err(MoveError::Name)?
    } else {
        // 意图位置与主条目不同：保留用户选定的名称，占用与否由执行器核验后重新规划。
        local.path.clone()
    };
    Ok(ConflictCopy::Create(create_copy_plan(
        path,
        None,
        local,
        late.entry_id,
        late.naming.op_id,
    )))
}

/// 跨 Root 移动的输入（design §9.5：目标创建确认后再删源，不假装是原子 rename）。
///
/// 跨 Root 不保持 `EntryId`：目标 Root 的命名空间里身份不复用，因此调用方要给出服务端为目标分配的
/// [`EntryId`]，并保证源与目标 Root 不同。
#[derive(Clone, Copy, Debug)]
pub struct CrossRootMoveRequest<'a> {
    /// 源 Root 身份（只用于与目标 Root 做一致性检查）。
    pub source_root: RootId,
    /// 源 Root 的已提交事实：身份、修订与类型。
    pub source: &'a ServerFact,
    /// 目标 Root 身份。
    pub target_root: RootId,
    /// 目标 Root 内的位置。
    pub target_path: &'a RelativePath,
    /// 服务端为目标条目分配的身份；源删除依赖它已提交。
    pub target_entry_id: EntryId,
    /// 要上传到目标的本地内容代际。
    pub generation: Generation,
    /// 源 Root 删除所需的子树 token：目录必须为 `Some`，文件必须为 `None`（design §10.3）。
    pub source_subtree: Option<SubtreeToken>,
    /// 目标位置的服务端观察。
    pub target: TargetState,
}

/// 跨 Root 移动的两段计划。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrossRootMovePlan {
    /// 第一步：在目标 Root 新建条目。目标位置仍为空时才允许创建（`ServerPathVacant`）。
    pub create_target: Plan,
    /// 第二步：删除源条目。依赖 [`Dependency::CrossRootTargetCommitted`]：目标创建失败或结果未知时，
    /// 源删除不得计划完成。
    pub delete_source: Plan,
}

/// 跨 Root 移动的规划结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CrossRootOutcome {
    /// 两段计划：先创建目标、再删源（装箱：计划值远大于重规划原因）。
    Planned(Box<CrossRootMovePlan>),
    /// 目标位置前提不成立：重新规划；既不创建目标，也不计划删除源。
    Replan(ReplanReason),
}

/// 规划一次跨 Root 移动（design §9.5）。
///
/// # Errors
///
/// 见 [`MoveError`]：两侧是同一个 Root、目录源缺子树 token、文件源带了子树 token，或目标位置前提不成立
/// 时由调用方改用 [`CrossRootOutcome::Replan`]（那是结果，不是错误）。
pub fn plan_cross_root_move(
    request: &CrossRootMoveRequest<'_>,
) -> Result<CrossRootOutcome, MoveError> {
    let CrossRootMoveRequest {
        source_root,
        source,
        target_root,
        target_path,
        target_entry_id,
        generation,
        source_subtree,
        target,
    } = *request;

    if source_root == target_root {
        return Err(MoveError::SameRoot);
    }
    if let Some(reason) = replan(target) {
        return Ok(CrossRootOutcome::Replan(reason));
    }
    match (matches!(source.kind, EntryKind::Directory), source_subtree) {
        (true, None) => return Err(MoveError::MissingSubtreeToken),
        (false, Some(_)) => return Err(MoveError::UnexpectedSubtreeToken),
        _ => {}
    }

    let create_target = Plan {
        path: target_path.clone(),
        action: Action::UploadNewEntry { generation },
        prerequisites: vec![
            Prerequisite::GenerationUnchanged {
                expected: generation,
            },
            Prerequisite::TargetPathUsable,
            Prerequisite::ServerPathVacant,
        ],
        dependencies: vec![
            Dependency::ParentDirectoryReady,
            Dependency::SnapshotLocalContent { generation },
        ],
        local_protection: LocalProtection::UploadSnapshot { generation },
    };
    let delete_source = Plan {
        path: source.path.clone(),
        action: Action::DeleteEntry {
            entry_id: source.entry_id,
            base_revision: source.revision,
            subtree: source_subtree,
        },
        prerequisites: vec![Prerequisite::BaseRevisionCurrent {
            expected: source.revision,
        }],
        dependencies: vec![Dependency::CrossRootTargetCommitted {
            entry_id: target_entry_id,
        }],
        local_protection: LocalProtection::KeepLocalContent,
    };
    Ok(CrossRootOutcome::Planned(Box::new(CrossRootMovePlan {
        create_target,
        delete_source,
    })))
}

/// 目录环：目录被移进自己的子树（design §9.5 的目录移动前提）。
///
/// 按路径分量判定，不看字符串前缀（`docs` 与 `docs-old` 无关，`docs` 与 `docs/2026` 才是祖先关系）。
fn directory_cycle(
    base: &ServerFact,
    local_target: Option<&RelativePath>,
    remote_target: Option<&RelativePath>,
) -> Option<ReplanReason> {
    if !matches!(base.kind, EntryKind::Directory) {
        return None;
    }
    [local_target, remote_target]
        .into_iter()
        .flatten()
        .any(|target| relation(target, &base.path) == PathRelation::Descendant)
        .then_some(ReplanReason::DirectoryCycle)
}

/// 目标位置观察不成立时给出重规划原因。
fn replan(state: TargetState) -> Option<ReplanReason> {
    match state {
        TargetState::Vacant => None,
        TargetState::Occupied => Some(ReplanReason::TargetOccupied),
        TargetState::AncestorMissing => Some(ReplanReason::TargetAncestorMissing),
    }
}

/// 类型相同（文件/目录）判定：类型互换不归本模块。
fn same_type(left: EntryKind, right: EntryKind) -> bool {
    core::mem::discriminant(&left) == core::mem::discriminant(&right)
}

#[cfg(test)]
mod tests {
    use super::{
        CrossRootMoveRequest, CrossRootOutcome, LateBranch, MoveError, MoveOutcome, MoveRequest,
        MoveTargets, ReplanReason, TargetState, plan_cross_root_move, plan_move,
    };
    use crate::conflict::{
        CommittedConflict, ConflictCopy, ConflictNameRequest, NameBudget, ShortIdLen,
    };
    use crate::delete::SubtreeToken;
    use crate::reconcile::{
        Action, ContentFingerprint, Dependency, LocalFact, LocalProtection, Prerequisite,
        ServerFact,
    };
    use vaultsyncd_domain::ids::{EntryId, OpId, RootId, VersionId};
    use vaultsyncd_domain::version::{Generation, Revision, UtcTimestamp};
    use vaultsyncd_fs_common::path::RelativePath;

    const ENTRY: EntryId = EntryId::from_bytes([0x11; 16]);
    const OTHER_ENTRY: EntryId = EntryId::from_bytes([0x22; 16]);
    const COPY_ENTRY: EntryId = EntryId::from_bytes([0xc0; 16]);
    const TARGET_ENTRY: EntryId = EntryId::from_bytes([0x33; 16]);
    const VERSION_OLD: VersionId = VersionId::from_bytes([0xa1; 16]);
    const VERSION_NEW: VersionId = VersionId::from_bytes([0xb2; 16]);
    const OP: OpId = OpId::from_bytes([0x5a; 16]);
    const OTHER_OP: OpId = OpId::from_bytes([0x5b; 16]);
    const SOURCE_ROOT: RootId = RootId::from_bytes([0x91; 16]);
    const TARGET_ROOT: RootId = RootId::from_bytes([0x92; 16]);
    const SOURCE_TOKEN: SubtreeToken = SubtreeToken::from_bytes([0x7e; 16]);
    /// 2026-11-05T08:30:00Z。
    const SERVER_TIME: i64 = 1_793_867_400;
    const DEVICE_LABEL: &str = "3f7c9a01";
    const EXPECTED_NAME: &str = "report（冲突-3f7c9a01-20261105T083000Z-5a5a5a5a）.txt";

    fn path(text: &str) -> RelativePath {
        RelativePath::parse(text).expect("测试路径合法")
    }

    fn content(digest: u8, size: u64) -> ContentFingerprint {
        ContentFingerprint::new(size, [digest; 32])
    }

    fn revision(value: u64) -> Revision {
        Revision::new(value).expect("测试修订在范围内")
    }

    fn generation(value: u64) -> Generation {
        Generation::new(value).expect("测试代际在范围内")
    }

    fn mtime(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("测试时间在范围内")
    }

    fn naming() -> ConflictNameRequest<'static> {
        ConflictNameRequest {
            device_label: DEVICE_LABEL,
            op_id: OP,
            server_time: mtime(SERVER_TIME),
            budget: NameBudget::CROSS_PLATFORM,
            short_id_len: ShortIdLen::DEFAULT,
        }
    }

    fn base_file() -> ServerFact {
        ServerFact::file(
            path("docs/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(4),
            VERSION_OLD,
        )
    }

    fn base_dir() -> ServerFact {
        ServerFact::directory(path("docs"), ENTRY, revision(4))
    }

    fn local_file_at(text: &str, digest: u8) -> LocalFact {
        LocalFact::file(path(text), content(digest, 120), generation(9))
    }

    fn late_branch() -> LateBranch<'static> {
        LateBranch {
            entry_id: COPY_ENTRY,
            naming: naming(),
            committed: None,
        }
    }

    fn vacant() -> MoveTargets {
        MoveTargets {
            local: TargetState::Vacant,
            server: TargetState::Vacant,
        }
    }

    #[test]
    fn remote_move_applies_without_downloading_content() {
        let base = base_file();
        let local = local_file_at("docs/report.txt", 0x31);
        let remote = ServerFact::file(
            path("docs/report-final.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: None,
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("仅远端移动应给出计划");
        };
        assert_eq!(plan.main.path, path("docs/report.txt"));
        assert_eq!(
            plan.main.action,
            Action::ApplyRemoteMove {
                entry_id: ENTRY,
                revision: revision(5),
                version: None,
                target: path("docs/report-final.txt"),
            },
            "T09：只改位置，不下载内容，EntryId 不变"
        );
        assert_eq!(
            plan.main.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(9)
                },
                Prerequisite::UsagePermitsChange,
                Prerequisite::TargetPathUsable,
            ]
        );
        assert!(plan.main.dependencies.is_empty());
        assert_eq!(
            plan.main.local_protection,
            LocalProtection::KeepLocalContent
        );
        assert!(plan.late.is_none());
    }

    #[test]
    fn remote_directory_move_keeps_entry_id_and_downloads_nothing() {
        let base = base_dir();
        let local = LocalFact::directory(path("docs"), generation(9));
        let remote = ServerFact::directory(path("docs-2026"), ENTRY, revision(5));
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: None,
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("目录移动应给出计划");
        };
        assert_eq!(
            plan.main.action,
            Action::ApplyRemoteMove {
                entry_id: ENTRY,
                revision: revision(5),
                version: None,
                target: path("docs-2026"),
            },
            "design §9.5：移动目录只改父目录关系，子项不重新传输"
        );
    }

    #[test]
    fn local_move_uploads_target_and_source_revision() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x31);
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &base,
            targets: vacant(),
            late: None,
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("仅本地移动应给出计划");
        };
        assert_eq!(
            plan.main.action,
            Action::UploadMove {
                entry_id: ENTRY,
                base_revision: revision(4),
                target: path("docs/archive/report.txt"),
            }
        );
        assert_eq!(plan.main.dependencies, [Dependency::ParentDirectoryReady]);
        assert_eq!(
            plan.main.local_protection,
            LocalProtection::KeepLocalContent
        );
    }

    #[test]
    fn local_move_with_content_change_snapshots_the_generation() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x33);
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &base,
            targets: vacant(),
            late: None,
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("移动+修改应给出计划");
        };
        assert_eq!(
            plan.main.action,
            Action::UploadMoveWithContent {
                entry_id: ENTRY,
                base_revision: revision(4),
                target: path("docs/archive/report.txt"),
                generation: generation(9),
            }
        );
        assert_eq!(
            plan.main.dependencies,
            [
                Dependency::ParentDirectoryReady,
                Dependency::SnapshotLocalContent {
                    generation: generation(9)
                },
            ]
        );
        assert_eq!(
            plan.main.local_protection,
            LocalProtection::UploadSnapshot {
                generation: generation(9)
            }
        );
    }

    #[test]
    fn both_moving_to_the_same_place_with_same_content_merges() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x31);
        let remote = ServerFact::file(
            path("docs/archive/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: None,
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("同位置同内容应幂等归并");
        };
        assert_eq!(
            plan.main.action,
            Action::MergeSameResult {
                revision: revision(5),
                version: Some(VERSION_OLD),
            }
        );
        assert_eq!(plan.main.path, path("docs/archive/report.txt"));
        assert!(plan.late.is_none(), "归并不造副本");
    }

    #[test]
    fn two_different_targets_keep_the_committed_path_and_copy_the_late_one() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x31);
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let late = late_branch();
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: Some(&late),
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("双移动到不同位置应保留双方");
        };
        assert_eq!(
            plan.main.action,
            Action::ApplyRemoteMove {
                entry_id: ENTRY,
                revision: revision(5),
                version: None,
                target: path("docs/2026/report.txt"),
            },
            "已提交位置为主"
        );
        assert_eq!(
            plan.main.dependencies,
            [Dependency::ConflictCopyCommitted {
                entry_id: COPY_ENTRY
            }],
            "迟到分支提交后才能把本地身份移到已提交位置"
        );
        let Some(ConflictCopy::Create(copy)) = plan.late else {
            panic!("迟到目标应成为稳定身份的冲突副本");
        };
        assert_eq!(
            copy.path,
            path("docs/archive/report.txt"),
            "意图位置与主条目不同：保留用户选定的名称"
        );
        assert_eq!(
            copy.action,
            Action::CreateConflictCopy {
                entry_id: COPY_ENTRY,
                op_id: OP,
                generation: generation(9),
            }
        );
    }

    #[test]
    fn move_plus_modify_keeps_both_candidates_conservatively() {
        let base = base_file();
        let local = local_file_at("docs/report.txt", 0x33);
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let late = late_branch();
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: Some(&late),
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("移动+修改应保留双方");
        };
        assert_eq!(
            plan.main.action,
            Action::ApplyRemoteMove {
                entry_id: ENTRY,
                revision: revision(5),
                version: Some(VERSION_OLD),
                target: path("docs/2026/report.txt"),
            },
            "已提交的位置与内容继续作为主条目"
        );
        assert_eq!(
            plan.main.dependencies,
            [
                Dependency::DownloadVersion {
                    version: VERSION_OLD
                },
                Dependency::ConflictCopyCommitted {
                    entry_id: COPY_ENTRY
                },
            ]
        );
        assert_eq!(
            plan.main.local_protection,
            LocalProtection::SnapshotBeforeReplace {
                generation: generation(9)
            }
        );
        let Some(ConflictCopy::Create(copy)) = plan.late else {
            panic!("迟到分支保留其内容与意图位置");
        };
        assert_eq!(copy.path, path("docs/report.txt"));
    }

    #[test]
    fn same_target_with_different_content_needs_a_conflict_name() {
        let base = base_file();
        let local = local_file_at("docs/2026/report.txt", 0x33);
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let late = late_branch();
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: Some(&late),
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("同位置不同内容应保留双方");
        };
        let Some(ConflictCopy::Create(copy)) = plan.late else {
            panic!("迟到分支必须另名");
        };
        assert_eq!(
            copy.path,
            path(&format!("docs/2026/{EXPECTED_NAME}")),
            "主条目保留已提交名称，迟到分支加冲突后缀"
        );
    }

    #[test]
    fn both_moved_and_modified_keeps_the_committed_side_and_the_late_copy() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x33);
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x35, 120),
            ENTRY,
            revision(5),
            VERSION_NEW,
        );
        let late = late_branch();
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: Some(&late),
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("两边都移动并修改时应保留双方");
        };
        assert_eq!(
            plan.main.action,
            Action::ApplyRemoteMove {
                entry_id: ENTRY,
                revision: revision(5),
                version: Some(VERSION_NEW),
                target: path("docs/2026/report.txt"),
            }
        );
        assert_eq!(
            plan.main.dependencies,
            [
                Dependency::DownloadVersion {
                    version: VERSION_NEW
                },
                Dependency::ConflictCopyCommitted {
                    entry_id: COPY_ENTRY
                },
            ]
        );
        let Some(ConflictCopy::Create(copy)) = plan.late else {
            panic!("本地修改后的内容必须保留");
        };
        assert_eq!(copy.path, path("docs/archive/report.txt"));
    }

    #[test]
    fn directory_cycle_is_replanned() {
        let base = base_dir();
        let local = LocalFact::directory(path("docs/2026"), generation(9));
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &base,
            targets: vacant(),
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect("可规划"),
            MoveOutcome::Replan(ReplanReason::DirectoryCycle)
        );

        let sibling = LocalFact::directory(path("docs-old"), generation(9));
        let request = MoveRequest {
            base: &base,
            local: &sibling,
            remote: &base,
            targets: vacant(),
            late: None,
        };
        assert!(
            matches!(
                plan_move(&request).expect("可规划"),
                MoveOutcome::Planned(_)
            ),
            "按分量判定：docs 与 docs-old 不是祖先关系"
        );
    }

    #[test]
    fn unusable_targets_are_replanned() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x31);
        let occupied = MoveTargets {
            local: TargetState::Vacant,
            server: TargetState::Occupied,
        };
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &base,
            targets: occupied,
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect("可规划"),
            MoveOutcome::Replan(ReplanReason::TargetOccupied)
        );

        let missing_ancestor = MoveTargets {
            local: TargetState::AncestorMissing,
            server: TargetState::Vacant,
        };
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let request = MoveRequest {
            base: &base,
            local: &local_file_at("docs/report.txt", 0x31),
            remote: &remote,
            targets: missing_ancestor,
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect("可规划"),
            MoveOutcome::Replan(ReplanReason::TargetAncestorMissing)
        );
    }

    #[test]
    fn replaced_identity_is_replanned() {
        let base = base_file();
        let local = local_file_at("docs/report.txt", 0x31);
        let remote = ServerFact::file(
            path("docs/report-final.txt"),
            content(0x31, 120),
            OTHER_ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect("可规划"),
            MoveOutcome::Replan(ReplanReason::IdentityReplaced)
        );
    }

    #[test]
    fn non_move_inputs_are_rejected() {
        let base = base_file();
        let unchanged = local_file_at("docs/report.txt", 0x31);
        let request = MoveRequest {
            base: &base,
            local: &unchanged,
            remote: &base,
            targets: vacant(),
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect_err("不是移动"),
            MoveError::NotAMove
        );

        let modified = local_file_at("docs/report.txt", 0x33);
        let request = MoveRequest {
            base: &base,
            local: &modified,
            remote: &base,
            targets: vacant(),
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect_err("纯内容变化不是移动"),
            MoveError::NotAMove
        );

        let local_directory = LocalFact::directory(path("docs/report.txt"), generation(9));
        let request = MoveRequest {
            base: &base,
            local: &local_directory,
            remote: &base,
            targets: vacant(),
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect_err("类型互换不归移动"),
            MoveError::NotAMove
        );
    }

    #[test]
    fn missing_late_branch_input_is_an_explicit_error() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x31);
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: None,
        };
        assert_eq!(
            plan_move(&request).expect_err("缺冲突身份不能默认成功"),
            MoveError::LateBranchMissing
        );
    }

    #[test]
    fn late_branch_retry_reuses_the_committed_identity() {
        let base = base_file();
        let local = local_file_at("docs/archive/report.txt", 0x33);
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let record = CommittedConflict {
            op_id: OP,
            entry_id: COPY_ENTRY,
            path: path("docs/archive/report.txt"),
            content: content(0x33, 120),
        };
        let late = LateBranch {
            entry_id: COPY_ENTRY,
            naming: naming(),
            committed: Some(&record),
        };
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: Some(&late),
        };
        let MoveOutcome::Planned(plan) = plan_move(&request).expect("可规划") else {
            panic!("仍应保留双方");
        };
        assert_eq!(plan.late, Some(ConflictCopy::Reuse(record.clone())));

        let changed = LateBranch {
            entry_id: COPY_ENTRY,
            naming: ConflictNameRequest {
                op_id: OTHER_OP,
                ..naming()
            },
            committed: Some(&record),
        };
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: Some(&changed),
        };
        assert_eq!(
            plan_move(&request).expect_err("同 OpId 不同请求必须拒绝"),
            MoveError::OpIdRequestChanged
        );
    }

    #[test]
    fn clock_skew_does_not_change_the_plan() {
        let base = base_file().with_observed_mtime(mtime(1_000));
        let local =
            local_file_at("docs/archive/report.txt", 0x31).with_observed_mtime(mtime(9_999));
        let remote = ServerFact::file(
            path("docs/2026/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        )
        .with_observed_mtime(mtime(50));
        let late = late_branch();
        let request = MoveRequest {
            base: &base,
            local: &local,
            remote: &remote,
            targets: vacant(),
            late: Some(&late),
        };
        let first = plan_move(&request).expect("可规划");
        let second = plan_move(&request).expect("可规划");
        assert_eq!(first, second, "同一输入得到同一计划");
    }

    fn source_file() -> ServerFact {
        ServerFact::file(
            path("docs/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(4),
            VERSION_OLD,
        )
    }

    #[test]
    fn cross_root_move_confirms_the_target_before_deleting_the_source() {
        let source = source_file();
        let target_path = path("imports/report.txt");
        let request = CrossRootMoveRequest {
            source_root: SOURCE_ROOT,
            source: &source,
            target_root: TARGET_ROOT,
            target_path: &target_path,
            target_entry_id: TARGET_ENTRY,
            generation: generation(9),
            source_subtree: None,
            target: TargetState::Vacant,
        };
        let CrossRootOutcome::Planned(plan) = plan_cross_root_move(&request).expect("可规划")
        else {
            panic!("跨 Root 移动应给出两段计划");
        };
        assert_eq!(
            plan.create_target.action,
            Action::UploadNewEntry {
                generation: generation(9)
            }
        );
        assert_eq!(plan.create_target.path, path("imports/report.txt"));
        assert!(
            plan.create_target
                .prerequisites
                .contains(&Prerequisite::ServerPathVacant),
            "创建条件是该路径在目标 Root 仍为空"
        );
        assert_eq!(
            plan.delete_source.action,
            Action::DeleteEntry {
                entry_id: ENTRY,
                base_revision: revision(4),
                subtree: None,
            }
        );
        assert_eq!(
            plan.delete_source.dependencies,
            [Dependency::CrossRootTargetCommitted {
                entry_id: TARGET_ENTRY
            }],
            "目标创建失败或结果未知时不得计划完成源删除"
        );
        assert_eq!(
            plan.delete_source.prerequisites,
            [Prerequisite::BaseRevisionCurrent {
                expected: revision(4)
            }]
        );
    }

    #[test]
    fn cross_root_target_failure_cannot_plan_a_source_delete() {
        let source = source_file();
        let target_path = path("imports/report.txt");
        let request = CrossRootMoveRequest {
            source_root: SOURCE_ROOT,
            source: &source,
            target_root: TARGET_ROOT,
            target_path: &target_path,
            target_entry_id: TARGET_ENTRY,
            generation: generation(9),
            source_subtree: None,
            target: TargetState::Occupied,
        };
        assert_eq!(
            plan_cross_root_move(&request).expect("可规划"),
            CrossRootOutcome::Replan(ReplanReason::TargetOccupied)
        );
    }

    #[test]
    fn cross_root_move_checks_root_and_subtree_token() {
        let source = source_file();
        let target_path = path("imports/report.txt");
        let same_root = CrossRootMoveRequest {
            source_root: SOURCE_ROOT,
            source: &source,
            target_root: SOURCE_ROOT,
            target_path: &target_path,
            target_entry_id: TARGET_ENTRY,
            generation: generation(9),
            source_subtree: None,
            target: TargetState::Vacant,
        };
        assert_eq!(
            plan_cross_root_move(&same_root).expect_err("同 Root 不是跨 Root 移动"),
            MoveError::SameRoot
        );

        let with_token = CrossRootMoveRequest {
            source_subtree: Some(SOURCE_TOKEN),
            target_root: TARGET_ROOT,
            ..same_root
        };
        assert_eq!(
            plan_cross_root_move(&with_token).expect_err("文件源不应带 token"),
            MoveError::UnexpectedSubtreeToken
        );

        let directory = ServerFact::directory(path("docs"), ENTRY, revision(4));
        let directory_move = CrossRootMoveRequest {
            source_root: SOURCE_ROOT,
            source: &directory,
            target_root: TARGET_ROOT,
            target_path: &target_path,
            target_entry_id: TARGET_ENTRY,
            generation: generation(9),
            source_subtree: None,
            target: TargetState::Vacant,
        };
        assert_eq!(
            plan_cross_root_move(&directory_move).expect_err("目录源必须带 token"),
            MoveError::MissingSubtreeToken
        );
    }
}
