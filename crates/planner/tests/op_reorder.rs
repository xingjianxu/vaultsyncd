//! 规划器操作重排验证（S023；`docs/design.md` §6 的 I02/I03/I04/I06/I07、§21.1 的 T02/T04/T09—T11）。
//!
//! 目的：证明同一批设备操作换个先后次序，规划器仍然①不丢唯一未确认的本地内容（I02）、②不在路径
//! 变化时改动条目身份（I03）、③不拿过期基线静默覆盖另一端的修改（I04）、④重试不造第二份副本
//! （I06）、⑤首次合并推不出删除（I07）。
//!
//! 做法（不引入模型检查框架）：`fixtures/planner/op-reorder.txt` 给出少量固定种子的操作多重集；
//! 本测试按种子枚举这些操作的重排次序，从“已同步”与“首次合并”两个初始世界出发，对每一步、
//! 每个槽位落到该条目的全部规划入口做不变量检查，并核对“相同的终态必须得到相同的规划”。
//!
//! 失败时把操作轨迹缩到最短再报告：最短轨迹里的操作按 fixture 的写法直接可读，把种子交给
//! `VAULTSYNCD_PLANNER_REPLAY` 就能只重放该场景：
//!
//! ```text
//! VAULTSYNCD_PLANNER_REPLAY=0x23003 cargo test --locked -p vaultsyncd-planner \
//!     --test op_reorder -- --nocapture seeded_operation_orderings_keep_unconfirmed_content
//! ```
//!
//! 槽位只有三个（两个文件、一个目录）：规划器是纯规则入口，不看同级条目也不做 I/O，一个槽位就是
//! “同一个条目在三方事实里的全部信息”，操作则是设备对它的编辑、移动、删除与重试。
//!
//! 检查器本身有负控（[`checkers_reject_unpreserved_replacement`]）：人工编一份“远端内容替换唯一
//! 本地内容却没有冲突副本”的计划，检查器必须报错，否则本文件的通过没有意义。

use std::collections::{HashMap, HashSet};
use std::fmt;

use vaultsyncd_domain::ids::{EntryId, OpId, VersionId};
use vaultsyncd_domain::version::{Generation, Revision, UtcTimestamp};
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_planner::conflict::{
    CommittedConflict, ConflictCopy, ConflictNameRequest, ConflictPlan, ContentConflictRequest,
    NameBudget, ShortIdLen, plan_content_conflict,
};
use vaultsyncd_planner::delete::{
    ConflictReason, DeleteOutcome, DeleteReplanReason, DeleteRequest, PreservePlan,
    PreservedContent, PreservedReason, ServerDeletion, SubtreeToken, plan_delete,
};
use vaultsyncd_planner::first_sync::{BindingMode, plan_first_sync};
use vaultsyncd_planner::reconcile::{
    Action, ContentFingerprint, DeferReason, Dependency, EntryKind, LocalFact, LocalProtection,
    Plan, Prerequisite, ServerFact, reconcile,
};
use vaultsyncd_planner::rename::{
    LateBranch, MoveOutcome, MovePlan, MoveRequest, MoveTargets, ReplanReason, TargetState,
    plan_move,
};

/// 重排样本（编译期嵌入：fixture 变化会重新编译本测试，路径也不依赖工作目录）。
const FIXTURE: &str = include_str!("../../../fixtures/planner/op-reorder.txt");

/// 每个场景最多跑多少个重排次序（4 个操作有 24 种，这里只取种子挑出的少量）。
const MAX_ORDERINGS: usize = 12;

/// 固定内容长度：只有摘要不同，长度一致，避免把“长度不同”当成额外变量。
const CONTENT_SIZE: u64 = 120;

/// 冲突名称里的设备短标识与服务器时间（固定值，不读时钟）。
const DEVICE_LABEL: &str = "3f7c9a01";
const SERVER_TIME_SECONDS: i64 = 1_793_867_400;

/// 目录删除的子树 token：规划器只比较相等，值本身没有语义。
const SUBTREE_TOKEN: SubtreeToken = SubtreeToken::from_bytes([0x7e; 16]);

/// 枚举必须真正走到的关键裁决：否则“重排后不变量都成立”可能只是因为没走到有意思的状态。
const REQUIRED_COVERAGE: &[&str] = &[
    "defer-content-conflict",
    "identity-replaced",
    "preserve-both",
    "conflict-copy-create",
    "conflict-copy-reuse",
    "delete-preserve",
    "apply-remote-delete",
    "already-deleted",
    "DirectoryPreserveRequired",
    "upload-new-entry",
    "create-from-remote",
    "apply-remote-move",
    "upload-move-with-content",
    "preservation-required",
];

// ---------------------------------------------------------------------------
// 世界模型：三个槽位、两个初始状态、一组设备操作
// ---------------------------------------------------------------------------

/// 被操作的条目槽位。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    /// 文件 `docs/report.txt`。
    Report,
    /// 文件 `docs/notes.txt`。
    Notes,
    /// 目录 `docs`。
    Docs,
}

impl Slot {
    const ALL: [Slot; 3] = [Slot::Report, Slot::Notes, Slot::Docs];

    fn index(self) -> usize {
        match self {
            Slot::Report => 0,
            Slot::Notes => 1,
            Slot::Docs => 2,
        }
    }

    fn is_directory(self) -> bool {
        matches!(self, Slot::Docs)
    }

    /// 该槽位可用的路径：文件三个候选位置；目录第三个候选落在自己子树里，用来触发目录环重规划。
    fn paths(self) -> &'static [&'static str] {
        match self {
            Slot::Report => &[
                "docs/report.txt",
                "docs/report-moved.txt",
                "docs/archive/report.txt",
            ],
            Slot::Notes => &[
                "docs/notes.txt",
                "docs/notes-moved.txt",
                "docs/archive/notes.txt",
            ],
            Slot::Docs => &["docs", "docs-archive", "docs/archive"],
        }
    }

    /// 已同步世界里的条目身份；移动不改变它（I03）。
    fn synced_identity(self) -> EntryId {
        EntryId::from_bytes([0x11 * (self.index() as u8 + 1); 16])
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Slot::Report => "report",
            Slot::Notes => "notes",
            Slot::Docs => "docs",
        })
    }
}

/// 设备操作：规划器只看这些操作留下的事实，不看它们是谁在什么时候做的。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Op {
    /// 本地保存新内容（本地代际 +1）。
    LocalEdit(Slot, u8),
    /// 另一设备已提交新内容（服务器修订与内容版本 +1）。
    RemoteEdit(Slot, u8),
    /// 本地把条目挪到另一个路径（同一身份）。
    LocalMove(Slot, u8),
    /// 已提交的移动（服务器修订 +1）。
    RemoteMove(Slot, u8),
    /// 本地删除。
    LocalDelete(Slot),
    /// 已提交的删除。
    RemoteDelete(Slot),
    /// 删除后在同一路径新建：新身份（I03）。
    Recreate(Slot, u8),
    /// 同一逻辑操作重试：事实不变，只增加一个检查点（I06）。
    Retry(Slot),
}

