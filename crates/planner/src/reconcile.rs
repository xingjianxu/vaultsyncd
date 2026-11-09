//! 三方比较与基本计划（`docs/design.md` §9.1、I02、I04）。
//!
//! 规划器把共同基线 B、本地状态 L 与最新远端状态 R 的事实转换成一份**计划**。比较对象是内容、
//! 名称/父目录、类型与“该侧是否存在”，**不是** `mtime`：时间戳只随事实一起记录，用于展示与检测
//! 候选变化（I04），任何分支都不用它决定胜者。
//!
//! 本模块实现 design §9.1 的四条基本分支：
//!
//! | 输入 | 计划 |
//! | --- | --- |
//! | L=B、R=B | [`Action::NoOp`] |
//! | L=B、R≠B | [`Action::ApplyRemote`]（先核对使用保护与路径条件） |
//! | L≠B、R=B | [`Action::UploadLocal`]（提交附带 B 的 `Revision`） |
//! | L≠B、R≠B 且已证明同一结果 | [`Action::MergeSameResult`] |
//!
//! “同一结果”指位置、类型与内容（长度 + SHA-256）都相同；仅 `mtime` 相同不算。其余输入不在本卡
//! 猜测：缺少基线（design §9.2）、某一侧缺失（design §10.3）、位置或类型变化（design §9.5、§10.1）
//! 与已被替换的远端身份（I03）都输出 [`Action::Defer`]，把决定权留给对应规则，同时说明本地内容
//! 必须原样保留（I02、I04）。其中没有共同基线的输入输出 [`DeferReason::FirstSync`]：它只表示“转交
//! 首次合并入口”，真正的计划由 [`crate::first_sync::plan_first_sync`] 给出（design §9.2）；已经同步
//! 过、只是游标过期的设备不得走那条入口，它必须带着旧基线回到本模块重建。
//!
//! 两侧“是否有变化”的判定方式不同，因为可用证据不同：
//!
//! - “R=B”按服务器分配的 [`Revision`] 判定：服务器只在条目发生变化时递增该值，因此修订相同且
//!   形状相同才算无变化；
//! - “L=B”只能按本地观察到的内容、位置与类型判定，本地没有服务器修订。
//!
//! 任一证据显示变化即按“有变化”处理：宁可重新规划一次，也不把过期基线当成无操作。

use vaultsyncd_domain::ids::{EntryId, OpId, VersionId};
use vaultsyncd_domain::version::{Generation, Revision, UtcTimestamp};
use vaultsyncd_fs_common::path::RelativePath;

use crate::delete::SubtreeToken;
use crate::first_sync::{BindingMode, PreserveReason};

use core::fmt;
use core::mem::discriminant;

/// 内容等价判定值：长度与 SHA-256 摘要（design §7.1 的 `ContentHash`、§10.1“校验哈希与大小”）。
///
/// 规划器不读字节也不计算哈希：摘要由调用方（传输层或平台层）算出后以值传入。相等要求长度与
/// 摘要**同时**相同；名称、`mtime` 与条目身份都不参与内容相等的判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentFingerprint {
    size: u64,
    sha256: [u8; 32],
}

impl ContentFingerprint {
    /// 由内容长度（字节）与 SHA-256 摘要构造。
    pub const fn new(size: u64, sha256: [u8; 32]) -> Self {
        Self { size, sha256 }
    }

    /// 内容长度（字节）。
    pub const fn size(self) -> u64 {
        self.size
    }

    /// SHA-256 摘要原文。
    pub const fn sha256(self) -> [u8; 32] {
        self.sha256
    }
}

/// 条目类型与内容（design §9.1 要比较的“类型”与“内容”）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    /// 文件：内容由长度与 SHA-256 判定。
    File(ContentFingerprint),
    /// 目录：没有文件内容版本（design I12）。
    Directory,
}

/// 服务器侧的条目事实：最新远端状态 R 或共同基线 B。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerFact {
    /// Vault 内相对位置（父目录与名称）。
    pub path: RelativePath,
    /// 类型与内容。
    pub kind: EntryKind,
    /// 服务器命名空间条目身份；路径改变不改变它（I03）。
    pub entry_id: EntryId,
    /// 服务器为该条目分配的修订。
    pub revision: Revision,
    /// 文件当前内容的不可变版本。目录没有内容版本，必须是 `None`；文件由
    /// [`ServerFact::file`] 保证为 `Some`。
    pub version: Option<VersionId>,
    /// 观察到的修改时间：只用于展示与检测候选变化，不参与任何判定（I04）。
    pub observed_mtime: Option<UtcTimestamp>,
}