impl Op {
    /// fixture 里的规范写法，用于失败报告与手工重放。
    fn text(self) -> String {
        match self {
            Op::LocalEdit(slot, value) => format!("{slot}.local-edit.{value}"),
            Op::RemoteEdit(slot, value) => format!("{slot}.remote-edit.{value}"),
            Op::LocalMove(slot, value) => format!("{slot}.local-move.{value}"),
            Op::RemoteMove(slot, value) => format!("{slot}.remote-move.{value}"),
            Op::LocalDelete(slot) => format!("{slot}.local-delete"),
            Op::RemoteDelete(slot) => format!("{slot}.remote-delete"),
            Op::Recreate(slot, value) => format!("{slot}.recreate.{value}"),
            Op::Retry(slot) => format!("{slot}.retry"),
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

/// 初始世界：条目是否已经有共同基线，决定走三方比较还是首次合并。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitialState {
    /// 三个槽位都已同步（B=L=R）。
    Bound,
    /// 首次合并：没有共同基线，两侧各有一些条目。
    FirstSync,
}

impl InitialState {
    fn name(self) -> &'static str {
        match self {
            InitialState::Bound => "已同步",
            InitialState::FirstSync => "首次合并",
        }
    }
}

/// 一个槽位的三方事实：直接存规划器自己的事实类型，避免测试模型与计划输入出现两套拼写。
#[derive(Clone, Debug, PartialEq, Eq)]
struct SlotState {
    slot: Slot,
    base: Option<ServerFact>,
    local: Option<LocalFact>,
    remote: Option<ServerFact>,
    /// 远端已提交删除时所依据的修订（`remote` 为 `None` 时才有值）。
    deleted_at: Option<Revision>,
}

impl SlotState {
    /// 这个槽位当前的路径：优先本地，然后基线，再远端，最后回到槽位默认路径。
    fn preferred_path(&self) -> RelativePath {
        self.local
            .as_ref()
            .map(|local| local.path.clone())
            .or_else(|| self.base.as_ref().map(|base| base.path.clone()))
            .or_else(|| self.remote.as_ref().map(|remote| remote.path.clone()))
            .unwrap_or_else(|| path(self.slot, 0))
    }
}

/// 计数器与三个槽位组成的测试世界。
struct World {
    slots: [SlotState; 3],
    revisions: u64,
    generations: u64,
    versions: u64,
    identities: u8,
}

impl World {
    fn new(initial: InitialState) -> Self {
        let mut world = Self {
            slots: Slot::ALL.map(|slot| SlotState {
                slot,
                base: None,
                local: None,
                remote: None,
                deleted_at: None,
            }),
            revisions: 0,
            generations: 0,
            versions: 0,
            identities: 0x80,
        };
        for slot in Slot::ALL {
            match initial {
                InitialState::Bound => world.bind_synced(slot),
                InitialState::FirstSync => world.prime_first_sync(slot),
            }
        }
        world
    }

    fn slot(&self, slot: Slot) -> &SlotState {
        &self.slots[slot.index()]
    }

    fn slot_mut(&mut self, slot: Slot) -> &mut SlotState {
        &mut self.slots[slot.index()]
    }

    fn next_revision(&mut self) -> Revision {
        self.revisions += 1;
        Revision::new(self.revisions).expect("测试修订在范围内")
    }

    fn next_generation(&mut self) -> Generation {
        self.generations += 1;
        Generation::new(self.generations).expect("测试代际在范围内")
    }

    fn next_version(&mut self) -> VersionId {
        self.versions += 1;
        VersionId::from_bytes([self.versions as u8; 16])
    }

    fn next_identity(&mut self) -> EntryId {
        self.identities += 1;
        EntryId::from_bytes([self.identities; 16])
    }

    /// 已同步世界：三方事实完全一致。
    fn bind_synced(&mut self, slot: Slot) {
        let revision = self.next_revision();
        let version = self.next_version();
        let generation = self.next_generation();
        let entry_id = slot.synced_identity();
        let path = path(slot, 0);
        let (server, local) = if slot.is_directory() {
            (
                ServerFact::directory(path.clone(), entry_id, revision),
                LocalFact::directory(path, generation),
            )
        } else {
            let content = digest(0x10 + slot.index() as u8);
            (
                ServerFact::file(path.clone(), content, entry_id, revision, version),
                LocalFact::file(path, content, generation),
            )
        };
        let state = self.slot_mut(slot);
        state.base = Some(server.clone());
        state.remote = Some(server);
        state.local = Some(local);
    }

    /// 首次合并世界：一个两侧不同内容、一个只有本地、一个只有远端。
    fn prime_first_sync(&mut self, slot: Slot) {
        let path = path(slot, 0);
        let entry_id = slot.synced_identity();
        match slot {
            // 同路径两侧内容不同：保留双方，不推导删除（I07）。
            Slot::Report => {
                let revision = self.next_revision();
                let version = self.next_version();
                let generation = self.next_generation();
                let state = self.slot_mut(slot);
                state.remote = Some(ServerFact::file(
                    path.clone(),
                    digest(0x40),
                    entry_id,
                    revision,
                    version,
                ));
                state.local = Some(LocalFact::file(path, digest(0x41), generation));
            }
            // 只有本地：新建条目（I07）。
            Slot::Notes => {
                let generation = self.next_generation();
                self.slot_mut(slot).local = Some(LocalFact::file(path, digest(0x50), generation));
            }
            // 只有远端目录：在本地建立（I07）。
            Slot::Docs => {
                let revision = self.next_revision();
                self.slot_mut(slot).remote = Some(ServerFact::directory(path, entry_id, revision));
            }
        }
    }

    /// 应用一个设备操作。对已经不存在的条目做移动是空操作：它只增加一个检查点。
    fn apply(&mut self, op: Op) {
        match op {
            Op::LocalEdit(slot, content) => {
                let generation = self.next_generation();
                let state = self.slot_mut(slot);
                let path = state.preferred_path();
                state.local = Some(LocalFact::file(path, digest(content), generation));
            }
            Op::RemoteEdit(slot, content) => {
                let revision = self.next_revision();
                let version = self.next_version();
                let existing = self
                    .slot(slot)
                    .remote
                    .as_ref()
                    .map(|remote| remote.entry_id);
                let entry_id = match existing {
                    Some(entry_id) => entry_id,
                    None => self.next_identity(),
                };
                let state = self.slot_mut(slot);
                let path = state.preferred_path();
                state.remote = Some(ServerFact::file(
                    path,
                    digest(content),
                    entry_id,
                    revision,
                    version,
                ));
                state.deleted_at = None;
            }
            Op::LocalMove(slot, index) => {
                let target = path(slot, index);
                let state = self.slot_mut(slot);
                if let Some(local) = state.local.as_mut() {
                    local.path = target;
                }
            }
            Op::RemoteMove(slot, index) => {
                let target = path(slot, index);
                let unchanged = self
                    .slot(slot)
                    .remote
                    .as_ref()
                    .is_none_or(|remote| remote.path == target);
                if unchanged {
                    // 移到原路径不是一次提交：不推进服务器修订。
                    return;
                }
                let revision = self.next_revision();
                let state = self.slot_mut(slot);
                if let Some(remote) = state.remote.as_mut() {
                    remote.path = target;
                    remote.revision = revision;
                }
            }
            Op::LocalDelete(slot) => self.slot_mut(slot).local = None,
            Op::RemoteDelete(slot) => {
                let revision = self.next_revision();
                let state = self.slot_mut(slot);
                state.remote = None;
                state.deleted_at = Some(revision);
            }
            Op::Recreate(slot, content) => {
                let entry_id = self.next_identity();
                let revision = self.next_revision();
                let version = self.next_version();
                let state = self.slot_mut(slot);
                let path = state.preferred_path();
                state.remote = Some(ServerFact::file(
                    path,
                    digest(content),
                    entry_id,
                    revision,
                    version,
                ));
                state.deleted_at = None;
            }
            // 重试不改事实：同一请求再提交一次，只有幂等结果必须相同（I06）。
            Op::Retry(_) => {}
        }
    }

    /// 该槽位交给规划器的三方事实。
    fn facts(&self, slot: Slot) -> Facts {
        let state = self.slot(slot);
        Facts {
            slot,
            base: state.base.clone(),
            local: state.local.clone(),
            remote: state.remote.clone(),
            deleted_at: state.deleted_at,
        }
    }

    /// 世界里当前在用的条目身份：冲突副本不得复用它们（I03）。
    fn live_identities(&self) -> Vec<EntryId> {
        let mut identities = Vec::new();
        for state in &self.slots {
            if let Some(base) = &state.base {
                identities.push(base.entry_id);
            }
            if let Some(remote) = &state.remote {
                identities.push(remote.entry_id);
            }
        }
        identities
    }

    /// 终态指纹：用于核对“相同的终态必须得到相同的规划”。
    fn fingerprint(&self) -> String {
        self.slots
            .iter()
            .map(|state| format!("{state:?}"))
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

fn path(slot: Slot, index: u8) -> RelativePath {
    let names = slot.paths();
    let name = names[usize::from(index) % names.len()];
    RelativePath::parse(name).expect("固定的测试路径必须合法")
}

fn digest(value: u8) -> ContentFingerprint {
    ContentFingerprint::new(CONTENT_SIZE, [value; 32])
}

// ---------------------------------------------------------------------------
// 交给规划器的输入与归一化后的结果
// ---------------------------------------------------------------------------

/// 一个槽位交给规划器的三方事实。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Facts {
    slot: Slot,
    base: Option<ServerFact>,
    local: Option<LocalFact>,
    remote: Option<ServerFact>,
    deleted_at: Option<Revision>,
}

impl Facts {
    fn is_empty(&self) -> bool {
        self.base.is_none() && self.local.is_none() && self.remote.is_none()
    }

    fn base_identity(&self) -> Option<EntryId> {
        self.base.as_ref().map(|base| base.entry_id)
    }

    fn remote_identity(&self) -> Option<EntryId> {
        self.remote.as_ref().map(|remote| remote.entry_id)
    }

    /// 唯一未确认的本地文件内容：既不等于基线内容、也不等于服务器已提交内容（I02 的主语）。
    fn unique_local_content(&self) -> Option<(&LocalFact, ContentFingerprint)> {
        let local = self.local.as_ref()?;
        let EntryKind::File(content) = local.kind else {
            return None;
        };
        if self
            .base
            .as_ref()
            .is_some_and(|base| base.kind == local.kind)
        {
            return None;
        }
        if self
            .remote
            .as_ref()
            .is_some_and(|remote| remote.kind == local.kind)
        {
            return None;
        }
        Some((local, content))
    }
}

/// 本步给冲突副本分配的新身份与命名输入（固定值，跨重排与重放都相同）。
#[derive(Clone, Copy, Debug)]
struct Minted {
    conflict_entry_id: EntryId,
    op_id: OpId,
}

impl Minted {
    fn naming(self) -> ConflictNameRequest<'static> {
        ConflictNameRequest {
            device_label: DEVICE_LABEL,
            op_id: self.op_id,
            server_time: UtcTimestamp::from_unix_seconds(SERVER_TIME_SECONDS)
                .expect("固定的服务器时间在范围内"),
            budget: NameBudget::CROSS_PLATFORM,
            short_id_len: ShortIdLen::DEFAULT,
        }
    }
}

fn minted(slot: Slot) -> Minted {
    Minted {
        conflict_entry_id: EntryId::from_bytes([0xe0 + slot.index() as u8; 16]),
        op_id: OpId::from_bytes([0x5a + slot.index() as u8; 16]),
    }
}

/// 一次规划调用的完整输入（含“同一 OpId 已提交”的重试输入）。
#[derive(Clone, Copy, Debug)]
struct Probe<'a> {
    minted: Minted,
    committed: Option<&'a CommittedConflict>,
}

/// 归一化后的规划结果：不变量只看这些字段，不看是哪个入口给出的。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Decision {
    verdict: Verdict,
    facts: Facts,
    actions: Vec<Action>,
    copies: Vec<ConflictCopy>,
    prerequisites: Vec<Prerequisite>,
    dependencies: Vec<Dependency>,
    protection: LocalProtection,
}

/// 结果形状。`Plan` 表示给出了可执行动作（可能是 `Action::Defer` 的延期裁决）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Plan,
    Preserve(&'static str),
    Replan(&'static str),
    ConflictRequired(&'static str),
    AlreadyDeleted,
}

impl Decision {
    fn plan(facts: Facts, plan: &Plan) -> Self {
        Self {
            verdict: Verdict::Plan,
            facts,
            actions: vec![plan.action.clone()],
            copies: Vec::new(),
            prerequisites: plan.prerequisites.clone(),
            dependencies: plan.dependencies.clone(),
            protection: plan.local_protection,
        }
    }

    fn move_plan(facts: Facts, plan: &MovePlan) -> Self {
        Self {
            verdict: Verdict::Plan,
            facts,
            actions: vec![plan.main.action.clone()],
            copies: plan.late.iter().cloned().collect(),
            prerequisites: plan.main.prerequisites.clone(),
            dependencies: plan.main.dependencies.clone(),
            protection: plan.main.local_protection,
        }
    }

    fn preserve_plan(facts: Facts, plan: &PreservePlan) -> Self {
        Self {
            verdict: Verdict::Preserve(preserved_reason_name(plan.reason)),
            facts,
            actions: vec![plan.main.action.clone()],
            copies: vec![plan.preserve.clone()],
            prerequisites: plan.main.prerequisites.clone(),
            dependencies: plan.main.dependencies.clone(),
            protection: plan.main.local_protection,
        }
    }

    fn conflict_plan(facts: Facts, plan: &ConflictPlan) -> Self {
        Self {
            verdict: Verdict::Plan,
            facts,
            actions: vec![plan.main.action.clone()],
            copies: vec![plan.copy.clone()],
            prerequisites: plan.main.prerequisites.clone(),
            dependencies: plan.main.dependencies.clone(),
            protection: plan.main.local_protection,
        }
    }

    /// 不生成动作的裁决（重规划、需要冲突处理、幂等成功）：本地内容不动。
    fn no_actions(facts: Facts, verdict: Verdict) -> Self {
        Self {
            verdict,
            facts,
            actions: Vec::new(),
            copies: Vec::new(),
            prerequisites: Vec::new(),
            dependencies: Vec::new(),
            protection: LocalProtection::KeepLocalContent,
        }
    }

    fn describe(&self) -> String {
        let actions: Vec<&str> = self.actions.iter().map(action_label).collect();
        format!(
            "{:?} actions={actions:?} copies={} 依赖={:?} 保护={:?}",
            self.verdict,
            self.copies.len(),
            self.dependencies,
            self.protection
        )
    }

    /// 这一步会不会拿替换/删除动作碰唯一未确认的本地内容（I02 的强分支前提）。
    fn requires_preservation_destination(&self) -> bool {
        let Some((_, content)) = self.facts.unique_local_content() else {
            return false;
        };
        self.actions
            .iter()
            .any(|action| replaces_local_content(action, &self.facts, content))
    }

    /// 结果里的冲突副本若是首次创建，换算成“服务端已记录、重试必须沿用”的记录（I06）。
    fn committed_record(&self) -> Option<CommittedConflict> {
        for copy in &self.copies {
            let ConflictCopy::Create(plan) = copy else {
                continue;
            };
            let Action::CreateConflictCopy {
                entry_id, op_id, ..
            } = plan.action
            else {
                continue;
            };
            let EntryKind::File(content) = self.facts.local.as_ref()?.kind else {
                continue;
            };
            return Some(CommittedConflict {
                op_id,
                entry_id,
                path: plan.path.clone(),
                content,
            });
        }
        None
    }
}

fn preserved_reason_name(reason: PreservedReason) -> &'static str {
    match reason {
        PreservedReason::Modified => "modified",
        PreservedReason::Moved => "moved",
    }
}

fn move_replan_name(reason: ReplanReason) -> &'static str {
    match reason {
        ReplanReason::DirectoryCycle => "DirectoryCycle",
        ReplanReason::TargetOccupied => "TargetOccupied",
        ReplanReason::TargetAncestorMissing => "TargetAncestorMissing",
        ReplanReason::IdentityReplaced => "IdentityReplaced",
        ReplanReason::DirectoryConflictRequired => "DirectoryConflictRequired",
    }
}

fn delete_replan_name(reason: DeleteReplanReason) -> &'static str {
    match reason {
        DeleteReplanReason::SubtreeChanged => "SubtreeChanged",
        DeleteReplanReason::BaseRevisionMismatch => "BaseRevisionMismatch",
        DeleteReplanReason::LocalChanged => "LocalChanged",
        DeleteReplanReason::IdentityReplaced => "IdentityReplaced",
        DeleteReplanReason::DirectoryPreserveRequired => "DirectoryPreserveRequired",
    }
}

fn conflict_reason_name(reason: ConflictReason) -> &'static str {
    match reason {
        ConflictReason::ContentModified => "ContentModified",
        ConflictReason::Moved => "Moved",
    }
}

fn action_label(action: &Action) -> &'static str {
    match action {
        Action::NoOp => "NoOp",
        Action::ApplyRemote { .. } => "ApplyRemote",
        Action::UploadLocal { .. } => "UploadLocal",
        Action::MergeSameResult { .. } => "MergeSameResult",
        Action::CreateConflictCopy { .. } => "CreateConflictCopy",
        Action::ApplyRemoteMove { .. } => "ApplyRemoteMove",
        Action::UploadMove { .. } => "UploadMove",
        Action::UploadMoveWithContent { .. } => "UploadMoveWithContent",
        Action::ApplyRemoteDelete { .. } => "ApplyRemoteDelete",
        Action::DeleteEntry { .. } => "DeleteEntry",
        Action::UploadNewEntry { .. } => "UploadNewEntry",
        Action::CreateFromRemote { .. } => "CreateFromRemote",
        Action::BindRemoteEntry { .. } => "BindRemoteEntry",
        Action::PreserveBoth(_) => "PreserveBoth",
        Action::Defer(_) => "Defer",
    }
}