impl ServerFact {
    /// 文件的服务器事实：内容版本是必需参数，避免“文件内容变化却没有版本”的事实进入计划。
    pub fn file(
        path: RelativePath,
        content: ContentFingerprint,
        entry_id: EntryId,
        revision: Revision,
        version: VersionId,
    ) -> Self {
        Self {
            path,
            kind: EntryKind::File(content),
            entry_id,
            revision,
            version: Some(version),
            observed_mtime: None,
        }
    }

    /// 目录的服务器事实（目录没有文件内容版本）。
    pub fn directory(path: RelativePath, entry_id: EntryId, revision: Revision) -> Self {
        Self {
            path,
            kind: EntryKind::Directory,
            entry_id,
            revision,
            version: None,
            observed_mtime: None,
        }
    }

    /// 记录观察到的修改时间；该值不参与比较（I04）。
    pub fn with_observed_mtime(mut self, mtime: UtcTimestamp) -> Self {
        self.observed_mtime = Some(mtime);
        self
    }

    /// 三方比较用的可比形状。
    fn shape(&self) -> Shape<'_> {
        Shape::new(&self.path, &self.kind)
    }
}

/// 本地侧的条目事实 L：本地只有观察到的内容与代际，没有服务器修订。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalFact {
    /// Vault 内相对位置（父目录与名称）。
    pub path: RelativePath,
    /// 类型与内容。
    pub kind: EntryKind,
    /// 本地内容代际（design §8.3 的 `local_generation`）。
    pub generation: Generation,
    /// 观察到的修改时间：只用于展示与检测候选变化，不参与任何判定（I04）。
    pub observed_mtime: Option<UtcTimestamp>,
}

impl LocalFact {
    /// 文件的本地事实。
    pub fn file(path: RelativePath, content: ContentFingerprint, generation: Generation) -> Self {
        Self {
            path,
            kind: EntryKind::File(content),
            generation,
            observed_mtime: None,
        }
    }

    /// 目录的本地事实。
    pub fn directory(path: RelativePath, generation: Generation) -> Self {
        Self {
            path,
            kind: EntryKind::Directory,
            generation,
            observed_mtime: None,
        }
    }

    /// 记录观察到的修改时间；该值不参与比较（I04）。
    pub fn with_observed_mtime(mut self, mtime: UtcTimestamp) -> Self {
        self.observed_mtime = Some(mtime);
        self
    }

    /// 三方比较用的可比形状。
    fn shape(&self) -> Shape<'_> {
        Shape::new(&self.path, &self.kind)
    }
}

/// 两个事实的可比形状：位置与类型/内容。所有“是否同一结果”的判断只看它，不看 `mtime`、身份、
/// 修订或代际。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape<'a> {
    path: &'a RelativePath,
    kind: &'a EntryKind,
}

impl<'a> Shape<'a> {
    const fn new(path: &'a RelativePath, kind: &'a EntryKind) -> Self {
        Self { path, kind }
    }

    /// 位置、类型与内容都相同（“已证明同一结果”，design §9.1 第三条）。
    fn same_result(self, other: Self) -> bool {
        self.path == other.path && self.kind == other.kind
    }

    /// 位置相同且类型（文件/目录）相同，但内容可以不同。
    fn same_location_and_kind(self, other: Self) -> bool {
        self.path == other.path && discriminant(self.kind) == discriminant(other.kind)
    }
}

/// 执行计划前必须重新核对的现场条件（顺序固定，按声明顺序出现在 [`Plan::prerequisites`] 中）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prerequisite {
    /// 服务器当前修订仍等于计划所依据的基线（design §11.2 第 4 步、§9.2 第 8 步的 CAS）。
    BaseRevisionCurrent {
        /// 计划依据的基线修订。
        expected: Revision,
    },
    /// 本地内容代际仍等于观察值；读取或核对期间再次变化就重新排队（design §9.1 末段）。
    GenerationUnchanged {
        /// 计划依据的本地代际。
        expected: Generation,
    },
    /// 使用保护允许替换本地内容：没有写入句柄、也未处于无法判断的状态（design §9.1、§14.4）。
    UsagePermitsChange,
    /// 目标位置可用：父目录已同步存在、名称可表示、且没有同名的另一种类型条目（design §9.1
    /// “路径条件”、§7.4）。
    TargetPathUsable,
    /// 服务器在该路径仍没有条目：首次合并的创建 CAS，避免用旧快照覆盖快照之后在服务器新建的条目
    /// （design §9.2 第 8 步）。
    ServerPathVacant,
    /// 本地在该路径仍没有条目：避免远端占位或下载覆盖扫描之后新建的本地内容（I02）。
    LocalPathVacant,
}