// ---------------------------------------------------------------------------
// 规划入口：按事实形状选择入口，并把结果归一化
// ---------------------------------------------------------------------------

/// 该事实形状下全部适用的规划入口（三方比较/首次合并/移动/删除，外加内容冲突入口）。
fn collect_decisions(
    facts: &Facts,
    minted: Minted,
    committed: Option<&CommittedConflict>,
) -> Result<Vec<Decision>, Violation> {
    let probe = Probe { minted, committed };
    let mut decisions = dispatch(facts, &probe)?;
    if let Some(conflict) = content_conflict(facts, &probe)? {
        decisions.push(conflict);
    }
    Ok(decisions)
}

/// 按“有没有基线、哪一侧缺失、有没有移动”选择入口（入口分工见 `planner::lib` 的 crate 文档）。
fn dispatch(facts: &Facts, probe: &Probe<'_>) -> Result<Vec<Decision>, Violation> {
    if facts.is_empty() {
        return Ok(Vec::new());
    }
    let Some(base) = facts.base.as_ref() else {
        return first_sync_decisions(facts);
    };
    if facts.local.is_none() || facts.remote.is_none() {
        return Ok(vec![delete_decision(facts, base, probe)?]);
    }
    let local = facts.local.as_ref().expect("上一分支保证本地存在");
    let remote = facts.remote.as_ref().expect("上一分支保证远端存在");
    if remote.entry_id != base.entry_id {
        // 删除后原路径新建：按新身份重新规划，不拿新身份的内容替换旧身份的本地内容（I03）。
        let plan = reconcile(Some(base), Some(local), Some(remote))
            .map_err(|error| Violation::new("planner-error", format!("三方比较失败：{error}")))?;
        return Ok(vec![Decision::plan(facts.clone(), &plan)]);
    }
    if local.path != base.path || remote.path != base.path {
        return Ok(vec![move_decision(facts, base, local, remote, probe)?]);
    }
    let plan = reconcile(Some(base), Some(local), Some(remote))
        .map_err(|error| Violation::new("planner-error", format!("三方比较失败：{error}")))?;
    Ok(vec![Decision::plan(facts.clone(), &plan)])
}

/// 首次合并：没有基线，推不出删除（I07）。两侧路径不同说明它们是两个路径，各自规划一次。
fn first_sync_decisions(facts: &Facts) -> Result<Vec<Decision>, Violation> {
    fn failed(error: impl fmt::Display) -> Violation {
        Violation::new("planner-error", format!("首次合并规划失败：{error}"))
    }
    if let (Some(local), Some(remote)) = (&facts.local, &facts.remote)
        && local.path != remote.path
    {
        let local_only = plan_first_sync(Some(local), None, BindingMode::Full).map_err(failed)?;
        let remote_only = plan_first_sync(None, Some(remote), BindingMode::Full).map_err(failed)?;
        return Ok(vec![
            Decision::plan(local_only_facts(facts), &local_only),
            Decision::plan(remote_only_facts(facts), &remote_only),
        ]);
    }
    let plan = plan_first_sync(
        facts.local.as_ref(),
        facts.remote.as_ref(),
        BindingMode::Full,
    )
    .map_err(failed)?;
    Ok(vec![Decision::plan(facts.clone(), &plan)])
}

fn local_only_facts(facts: &Facts) -> Facts {
    Facts {
        remote: None,
        deleted_at: None,
        ..facts.clone()
    }
}

fn remote_only_facts(facts: &Facts) -> Facts {
    Facts {
        local: None,
        deleted_at: None,
        ..facts.clone()
    }
}

fn delete_decision(
    facts: &Facts,
    base: &ServerFact,
    probe: &Probe<'_>,
) -> Result<Decision, Violation> {
    let server = match facts.remote.as_ref() {
        Some(remote) => ServerDeletion::Present(remote),
        None => ServerDeletion::Deleted {
            at_revision: facts.deleted_at.unwrap_or(base.revision),
        },
    };
    let preserve = PreservedContent {
        entry_id: probe.minted.conflict_entry_id,
        naming: probe.minted.naming(),
        committed: probe.committed,
    };
    let request = DeleteRequest {
        base,
        server,
        local: facts.local.as_ref(),
        subtree: facts.slot.is_directory().then_some(SUBTREE_TOKEN),
        subtree_changed: false,
        preserve: Some(&preserve),
    };
    let outcome = plan_delete(&request)
        .map_err(|error| Violation::new("planner-error", format!("删除规划失败：{error}")))?;
    Ok(match outcome {
        DeleteOutcome::Upload(plan) => Decision::plan(facts.clone(), &plan),
        DeleteOutcome::ApplyLocally(plan) => Decision::plan(facts.clone(), &plan),
        DeleteOutcome::Preserve(plan) => Decision::preserve_plan(facts.clone(), &plan),
        DeleteOutcome::AlreadyDeleted => {
            Decision::no_actions(facts.clone(), Verdict::AlreadyDeleted)
        }
        DeleteOutcome::ConflictRequired(reason) => Decision::no_actions(
            facts.clone(),
            Verdict::ConflictRequired(conflict_reason_name(reason)),
        ),
        DeleteOutcome::Replan(reason) => {
            Decision::no_actions(facts.clone(), Verdict::Replan(delete_replan_name(reason)))
        }
    })
}

fn move_decision(
    facts: &Facts,
    base: &ServerFact,
    local: &LocalFact,
    remote: &ServerFact,
    probe: &Probe<'_>,
) -> Result<Decision, Violation> {
    let late = LateBranch {
        entry_id: probe.minted.conflict_entry_id,
        naming: probe.minted.naming(),
        committed: probe.committed,
    };
    let request = MoveRequest {
        base,
        local,
        remote,
        // 现场观察：每个槽位各有独立目标路径，目标位置为空、祖先目录已存在。
        targets: MoveTargets {
            local: TargetState::Vacant,
            server: TargetState::Vacant,
        },
        late: Some(&late),
    };
    let outcome = plan_move(&request)
        .map_err(|error| Violation::new("planner-error", format!("移动规划失败：{error}")))?;
    Ok(match outcome {
        MoveOutcome::Planned(plan) => Decision::move_plan(facts.clone(), &plan),
        MoveOutcome::Replan(reason) => {
            Decision::no_actions(facts.clone(), Verdict::Replan(move_replan_name(reason)))
        }
    })
}

/// 内容冲突入口：两侧都改内容且字节不同时才适用（design §10.1、§10.2）。
fn content_conflict(facts: &Facts, probe: &Probe<'_>) -> Result<Option<Decision>, Violation> {
    let (Some(base), Some(local), Some(remote)) = (&facts.base, &facts.local, &facts.remote) else {
        return Ok(None);
    };
    let (
        EntryKind::File(base_content),
        EntryKind::File(local_content),
        EntryKind::File(remote_content),
    ) = (base.kind, local.kind, remote.kind)
    else {
        return Ok(None);
    };
    if base.path != local.path || local.path != remote.path || base.entry_id != remote.entry_id {
        return Ok(None);
    }
    if base_content == local_content
        || base_content == remote_content
        || local_content == remote_content
    {
        return Ok(None);
    }
    let request = ContentConflictRequest {
        base,
        local,
        remote,
        conflict_entry_id: probe.minted.conflict_entry_id,
        committed: probe.committed,
        naming: probe.minted.naming(),
    };
    let plan = plan_content_conflict(&request)
        .map_err(|error| Violation::new("planner-error", format!("内容冲突规划失败：{error}")))?;
    Ok(Some(Decision::conflict_plan(facts.clone(), &plan)))
}

// ---------------------------------------------------------------------------
// 不变量检查
// ---------------------------------------------------------------------------

/// 违反某条不变量。
#[derive(Debug)]
struct Violation {
    invariant: &'static str,
    detail: String,
    location: String,
}