/// 必须先完成的任务或操作（顺序固定，按声明顺序出现在 [`Plan::dependencies`] 中）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dependency {
    /// 目标文件版本必须已完整下载并通过校验后才能安装（design I05）。
    DownloadVersion {
        /// 要下载的不可变内容版本。
        version: VersionId,
    },
    /// 父目录必须先在该侧就绪，否则不能发布子条目。
    ParentDirectoryReady,
    /// 必须先按该代际固定并校验上传快照，再提交（design §8.3 的 `dirty` → 上传）。
    SnapshotLocalContent {
        /// 快照固定的本地代际。
        generation: Generation,
    },
    /// 冲突副本必须先提交，才能替换主路径上的本地内容（design §10.1、I02）。
    ConflictCopyCommitted {
        /// 要等待提交的冲突条目身份。
        entry_id: EntryId,
    },
    /// 跨 Vault 移动的目标条目必须先提交成功，才允许删除源条目（design §9.5）：
    /// 目标创建失败或结果未知时，源条目的删除不得计划完成。
    CrossVaultTargetCommitted {
        /// 目标 Vault 新建条目的身份。
        entry_id: EntryId,
    },
}

/// 本地未确认内容的保护说明（design I02）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalProtection {
    /// 计划不涉及本地内容（无操作）。
    NotNeeded,
    /// 本地字节保持原样：只允许在远端 head 已写入基线后推进状态（归并、延期裁决都如此）。
    KeepLocalContent,
    /// 替换本地内容前先固定当前代际的快照；替换或校验失败时保留旧内容（design §8.4）。
    SnapshotBeforeReplace {
        /// 替换前要固定的本地代际。
        generation: Generation,
    },
    /// 上传固定该代际的快照；上传期间的新写入属于后续代际，旧 ACK 不得清除它（I02）。
    UploadSnapshot {
        /// 上传固定并提交的本地代际。
        generation: Generation,
    },
}

/// 本卡不裁决、交给后续规则处理的输入原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferReason {
    /// 没有共同基线：进入首次合并规则（design §9.2），不推导删除（I07）。
    FirstSync,
    /// 本地或远端没有该条目：删除与恢复规则（[`crate::delete`]、design §10.3），不推导删除。
    MissingEntry,
    /// 远端的条目身份已不是基线身份：原条目被删除后在原路径新建属于新身份（I03）。
    IdentityReplaced,
    /// 位置或类型相对基线变化：移动规则（[`crate::rename`]、design §9.5）与文件—目录互换规则
    /// （design §10.1）。
    LocationOrKindChanged,
    /// 双方都变且无法证明同一结果：冲突规则（design §10）。
    ContentConflict,
    /// 输入自称内容变化却给不出文件版本等必需事实：不生成缺少前提的计划。
    IncompleteFacts,
}