impl Violation {
    fn new(invariant: &'static str, detail: impl Into<String>) -> Self {
        Self {
            invariant,
            detail: detail.into(),
            location: String::new(),
        }
    }

    fn at_slot(mut self, slot: Slot) -> Self {
        self.location = format!("{slot} 槽位");
        self
    }

    fn at_step(mut self, step: usize, op: Op) -> Self {
        let prefix = format!("第 {step} 步 {op} 之后 · ");
        self.location = format!("{prefix}{}", self.location);
        self
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}：{}", self.invariant, self.location, self.detail)
    }
}

fn check_invariants(
    decisions: &[Decision],
    minted: Minted,
    live: &[EntryId],
) -> Result<(), Violation> {
    for decision in decisions {
        check_i02(decision)?;
        check_i03(decision, minted, live)?;
        check_i04(decision)?;
        check_i07(decision)?;
    }
    Ok(())
}

/// I02：唯一未确认的本地内容总有保留去处。
///
/// 任何会替换或删除本地内容的动作，都必须先有一份保存该本地代际的冲突副本，且主计划声明
/// `ConflictCopyCommitted` 依赖——只固定快照不够，快照本身不是“去处”。
fn check_i02(decision: &Decision) -> Result<(), Violation> {
    let Some((local, content)) = decision.facts.unique_local_content() else {
        return Ok(());
    };
    for action in &decision.actions {
        if !replaces_local_content(action, &decision.facts, content) {
            continue;
        }
        let destination = decision
            .copies
            .iter()
            .find(|copy| copy_preserves(copy, local, content));
        let Some(copy) = destination else {
            return Err(Violation::new(
                "I02",
                format!(
                    "唯一未确认的本地内容（摘要首字节 {:#04x}）会被 {} 替换或删除，却没有任何冲突副本目的地：{}",
                    content.sha256()[0],
                    action_label(action),
                    decision.describe()
                ),
            ));
        };
        let entry_id = copy_entry_id(copy);
        if !decision.dependencies.iter().any(
            |dependency| matches!(dependency, Dependency::ConflictCopyCommitted { entry_id: id } if *id == entry_id),
        ) {
            return Err(Violation::new(
                "I02",
                format!(
                    "冲突副本 {entry_id:?} 未声明 ConflictCopyCommitted 依赖，替换可能先于副本提交：{}",
                    decision.describe()
                ),
            ));
        }
    }
    Ok(())
}

/// 该动作会不会让本地丢掉当前内容。`ApplyRemote`/`ApplyRemoteMove` 只有在远端内容与本地不同时
/// 才会真正替换字节；只改位置、只推进元数据都不算。
fn replaces_local_content(
    action: &Action,
    facts: &Facts,
    local_content: ContentFingerprint,
) -> bool {
    let remote_differs = facts
        .remote
        .as_ref()
        .is_some_and(|remote| remote.kind != EntryKind::File(local_content));
    match action {
        Action::ApplyRemote { .. } => remote_differs,
        Action::ApplyRemoteMove { version, .. } => version.is_some() && remote_differs,
        Action::ApplyRemoteDelete { .. } | Action::DeleteEntry { .. } => true,
        _ => false,
    }
}

/// 这份副本保存的是不是那份唯一的本地内容（按代际或按摘要核对）。
fn copy_preserves(copy: &ConflictCopy, local: &LocalFact, content: ContentFingerprint) -> bool {
    match copy {
        ConflictCopy::Create(plan) => matches!(
            plan.action,
            Action::CreateConflictCopy { generation, .. } if generation == local.generation
        ),
        ConflictCopy::Reuse(record) => record.content == content,
    }
}

fn copy_entry_id(copy: &ConflictCopy) -> EntryId {
    match copy {
        ConflictCopy::Create(plan) => match plan.action {
            Action::CreateConflictCopy { entry_id, .. } => entry_id,
            _ => panic!("冲突副本计划必须是 CreateConflictCopy 动作"),
        },
        ConflictCopy::Reuse(record) => record.entry_id,
    }
}

/// I03：身份稳定。路径变化不改 `EntryId`；删除后原路径新建是新身份，新身份不得接管旧身份的内容。
fn check_i03(decision: &Decision, minted: Minted, live: &[EntryId]) -> Result<(), Violation> {
    let writes = decision
        .actions
        .iter()
        .any(|action| !matches!(action, Action::Defer(_) | Action::NoOp))
        || !decision.copies.is_empty();
    if let (Some(base), Some(remote)) = (&decision.facts.base, &decision.facts.remote)
        && remote.entry_id != base.entry_id
        && writes
    {
        return Err(Violation::new(
            "I03",
            format!(
                "远端身份 {:?} 已不是基线身份 {:?}，却生成了动作：{}",
                remote.entry_id,
                base.entry_id,
                decision.describe()
            ),
        ));
    }
    let allowed = [
        decision.facts.base_identity(),
        decision.facts.remote_identity(),
        Some(minted.conflict_entry_id),
    ];
    for action in &decision.actions {
        if let Some(entry_id) = action_entry_id(action)
            && !allowed.contains(&Some(entry_id))
        {
            return Err(Violation::new(
                "I03",
                format!(
                    "{} 携带了不属于该条目的身份 {entry_id:?}：{}",
                    action_label(action),
                    decision.describe()
                ),
            ));
        }
    }
    for copy in &decision.copies {
        let entry_id = copy_entry_id(copy);
        if entry_id != minted.conflict_entry_id {
            return Err(Violation::new(
                "I03",
                format!("冲突副本必须使用本步新分配的身份，实际用了 {entry_id:?}"),
            ));
        }
        if live.contains(&entry_id) {
            return Err(Violation::new(
                "I03",
                format!("冲突副本身份 {entry_id:?} 与在用条目身份重合"),
            ));
        }
    }
    Ok(())
}

fn action_entry_id(action: &Action) -> Option<EntryId> {
    match action {
        Action::CreateConflictCopy { entry_id, .. }
        | Action::ApplyRemoteMove { entry_id, .. }
        | Action::UploadMove { entry_id, .. }
        | Action::UploadMoveWithContent { entry_id, .. }
        | Action::ApplyRemoteDelete { entry_id, .. }
        | Action::DeleteEntry { entry_id, .. }
        | Action::CreateFromRemote { entry_id, .. }
        | Action::BindRemoteEntry { entry_id, .. } => Some(*entry_id),
        _ => None,
    }
}

/// I04：冲突不静默覆盖。两个方向：
///
/// 1. 提交必须带**基线事实的**修订，并声明 `BaseRevisionCurrent` 复核：远端在这之后前进时，执行器
///    要先重新规划，而不是把旧基线写上去（design §9.1、§11.2 第 4 步的 CAS）；
/// 2. 双方都改且结果不同时，任何改动本地内容或位置的动作都必须先有保留副本。
fn check_i04(decision: &Decision) -> Result<(), Violation> {
    for action in &decision.actions {
        let Some(base_revision) = submit_base_revision(action) else {
            continue;
        };
        let Some(base) = decision.facts.base.as_ref() else {
            return Err(Violation::new(
                "I04",
                format!(
                    "{} 声称携带基线修订 {base_revision:?}，但输入没有基线",
                    action_label(action)
                ),
            ));
        };
        if base_revision != base.revision {
            return Err(Violation::new(
                "I04",
                format!(
                    "{} 携带的基线修订 {base_revision:?} 不是基线事实的修订 {:?}：{}",
                    action_label(action),
                    base.revision,
                    decision.describe()
                ),
            ));
        }
        let guarded = decision.prerequisites.iter().any(|prerequisite| {
            matches!(prerequisite, Prerequisite::BaseRevisionCurrent { expected } if *expected == base_revision)
        });
        if !guarded {
            return Err(Violation::new(
                "I04",
                format!(
                    "{} 没有 BaseRevisionCurrent 复核，远端在这之后前进就会用过期基线覆盖另一端的修改：{}",
                    action_label(action),
                    decision.describe()
                ),
            ));
        }
        if decision.facts.remote.is_none() {
            return Err(Violation::new(
                "I04",
                format!(
                    "远端已无该条目，{} 仍带着基线修订 {base_revision:?} 继续：{}",
                    action_label(action),
                    decision.describe()
                ),
            ));
        }
    }

    if both_sides_changed(&decision.facts) && !same_result(&decision.facts) {
        let unilateral = decision.actions.iter().any(|action| {
            matches!(
                action,
                Action::UploadLocal { .. }
                    | Action::UploadMove { .. }
                    | Action::UploadMoveWithContent { .. }
                    | Action::ApplyRemote { .. }
                    | Action::ApplyRemoteMove { .. }
                    | Action::ApplyRemoteDelete { .. }
                    | Action::DeleteEntry { .. }
            )
        });
        if unilateral && decision.copies.is_empty() {
            return Err(Violation::new(
                "I04",
                format!(
                    "双方都改了且结果不同，却在没有保留副本的情况下改动本地：{}",
                    decision.describe()
                ),
            ));
        }
    }
    Ok(())
}