/// 计划动作。移动/删除动作携带目标路径与子树 token，因此动作按值移动而不是 `Copy`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// 双方无变：不生成任何操作（design §9.1）。
    NoOp,
    /// 应用远端变化：先核对前提，再按依赖安装远端版本或仅推进元数据（design §9.1）。
    ApplyRemote {
        /// 要应用的远端修订。
        revision: Revision,
        /// 远端文件版本；目录变化没有内容版本。
        version: Option<VersionId>,
    },
    /// 生成本地提交：附带基线修订（design §9.1），由执行器固定该代际的快照后上传。
    UploadLocal {
        /// 提交时携带的基线修订（CAS）。
        base_revision: Revision,
        /// 要上传的本地代际。
        generation: Generation,
    },
    /// 双方已证明同一结果：只推进基线，不再传输内容、也不再造副本（design §9.1、§10.1）。
    MergeSameResult {
        /// 归并采用的远端修订。
        revision: Revision,
        /// 归并采用的远端文件版本；目录归并没有内容版本。
        version: Option<VersionId>,
    },
    /// 迟到分支的不同内容成为新冲突条目，不覆盖服务器 head（design §10.1、§10.2）。
    CreateConflictCopy {
        /// 冲突条目的稳定身份：同一 `OpId` 只分配一次（I06、T03）。
        entry_id: EntryId,
        /// 触发该副本的逻辑操作身份；它是幂等依据，不是文件名。
        op_id: OpId,
        /// 作为副本内容的本地代际。
        generation: Generation,
    },
    /// 应用远端已提交的移动：本地把该身份放到 `target`，`EntryId` 不变（design §9.5、T09）。
    ///
    /// `version` 为 `Some` 时同时安装该内容版本（移动+修改）；`None` 时只改位置、不下载内容。
    ApplyRemoteMove {
        /// 条目身份：路径改变不影响它（I03）。
        entry_id: EntryId,
        /// 已提交的远端修订。
        revision: Revision,
        /// 远端文件版本；仅位置变化时为 `None`（T09），内容也变了才需要下载。
        version: Option<VersionId>,
        /// 已提交的目标位置。
        target: RelativePath,
    },
    /// 提交本地移动：目标父目录/名称与源修订，`EntryId` 与内容版本都不变（design §9.5、§15.2）。
    UploadMove {
        /// 条目身份：同 Vault 内移动保持它（I03）。
        entry_id: EntryId,
        /// 提交时携带的源修订（CAS）。
        base_revision: Revision,
        /// 本地意图的新位置。
        target: RelativePath,
    },
    /// 提交本地移动并上传该代际的新内容（design §10.1“一端移动、另一端修改”的本地提交方向）。
    UploadMoveWithContent {
        /// 条目身份：同 Vault 内移动保持它（I03）。
        entry_id: EntryId,
        /// 提交时携带的源修订（CAS）。
        base_revision: Revision,
        /// 本地意图的新位置。
        target: RelativePath,
        /// 要上传的本地内容代际。
        generation: Generation,
    },
    /// 应用远端已提交的删除：本地撤掉已删除的身份，不删除服务端之后新建的内容（design §10.3）。
    ApplyRemoteDelete {
        /// 被删除的条目身份。
        entry_id: EntryId,
        /// 已提交删除所依据的修订。
        revision: Revision,
    },
    /// 提交本地删除：携带目标修订，目录另带服务端生成的子树 token（design §10.3）。
    DeleteEntry {
        /// 要删除的条目身份。
        entry_id: EntryId,
        /// 删除所依据的基线修订（CAS）。
        base_revision: Revision,
        /// 目录删除的子树 token；文件删除为 `None`。
        subtree: Option<SubtreeToken>,
    },
    /// 首次合并时本地独有：在服务器新建条目（design §9.2 第 5 步）。没有基线修订可携带，创建条件是
    /// 该路径在服务器仍为空。
    UploadNewEntry {
        /// 要上传的本地代际。
        generation: Generation,
    },
    /// 首次合并时远端独有：在本地建立该条目（design §9.2 第 5 步）。
    CreateFromRemote {
        /// 要绑定的远端条目身份。
        entry_id: EntryId,
        /// 快照观察到的远端修订。
        revision: Revision,
        /// 远端文件版本；占位与目录可以没有内容版本。
        version: Option<VersionId>,
        /// 决定建占位还是下载完整内容的 Binding 模式（[`BindingMode`]，定义在
        /// [`crate::first_sync`]）。
        mode: BindingMode,
    },
    /// 首次合并时同路径、同类型且内容相同：绑定远端身份并写入基线，不传输内容（design §9.2 第 4 步）。
    BindRemoteEntry {
        /// 要绑定的远端条目身份。
        entry_id: EntryId,
        /// 快照观察到的远端修订。
        revision: Revision,
        /// 远端文件版本；目录归并没有内容版本。
        version: Option<VersionId>,
    },
    /// 首次合并时同路径无法证明同一结果：双方都保留，服务器条目保留名称（design §9.2 第 6—7 步）。
    /// 本地副本身份与名称归冲突规则（design §10.1—10.2）。
    PreserveBoth(PreserveReason),
    /// 本卡不裁决：把输入交给后续规则，且不执行任何删除或覆盖。
    Defer(DeferReason),
}

/// 一个条目的同步计划：动作、执行前提、依赖与本地内容保护说明。
///
/// 计划是纯值：不含文件句柄、不承诺执行成功。执行器必须在动任何字节前重新核对
/// [`Plan::prerequisites`]，按 [`Plan::dependencies`] 完成前置任务，并按
/// [`Plan::local_protection`] 保留本地未确认内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// 计划所针对的条目位置：优先取基线位置，没有基线时取本地，再取远端。
    pub path: RelativePath,
    /// 动作。
    pub action: Action,
    /// 执行前必须重新核对的现场条件（顺序同 [`Prerequisite`] 的声明顺序）。
    pub prerequisites: Vec<Prerequisite>,
    /// 必须先完成的任务（顺序同 [`Dependency`] 的声明顺序）。
    pub dependencies: Vec<Dependency>,
    /// 本地未确认内容的保护说明（I02）。
    pub local_protection: LocalProtection,
}