fn submit_base_revision(action: &Action) -> Option<Revision> {
    match action {
        Action::UploadLocal { base_revision, .. }
        | Action::UploadMove { base_revision, .. }
        | Action::UploadMoveWithContent { base_revision, .. }
        | Action::DeleteEntry { base_revision, .. } => Some(*base_revision),
        _ => None,
    }
}

fn local_changed(facts: &Facts) -> bool {
    match (&facts.base, &facts.local) {
        (Some(base), Some(local)) => local.path != base.path || local.kind != base.kind,
        (None, Some(_)) => true,
        _ => false,
    }
}

fn remote_changed(facts: &Facts) -> bool {
    match (&facts.base, &facts.remote) {
        (Some(base), Some(remote)) => {
            remote.path != base.path || remote.kind != base.kind || remote.revision != base.revision
        }
        (Some(_), None) => true,
        _ => false,
    }
}

fn both_sides_changed(facts: &Facts) -> bool {
    local_changed(facts) && remote_changed(facts)
}

fn same_result(facts: &Facts) -> bool {
    match (&facts.local, &facts.remote) {
        (Some(local), Some(remote)) => local.path == remote.path && local.kind == remote.kind,
        _ => false,
    }
}

/// I07：首次合并不推导删除，也不覆盖本地内容。
fn check_i07(decision: &Decision) -> Result<(), Violation> {
    if decision.facts.base.is_some() {
        return Ok(());
    }
    for action in &decision.actions {
        let forbidden = matches!(
            action,
            Action::DeleteEntry { .. }
                | Action::ApplyRemoteDelete { .. }
                | Action::ApplyRemote { .. }
                | Action::ApplyRemoteMove { .. }
                | Action::UploadLocal { .. }
                | Action::UploadMove { .. }
                | Action::UploadMoveWithContent { .. }
        );
        if forbidden {
            return Err(Violation::new(
                "I07",
                format!(
                    "首次合并没有基线，却计划了 {}：{}",
                    action_label(action),
                    decision.describe()
                ),
            ));
        }
    }
    if decision.facts.local.is_some() && decision.protection == LocalProtection::NotNeeded {
        return Err(Violation::new(
            "I07",
            format!(
                "首次合并里本地存在条目，但计划把本地内容保护标成 NotNeeded：{}",
                decision.describe()
            ),
        ));
    }
    Ok(())
}