/// 三方比较的输入错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// 基线、本地、远端三项都缺失：没有可比对的条目，不编造计划。
    NoFacts,
    /// 首次合并按规范路径配对两侧事实；两侧路径不同说明调用方没有按路径配对。
    PathMismatch,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFacts => f.write_str("基线、本地与远端事实都缺失，无法规划"),
            Self::PathMismatch => f.write_str("两侧事实的路径不同，无法按同一路径规划"),
        }
    }
}

impl core::error::Error for PlanError {}

/// 三方比较并给出该条目的计划（design §9.1）。
///
/// 每个输入用 `None` 表示该侧没有这个条目（删除事实）。`base` 与 `remote` 是服务器事实，
/// `local` 是本地事实；三项的顺序就是 B、L、R。
///
/// # Errors
///
/// 三项都缺失时返回 [`PlanError::NoFacts`]：既没有可比对的条目，也不存在可执行的动作。
pub fn reconcile(
    base: Option<&ServerFact>,
    local: Option<&LocalFact>,
    remote: Option<&ServerFact>,
) -> Result<Plan, PlanError> {
    let path = base
        .map(|fact| &fact.path)
        .or_else(|| local.map(|fact| &fact.path))
        .or_else(|| remote.map(|fact| &fact.path))
        .ok_or(PlanError::NoFacts)?
        .clone();

    let Some(base) = base else {
        return Ok(deferred(path, DeferReason::FirstSync));
    };
    let (Some(local), Some(remote)) = (local, remote) else {
        return Ok(deferred(path, DeferReason::MissingEntry));
    };
    if remote.entry_id != base.entry_id {
        return Ok(deferred(path, DeferReason::IdentityReplaced));
    }

    let (base_shape, local_shape, remote_shape) = (base.shape(), local.shape(), remote.shape());
    if !local_shape.same_location_and_kind(base_shape)
        || !remote_shape.same_location_and_kind(base_shape)
    {
        return Ok(deferred(path, DeferReason::LocationOrKindChanged));
    }

    let local_unchanged = local_shape.same_result(base_shape);
    let remote_unchanged = remote_shape.same_result(base_shape) && remote.revision == base.revision;
    match (local_unchanged, remote_unchanged) {
        (true, true) => Ok(no_op(path)),
        (true, false) => Ok(apply_remote(path, base, local, remote)),
        (false, true) => Ok(upload_local(path, base, local)),
        (false, false) => {
            if local_shape.same_result(remote_shape) {
                Ok(merge_same_result(path, base, local, remote))
            } else {
                Ok(deferred(path, DeferReason::ContentConflict))
            }
        }
    }
}

/// 无操作：双方都没有变化，不存在需要保护或推进的内容。
fn no_op(path: RelativePath) -> Plan {
    Plan {
        path,
        action: Action::NoOp,
        prerequisites: Vec::new(),
        dependencies: Vec::new(),
        local_protection: LocalProtection::NotNeeded,
    }
}

/// 本卡不裁决的输入（或缺少必需事实）：不生成动作，也不允许执行器改动本地内容。
pub(crate) fn deferred(path: RelativePath, reason: DeferReason) -> Plan {
    Plan {
        path,
        action: Action::Defer(reason),
        prerequisites: Vec::new(),
        dependencies: Vec::new(),
        local_protection: LocalProtection::KeepLocalContent,
    }
}