/// I06：同一 `OpId` 已有提交结果时，重试必须复用同一身份与路径，主条目计划不变。
fn check_retry(facts: &Facts, decisions: &[Decision], minted: Minted) -> Result<(), Violation> {
    let Some(record) = decisions.iter().find_map(Decision::committed_record) else {
        return Ok(());
    };
    let retried = collect_decisions(facts, minted, Some(&record))?;
    if retried.len() != decisions.len() {
        return Err(Violation::new("I06", "同一 OpId 重试后规划结果的数量变了"));
    }
    for (original, again) in decisions.iter().zip(&retried) {
        if original.committed_record().is_none() {
            continue;
        }
        if again.copies != [ConflictCopy::Reuse(record.clone())] {
            return Err(Violation::new(
                "I06",
                format!(
                    "同一 OpId 重试没有复用已提交的副本 {record:?}，实际是 {:?}",
                    again.copies
                ),
            ));
        }
        if again.actions != original.actions || again.dependencies != original.dependencies {
            return Err(Violation::new("I06", "同一 OpId 重试改变了主条目计划"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 运行、枚举与收缩
// ---------------------------------------------------------------------------

/// 枚举里真正走到过的关键裁决。
#[derive(Default)]
struct Coverage {
    seen: HashSet<&'static str>,
}

impl Coverage {
    fn record(&mut self, name: &'static str) {
        self.seen.insert(name);
    }

    fn missing(&self) -> Vec<&'static str> {
        REQUIRED_COVERAGE
            .iter()
            .filter(|name| !self.seen.contains(*name))
            .copied()
            .collect()
    }
}

fn record_coverage(coverage: &mut Coverage, decisions: &[Decision]) {
    for decision in decisions {
        match decision.verdict {
            Verdict::Plan => {}
            Verdict::Preserve(_) => coverage.record("delete-preserve"),
            Verdict::Replan(name) => coverage.record(name),
            Verdict::ConflictRequired(name) => coverage.record(name),
            Verdict::AlreadyDeleted => coverage.record("already-deleted"),
        }
        for action in &decision.actions {
            match action {
                Action::Defer(DeferReason::ContentConflict) => {
                    coverage.record("defer-content-conflict");
                }
                Action::Defer(DeferReason::IdentityReplaced) => {
                    coverage.record("identity-replaced");
                }
                Action::PreserveBoth(_) => coverage.record("preserve-both"),
                Action::ApplyRemoteDelete { .. } => coverage.record("apply-remote-delete"),
                Action::ApplyRemoteMove { .. } => coverage.record("apply-remote-move"),
                Action::UploadMoveWithContent { .. } => {
                    coverage.record("upload-move-with-content");
                }
                Action::UploadNewEntry { .. } => coverage.record("upload-new-entry"),
                Action::CreateFromRemote { .. } => coverage.record("create-from-remote"),
                _ => {}
            }
        }
        for copy in &decision.copies {
            match copy {
                ConflictCopy::Create(_) => coverage.record("conflict-copy-create"),
                ConflictCopy::Reuse(_) => coverage.record("conflict-copy-reuse"),
            }
        }
    }
}

/// 跑一条操作轨迹：每一步之后检查每个槽位，返回终态世界。
fn run_sequence(
    initial: InitialState,
    ops: &[Op],
    coverage: &mut Coverage,
) -> Result<World, Violation> {
    let mut world = World::new(initial);
    check_world(&world, coverage)?;
    for (step, op) in ops.iter().enumerate() {
        world.apply(*op);
        check_world(&world, coverage).map_err(|violation| violation.at_step(step, *op))?;
    }
    Ok(world)
}

/// 检查一个世界里的全部槽位。
fn check_world(world: &World, coverage: &mut Coverage) -> Result<(), Violation> {
    let live = world.live_identities();
    for slot in Slot::ALL {
        check_slot(world, slot, &live, coverage).map_err(|violation| violation.at_slot(slot))?;
    }
    Ok(())
}

fn check_slot(
    world: &World,
    slot: Slot,
    live: &[EntryId],
    coverage: &mut Coverage,
) -> Result<(), Violation> {
    let facts = world.facts(slot);
    let minted = minted(slot);
    let decisions = collect_decisions(&facts, minted, None)?;
    // I06：同一事实重复规划必须得到同一结果（重试不改计划）。
    if decisions != collect_decisions(&facts, minted, None)? {
        return Err(Violation::new(
            "I06",
            "同一事实两次规划得到不同结果（计划不是纯函数）",
        ));
    }
    check_invariants(&decisions, minted, live)?;
    check_retry(&facts, &decisions, minted)?;
    if decisions
        .iter()
        .any(|decision| decision.committed_record().is_some())
    {
        // 这一步生成了冲突副本，重试核对走的是“服务端已记录、复用同一身份”的路径（I06）。
        coverage.record("conflict-copy-reuse");
    }
    if decisions
        .iter()
        .any(|decision| decision.requires_preservation_destination())
    {
        // I02 的强分支真的进过：唯一未确认内容遇上了会替换或删除它的动作。
        coverage.record("preservation-required");
    }
    record_coverage(coverage, &decisions);
    Ok(())
}

/// 把失败轨迹缩到最短：逐个删除操作，只要不变量仍然失效就保留删除。
fn shrink(initial: InitialState, ops: &[Op]) -> Vec<Op> {
    let mut current = ops.to_vec();
    let mut coverage = Coverage::default();
    let mut shrunk = true;
    while shrunk {
        shrunk = false;
        for index in 0..current.len() {
            let mut candidate = current.clone();
            candidate.remove(index);
            if run_sequence(initial, &candidate, &mut coverage).is_err() {
                current = candidate;
                shrunk = true;
                break;
            }
        }
    }
    current
}

/// SplitMix64：够用的固定种子发生器，不引入依赖。
struct SeededRng {
    state: u64,
}

impl SeededRng {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// 按种子挑出少量互不相同的重排次序；fixture 里写的次序总是第一个跑。
fn distinct_orderings(ops: &[Op], seed: u64, cap: usize) -> Vec<Vec<Op>> {
    let mut all = Vec::new();
    let mut current = Vec::new();
    let mut used = vec![false; ops.len()];
    collect_permutations(ops, &mut current, &mut used, &mut all);
    all.sort();
    all.dedup();

    let written = ops.to_vec();
    let mut rest: Vec<Vec<Op>> = all.into_iter().filter(|order| *order != written).collect();
    let mut rng = SeededRng::new(seed);
    for index in (1..rest.len()).rev() {
        let other = rng.below(index + 1);
        rest.swap(index, other);
    }
    let mut orderings = vec![written];
    orderings.extend(rest.into_iter().take(cap.saturating_sub(1)));
    orderings
}

fn collect_permutations(
    ops: &[Op],
    current: &mut Vec<Op>,
    used: &mut [bool],
    out: &mut Vec<Vec<Op>>,
) {
    if current.len() == ops.len() {
        out.push(current.clone());
        return;
    }
    for (index, op) in ops.iter().enumerate() {
        if used[index] {
            continue;
        }
        // 相同操作只按第一个未用的实例展开，避免同一排列被枚举多次。
        if index > 0 && *op == ops[index - 1] && used[index - 1] {
            continue;
        }
        used[index] = true;
        current.push(*op);
        collect_permutations(ops, current, used, out);
        current.pop();
        used[index] = false;
    }
}

// ---------------------------------------------------------------------------
// fixture 解析
// ---------------------------------------------------------------------------

/// fixture 里的一行场景。
struct Row {
    seed: u64,
    name: String,
    ops: Vec<Op>,
}

fn parse_corpus(text: &str) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    let mut seeds = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let mut fields = line.split_whitespace();
        let seed_text = fields
            .next()
            .ok_or_else(|| format!("第 {number} 行缺少种子"))?;
        let seed = parse_seed(seed_text).map_err(|error| format!("第 {number} 行种子：{error}"))?;
        let name = fields
            .next()
            .ok_or_else(|| format!("第 {number} 行缺少场景名"))?
            .to_string();
        let ops = fields
            .map(|token| {
                parse_op(token).map_err(|error| format!("第 {number} 行 {token}：{error}"))
            })
            .collect::<Result<Vec<Op>, String>>()?;
        if ops.is_empty() {
            return Err(format!("第 {number} 行没有任何操作"));
        }
        if !seeds.insert(seed) {
            return Err(format!("第 {number} 行的种子 0x{seed:x} 与前面的场景重复"));
        }
        rows.push(Row { seed, name, ops });
    }
    if rows.is_empty() {
        return Err("fixture 里没有任何场景".to_string());
    }
    Ok(rows)
}

fn parse_seed(text: &str) -> Result<u64, String> {
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).map_err(|error| error.to_string()),
        None => text.parse::<u64>().map_err(|error| error.to_string()),
    }
}

fn parse_op(token: &str) -> Result<Op, String> {
    let mut parts = token.split('.');
    let slot = match parts.next() {
        Some("report") => Slot::Report,
        Some("notes") => Slot::Notes,
        Some("docs") => Slot::Docs,
        other => return Err(format!("未知槽位 {other:?}")),
    };
    let action = parts.next().ok_or("缺少动作名")?;
    let argument = match parts.next() {
        Some(text) => Some(text.parse::<u8>().map_err(|error| error.to_string())?),
        None => None,
    };
    if parts.next().is_some() {
        return Err("操作最多只有三段".to_string());
    }
    let op = match (action, argument) {
        ("local-edit", Some(value)) => Op::LocalEdit(slot, content_argument(slot, value)?),
        ("remote-edit", Some(value)) => Op::RemoteEdit(slot, content_argument(slot, value)?),
        ("recreate", Some(value)) => Op::Recreate(slot, content_argument(slot, value)?),
        ("local-move", Some(value)) => Op::LocalMove(slot, path_argument(slot, value)?),
        ("remote-move", Some(value)) => Op::RemoteMove(slot, path_argument(slot, value)?),
        ("local-delete", None) => Op::LocalDelete(slot),
        ("remote-delete", None) => Op::RemoteDelete(slot),
        ("retry", None) => Op::Retry(slot),
        (name, argument) => {
            return Err(format!("未知操作 {name:?}（参数 {argument:?}）"));
        }
    };
    Ok(op)
}

fn content_argument(slot: Slot, value: u8) -> Result<u8, String> {
    if slot.is_directory() {
        return Err("目录槽位没有文件内容".to_string());
    }
    if value > 3 {
        return Err("内容摘要只取 0—3".to_string());
    }
    Ok(value)
}

fn path_argument(slot: Slot, value: u8) -> Result<u8, String> {
    if usize::from(value) >= slot.paths().len() {
        return Err(format!("{slot} 槽位只有 {} 个候选路径", slot.paths().len()));
    }
    Ok(value)
}

fn ops_text(ops: &[Op]) -> String {
    ops.iter()
        .map(|op| op.text())
        .collect::<Vec<String>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 固定种子枚举少量编辑/移动/删改/重试次序，每一步都核对面向上面的不变量（I02/I03/I04/I06/I07）。
#[test]
fn seeded_operation_orderings_keep_unconfirmed_content() {
    let corpus = parse_corpus(FIXTURE).expect("fixture 必须可解析");
    let replay = std::env::var("VAULTSYNCD_PLANNER_REPLAY")
        .ok()
        .map(|text| parse_seed(&text).expect("重放种子必须是十进制或 0x 十六进制"));
    let mut coverage = Coverage::default();
    let mut orderings_run = 0usize;
    let mut steps_checked = 0usize;

    for row in &corpus {
        if let Some(seed) = replay
            && seed != row.seed
        {
            continue;
        }
        if replay.is_some() {
            println!(
                "重放场景 {}（seed=0x{:x}）：{}",
                row.name,
                row.seed,
                ops_text(&row.ops)
            );
        }
        for initial in [InitialState::Bound, InitialState::FirstSync] {
            let salt = match initial {
                InitialState::Bound => 0,
                InitialState::FirstSync => 0x9e37_79b9,
            };
            // 终态 → 规划结果：重排后终态相同，规划就必须相同（计划是事实的纯函数）。
            let mut terminal: HashMap<String, Vec<Decision>> = HashMap::new();
            for ordering in distinct_orderings(&row.ops, row.seed ^ salt, MAX_ORDERINGS) {
                orderings_run += 1;
                steps_checked += ordering.len();
                let world = match run_sequence(initial, &ordering, &mut coverage) {
                    Ok(world) => world,
                    Err(violation) => {
                        let minimal = shrink(initial, &ordering);
                        panic!(
                            "规划器重排验证失败：seed=0x{:x} 场景={} 初始状态={}\n最短轨迹：{}\n{}\n\
                             单独重放：VAULTSYNCD_PLANNER_REPLAY=0x{:x} cargo test --locked \
                             -p vaultsyncd-planner --test op_reorder -- --nocapture \
                             seeded_operation_orderings_keep_unconfirmed_content",
                            row.seed,
                            row.name,
                            initial.name(),
                            ops_text(&minimal),
                            violation,
                            row.seed,
                        );
                    }
                };
                let mut decisions = Vec::new();
                for slot in Slot::ALL {
                    let facts = world.facts(slot);
                    let slot_minted = minted(slot);
                    match collect_decisions(&facts, slot_minted, None) {
                        Ok(found) => decisions.extend(found),
                        Err(violation) => panic!(
                            "终态无法规划：seed=0x{:x} 场景={} 初始状态={} 轨迹={} {violation}",
                            row.seed,
                            row.name,
                            initial.name(),
                            ops_text(&ordering)
                        ),
                    }
                }
                let key = world.fingerprint();
                match terminal.get(&key) {
                    Some(previous) if *previous != decisions => panic!(
                        "终态相同但规划不同：seed=0x{:x} 场景={} 初始状态={} 轨迹={}",
                        row.seed,
                        row.name,
                        initial.name(),
                        ops_text(&ordering)
                    ),
                    Some(_) => {}
                    None => {
                        terminal.insert(key, decisions);
                    }
                }
            }
        }
    }

    let missing = coverage.missing();
    assert!(
        missing.is_empty(),
        "这些关键裁决没有被枚举真正走到：{missing:?}"
    );
    assert!(
        orderings_run > 0,
        "没有跑任何重排次序（种子过滤是否把所有场景都跳过了？）"
    );
    println!(
        "重排验证通过：{} 个场景、{orderings_run} 个次序、{steps_checked} 个操作检查点、{} 种关键裁决",
        corpus.len(),
        coverage.seen.len()
    );
}

/// 同一份 fixture 与种子必须给出同样的重排次序（失败可以按种子单独重放）。
#[test]
fn seed_selects_the_same_orderings_every_time() {
    let corpus = parse_corpus(FIXTURE).expect("fixture 必须可解析");
    for row in &corpus {
        let first = distinct_orderings(&row.ops, row.seed, MAX_ORDERINGS);
        let second = distinct_orderings(&row.ops, row.seed, MAX_ORDERINGS);
        assert_eq!(first, second, "seed=0x{:x} 的重排次序不稳定", row.seed);
        assert_eq!(first[0], row.ops, "fixture 写的次序必须第一个跑");
        assert!(first.len() <= MAX_ORDERINGS);
        for ordering in &first {
            let mut sorted = ordering.clone();
            sorted.sort();
            let mut expected = row.ops.clone();
            expected.sort();
            assert_eq!(sorted, expected, "重排不得增删操作");
        }
    }
}

/// fixture 的词表是封闭的：写错的操作名、越界参数、目录上的内容操作都必须报错，不能静默跳过。
#[test]
fn fixture_rejects_unknown_tokens() {
    assert!(parse_op("report.local-edit.2").is_ok());
    assert!(parse_op("docs.local-move.2").is_ok());
    assert!(parse_op("docs.retry").is_ok());
    assert!(parse_op("report.local-edit.9").is_err(), "摘要越界");
    assert!(parse_op("docs.local-edit.1").is_err(), "目录没有内容");
    assert!(parse_op("docs.local-move.9").is_err(), "路径越界");
    assert!(parse_op("report.frobnicate").is_err(), "未知操作");
    assert!(parse_op("server.local-edit.1").is_err(), "未知槽位");
    assert!(
        parse_corpus("0x1 a report.local-edit.1\n0x1 b docs.retry\n").is_err(),
        "重复种子必须报错"
    );
    assert!(parse_corpus("# 只有注释\n").is_err(), "空 fixture 必须报错");
}

/// I03：删除后原路径新建是新身份，规划器必须认出来并停下，而不是让新身份接管旧内容。
#[test]
fn identity_replacement_never_writes() {
    let mut world = World::new(InitialState::Bound);
    world.apply(Op::LocalEdit(Slot::Report, 2));
    world.apply(Op::RemoteDelete(Slot::Report));
    world.apply(Op::Recreate(Slot::Report, 3));

    let facts = world.facts(Slot::Report);
    let decisions = collect_decisions(&facts, minted(Slot::Report), None).expect("可规划");
    assert_eq!(decisions.len(), 1, "重建身份的状态只走三方比较入口");
    assert_eq!(
        decisions[0].actions,
        [Action::Defer(DeferReason::IdentityReplaced)]
    );
    assert!(decisions[0].copies.is_empty());
    // 本地内容仍是唯一未确认内容：它没有服务器副本，也没有被替换。
    assert!(facts.unique_local_content().is_some());
}

/// I06：同一 `OpId` 已提交过的冲突副本，重试返回同一身份与路径，不造第二份。
#[test]
fn committed_conflict_copy_is_reused_on_retry() {
    let mut world = World::new(InitialState::Bound);
    world.apply(Op::LocalEdit(Slot::Report, 2));
    world.apply(Op::RemoteEdit(Slot::Report, 3));

    let facts = world.facts(Slot::Report);
    let minted = minted(Slot::Report);
    let first = content_conflict(
        &facts,
        &Probe {
            minted,
            committed: None,
        },
    )
    .expect("可规划")
    .expect("双方都改内容必须给出内容冲突计划");
    let record = first.committed_record().expect("首次规划必须创建副本");

    let retried = content_conflict(
        &facts,
        &Probe {
            minted,
            committed: Some(&record),
        },
    )
    .expect("可规划")
    .expect("内容冲突形态不变");
    assert_eq!(retried.copies, [ConflictCopy::Reuse(record.clone())]);
    assert_eq!(retried.actions, first.actions);
    assert_eq!(retried.dependencies, first.dependencies);
}

/// I07：首次合并推不出删除，也不会把本地内容标成不需要保护。
#[test]
fn first_merge_never_derives_a_delete() {
    let world = World::new(InitialState::FirstSync);
    let live = world.live_identities();
    let mut coverage = Coverage::default();
    for slot in Slot::ALL {
        let facts = world.facts(slot);
        let minted = minted(slot);
        let decisions = collect_decisions(&facts, minted, None).expect("可规划");
        assert!(!decisions.is_empty(), "{slot} 有事实就必须有规划");
        check_invariants(&decisions, minted, &live).expect("首次合并必须保住本地内容");
        for decision in &decisions {
            for action in &decision.actions {
                assert!(
                    !matches!(
                        action,
                        Action::DeleteEntry { .. } | Action::ApplyRemoteDelete { .. }
                    ),
                    "首次合并不能生成删除：{action:?}"
                );
            }
        }
        record_coverage(&mut coverage, &decisions);
    }
    // 三种首次合并形态都该出现：保留双方、只有本地、只有远端。
    for name in ["preserve-both", "upload-new-entry", "create-from-remote"] {
        assert!(coverage.seen.contains(name), "缺少首次合并形态 {name}");
    }
}

/// 检查器负控：编一份“远端内容替换唯一本地内容却没有冲突副本”的计划，检查器必须拒绝。
/// 没有这条，主测试的通过可能只是检查器什么都没检查。
#[test]
fn checkers_reject_unpreserved_replacement() {
    let mut world = World::new(InitialState::Bound);
    world.apply(Op::LocalEdit(Slot::Report, 2));
    world.apply(Op::RemoteEdit(Slot::Report, 3));
    let facts = world.facts(Slot::Report);
    let local = facts.local.as_ref().expect("本地存在");
    let remote = facts.remote.as_ref().expect("远端存在");
    let base = facts.base.as_ref().expect("基线存在");

    let unprotected = Decision::plan(
        facts.clone(),
        &Plan {
            path: remote.path.clone(),
            action: Action::ApplyRemote {
                revision: remote.revision,
                version: remote.version,
            },
            prerequisites: Vec::new(),
            dependencies: Vec::new(),
            local_protection: LocalProtection::SnapshotBeforeReplace {
                generation: local.generation,
            },
        },
    );
    assert!(
        matches!(check_i02(&unprotected), Err(ref violation) if violation.invariant == "I02"),
        "没有冲突副本的替换必须被 I02 拒绝"
    );
    assert!(
        matches!(check_i04(&unprotected), Err(ref violation) if violation.invariant == "I04"),
        "双方都改且没有副本的替换必须被 I04 拒绝"
    );

    let stale = Decision::plan(
        facts.clone(),
        &Plan {
            path: local.path.clone(),
            action: Action::UploadLocal {
                base_revision: base.revision,
                generation: local.generation,
            },
            prerequisites: Vec::new(),
            dependencies: Vec::new(),
            local_protection: LocalProtection::UploadSnapshot {
                generation: local.generation,
            },
        },
    );
    assert!(
        matches!(check_i04(&stale), Err(ref violation) if violation.invariant == "I04"),
        "远端修订已前进时仍带旧基线的提交必须被 I04 拒绝"
    );
}