/// 仅远端变化：先核对使用保护与路径条件，再安装远端版本或只推进元数据（design §9.1 第一条）。
///
/// 冲突规划（[`crate::conflict`]）复用本函数生成“主条目保留服务器 head”的计划，两处的前提与本地
/// 内容保护因此只有一份定义。
pub(crate) fn apply_remote(
    path: RelativePath,
    base: &ServerFact,
    local: &LocalFact,
    remote: &ServerFact,
) -> Plan {
    let mut dependencies = Vec::new();
    let protection = if remote.kind == local.kind {
        // 内容与本地一致（远端只改了元数据）：不替换本地字节。
        LocalProtection::KeepLocalContent
    } else {
        match remote.version {
            Some(version) => {
                dependencies.push(Dependency::DownloadVersion { version });
                LocalProtection::SnapshotBeforeReplace {
                    generation: local.generation,
                }
            }
            // 内容变化却给不出不可变版本：事实不完整，不生成缺少下载前提的计划。
            None => return deferred(path, DeferReason::IncompleteFacts),
        }
    };
    Plan {
        path,
        action: Action::ApplyRemote {
            revision: remote.revision,
            version: remote.version,
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
        local_protection: protection,
    }
}

/// 仅本地变化：生成附带基线修订的本地提交（design §9.1 第二条）。
fn upload_local(path: RelativePath, base: &ServerFact, local: &LocalFact) -> Plan {
    Plan {
        path,
        action: Action::UploadLocal {
            base_revision: base.revision,
            generation: local.generation,
        },
        prerequisites: vec![
            Prerequisite::BaseRevisionCurrent {
                expected: base.revision,
            },
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
        ],
        dependencies: vec![
            Dependency::ParentDirectoryReady,
            Dependency::SnapshotLocalContent {
                generation: local.generation,
            },
        ],
        local_protection: LocalProtection::UploadSnapshot {
            generation: local.generation,
        },
    }
}

/// 双方已证明同一结果：归并为远端 head，不再传输内容（design §9.1 第三条、§10.1）。
///
/// 移动规划复用本函数处理“双方已经到同一位置”的幂等归并，因此两处的前提与保护说明只有一份定义。
pub(crate) fn merge_same_result(
    path: RelativePath,
    base: &ServerFact,
    local: &LocalFact,
    remote: &ServerFact,
) -> Plan {
    Plan {
        path,
        action: Action::MergeSameResult {
            revision: remote.revision,
            version: remote.version,
        },
        prerequisites: vec![
            Prerequisite::BaseRevisionCurrent {
                expected: base.revision,
            },
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
        ],
        dependencies: Vec::new(),
        local_protection: LocalProtection::KeepLocalContent,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Action, ContentFingerprint, DeferReason, Dependency, EntryId, Generation, LocalFact,
        LocalProtection, PlanError, Prerequisite, RelativePath, Revision, ServerFact, UtcTimestamp,
        VersionId, reconcile,
    };

    const ENTRY: EntryId = EntryId::from_bytes([0x11; 16]);
    const OTHER_ENTRY: EntryId = EntryId::from_bytes([0x22; 16]);
    const VERSION_OLD: VersionId = VersionId::from_bytes([0xa1; 16]);
    const VERSION_NEW: VersionId = VersionId::from_bytes([0xb2; 16]);

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

    /// 固定路径与长度的服务器文件事实（只改摘要、修订与版本）。
    fn server_file(digest: u8, revision_value: u64, version: VersionId) -> ServerFact {
        ServerFact::file(
            path("docs/report.txt"),
            content(digest, 120),
            ENTRY,
            revision(revision_value),
            version,
        )
    }

    /// 同一路径的本地文件事实。
    fn local_file(digest: u8, generation_value: u64) -> LocalFact {
        LocalFact::file(
            path("docs/report.txt"),
            content(digest, 120),
            generation(generation_value),
        )
    }

    /// 一条表驱动用例：三方输入与期望动作。
    struct Case {
        name: &'static str,
        base: Option<ServerFact>,
        local: Option<LocalFact>,
        remote: Option<ServerFact>,
        expected: Action,
    }

    /// 逐行核对期望动作，并确认同一输入重复计算结果完全相同。
    fn check_table(cases: &[Case]) {
        for case in cases {
            let plan = reconcile(
                case.base.as_ref(),
                case.local.as_ref(),
                case.remote.as_ref(),
            )
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
            assert_eq!(plan.action, case.expected, "{}", case.name);
            let repeated = reconcile(
                case.base.as_ref(),
                case.local.as_ref(),
                case.remote.as_ref(),
            )
            .expect("同一输入必须仍可规划");
            assert_eq!(plan, repeated, "{} 的计划不稳定", case.name);
        }
    }

    #[test]
    fn four_basic_branches_follow_design_section_9_1() {
        let cases = [
            Case {
                name: "双方无变",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xaa, 7)),
                remote: Some(server_file(0xaa, 4, VERSION_OLD)),
                expected: Action::NoOp,
            },
            Case {
                name: "本地重写为相同内容仍是双方无变",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xaa, 8)),
                remote: Some(server_file(0xaa, 4, VERSION_OLD)),
                expected: Action::NoOp,
            },
            Case {
                name: "仅本地变",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xcc, 8)),
                remote: Some(server_file(0xaa, 4, VERSION_OLD)),
                expected: Action::UploadLocal {
                    base_revision: revision(4),
                    generation: generation(8),
                },
            },
            Case {
                name: "仅远端变",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xaa, 7)),
                remote: Some(server_file(0xbb, 5, VERSION_NEW)),
                expected: Action::ApplyRemote {
                    revision: revision(5),
                    version: Some(VERSION_NEW),
                },
            },
            Case {
                name: "双方同结果",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xdd, 9)),
                remote: Some(server_file(0xdd, 6, VERSION_NEW)),
                expected: Action::MergeSameResult {
                    revision: revision(6),
                    version: Some(VERSION_NEW),
                },
            },
        ];
        check_table(&cases);
    }

    #[test]
    fn apply_remote_carries_usage_and_path_preconditions_with_download() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xaa, 7);
        let remote = server_file(0xbb, 5, VERSION_NEW);

        let plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");

        assert_eq!(plan.path, path("docs/report.txt"));
        assert_eq!(
            plan.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(7)
                },
                Prerequisite::UsagePermitsChange,
                Prerequisite::TargetPathUsable,
            ]
        );
        assert_eq!(
            plan.dependencies,
            [Dependency::DownloadVersion {
                version: VERSION_NEW
            }]
        );
        assert_eq!(
            plan.local_protection,
            LocalProtection::SnapshotBeforeReplace {
                generation: generation(7)
            }
        );
    }

    #[test]
    fn remote_metadata_change_needs_no_download_or_replacement() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xaa, 7);
        // 远端修订前进但内容版本与本地相同：只推进元数据与基线。
        let remote = server_file(0xaa, 5, VERSION_OLD);

        let plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");

        assert_eq!(
            plan.action,
            Action::ApplyRemote {
                revision: revision(5),
                version: Some(VERSION_OLD),
            }
        );
        assert!(plan.dependencies.is_empty(), "内容未变不应要求下载");
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
    }

    #[test]
    fn upload_local_carries_base_revision_and_snapshot_generation() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xcc, 8);
        let remote = server_file(0xaa, 4, VERSION_OLD);

        let plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");

        assert_eq!(
            plan.action,
            Action::UploadLocal {
                base_revision: revision(4),
                generation: generation(8),
            }
        );
        assert_eq!(
            plan.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(8)
                },
            ]
        );
        assert_eq!(
            plan.dependencies,
            [
                Dependency::ParentDirectoryReady,
                Dependency::SnapshotLocalContent {
                    generation: generation(8)
                },
            ]
        );
        assert_eq!(
            plan.local_protection,
            LocalProtection::UploadSnapshot {
                generation: generation(8)
            }
        );
    }

    #[test]
    fn merge_keeps_local_bytes_until_the_baseline_moves() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xdd, 9);
        let remote = server_file(0xdd, 6, VERSION_NEW);

        let plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");

        assert_eq!(plan.dependencies, [], "归并不需要传输或快照");
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
        assert_eq!(
            plan.prerequisites,
            [
                Prerequisite::BaseRevisionCurrent {
                    expected: revision(4)
                },
                Prerequisite::GenerationUnchanged {
                    expected: generation(9)
                },
            ]
        );
    }

    #[test]
    fn no_op_asks_for_no_prerequisite_or_protection() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xaa, 7);
        let remote = server_file(0xaa, 4, VERSION_OLD);

        let plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");

        assert_eq!(plan.action, Action::NoOp);
        assert!(plan.prerequisites.is_empty());
        assert!(plan.dependencies.is_empty());
        assert_eq!(plan.local_protection, LocalProtection::NotNeeded);
    }

    #[test]
    fn directory_entries_plan_like_files_without_content_versions() {
        let base = ServerFact::directory(path("docs"), ENTRY, revision(3));
        let local = LocalFact::directory(path("docs"), generation(5));
        let remote = ServerFact::directory(path("docs"), ENTRY, revision(4));

        let unchanged = reconcile(Some(&base), Some(&local), Some(&base)).expect("可规划");
        assert_eq!(unchanged.action, Action::NoOp);

        let remote_changed = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");
        assert_eq!(
            remote_changed.action,
            Action::ApplyRemote {
                revision: revision(4),
                version: None,
            }
        );
        assert!(remote_changed.dependencies.is_empty());
        assert_eq!(
            remote_changed.local_protection,
            LocalProtection::KeepLocalContent
        );
    }

    #[test]
    fn equal_mtime_never_merges_different_content() {
        let observed = mtime(1_700_000_000);
        let local_content = content(0xcc, 120);
        let remote_content = content(0xdd, 120);
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = LocalFact::file(path("docs/report.txt"), local_content, generation(9))
            .with_observed_mtime(observed);
        let remote = ServerFact::file(
            path("docs/report.txt"),
            remote_content,
            ENTRY,
            revision(6),
            VERSION_NEW,
        )
        .with_observed_mtime(observed);

        // 长度与修改时间都相同，只有内容摘要不同：mtime 不参与判定。
        assert_eq!(local.observed_mtime, remote.observed_mtime);
        assert_eq!(
            local_content.size(),
            remote_content.size(),
            "前提：长度相同"
        );
        assert_ne!(
            local_content.sha256(),
            remote_content.sha256(),
            "前提：摘要不同"
        );

        let plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");

        assert_eq!(plan.action, Action::Defer(DeferReason::ContentConflict));
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
    }

    #[test]
    fn same_content_with_different_mtime_still_merges() {
        let base = server_file(0xaa, 4, VERSION_OLD);
        let local = local_file(0xdd, 9).with_observed_mtime(mtime(1_700_000_000));
        let remote = server_file(0xdd, 6, VERSION_NEW).with_observed_mtime(mtime(1_600_000_000));

        let plan = reconcile(Some(&base), Some(&local), Some(&remote)).expect("可规划");

        assert_eq!(
            plan.action,
            Action::MergeSameResult {
                revision: revision(6),
                version: Some(VERSION_NEW),
            }
        );
    }

    #[test]
    fn content_equality_requires_both_size_and_digest() {
        assert_eq!(content(0x01, 10), content(0x01, 10));
        assert_ne!(content(0x01, 10), content(0x02, 10));
        assert_ne!(content(0x01, 10), content(0x01, 11));
        assert_eq!(content(0x01, 10).size(), 10);
        assert_eq!(content(0x01, 10).sha256(), [0x01; 32]);
    }

    #[test]
    fn unsupported_inputs_defer_without_touching_local_content() {
        let cases = [
            Case {
                name: "没有共同基线",
                base: None,
                local: Some(local_file(0xcc, 8)),
                remote: Some(server_file(0xaa, 4, VERSION_OLD)),
                expected: Action::Defer(DeferReason::FirstSync),
            },
            Case {
                name: "本地缺失",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: None,
                remote: Some(server_file(0xbb, 5, VERSION_NEW)),
                expected: Action::Defer(DeferReason::MissingEntry),
            },
            Case {
                name: "远端缺失",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xcc, 8)),
                remote: None,
                expected: Action::Defer(DeferReason::MissingEntry),
            },
            Case {
                name: "远端身份被替换",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xaa, 7)),
                remote: Some(ServerFact::file(
                    path("docs/report.txt"),
                    content(0xaa, 120),
                    OTHER_ENTRY,
                    revision(9),
                    VERSION_NEW,
                )),
                expected: Action::Defer(DeferReason::IdentityReplaced),
            },
            Case {
                name: "本地位置变化",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(LocalFact::file(
                    path("docs/moved.txt"),
                    content(0xcc, 120),
                    generation(8),
                )),
                remote: Some(server_file(0xaa, 4, VERSION_OLD)),
                expected: Action::Defer(DeferReason::LocationOrKindChanged),
            },
            Case {
                name: "远端位置变化",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xaa, 7)),
                remote: Some(ServerFact::file(
                    path("docs/renamed.txt"),
                    content(0xaa, 120),
                    ENTRY,
                    revision(5),
                    VERSION_OLD,
                )),
                expected: Action::Defer(DeferReason::LocationOrKindChanged),
            },
            Case {
                name: "远端类型变化",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xaa, 7)),
                remote: Some(ServerFact::directory(
                    path("docs/report.txt"),
                    ENTRY,
                    revision(5),
                )),
                expected: Action::Defer(DeferReason::LocationOrKindChanged),
            },
            Case {
                name: "双方都变且结果不同",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xcc, 8)),
                remote: Some(server_file(0xdd, 5, VERSION_NEW)),
                expected: Action::Defer(DeferReason::ContentConflict),
            },
            Case {
                name: "内容变化却没有远端版本",
                base: Some(server_file(0xaa, 4, VERSION_OLD)),
                local: Some(local_file(0xaa, 7)),
                remote: Some(ServerFact {
                    version: None,
                    ..server_file(0xbb, 5, VERSION_NEW)
                }),
                expected: Action::Defer(DeferReason::IncompleteFacts),
            },
        ];
        check_table(&cases);

        for case in &cases {
            let plan = reconcile(
                case.base.as_ref(),
                case.local.as_ref(),
                case.remote.as_ref(),
            )
            .expect("延期裁决仍是可规划结果");
            assert!(plan.prerequisites.is_empty(), "{}", case.name);
            assert!(plan.dependencies.is_empty(), "{}", case.name);
            assert_eq!(
                plan.local_protection,
                LocalProtection::KeepLocalContent,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn missing_every_side_is_an_error() {
        assert_eq!(reconcile(None, None, None), Err(PlanError::NoFacts));
        assert!(PlanError::NoFacts.to_string().contains("缺失"));
    }
}
