//! 目录创建与同 Vault 移动：与内容 put 共用同一套持久意图、阶段与恢复顺序（design §9.5、§11.2—11.3、
//! T09/T10）。
//!
//! 一次命名空间变化只有两种：**新建目录**（在已存在的父目录下挂一个新条目）与**同 Vault 移动**
//! （把一个条目的父关系与名称改掉）。两者都遵守 put 的顺序与不变量：
//!
//! 1. **先持久意图再动磁盘**：`prepared` 意图里记着现场前提（`expected_before`：条目当时挂在哪里、是哪
//!    个 inode、revision 是多少）与目标位置（`desired_after`），写不进去就什么都不动；
//! 2. **动完要复核**：`mkdir`/`rename` 之后实测目标必须是意图目标（目录存在；移动还要求目标 inode 与
//!    前提是同一个、源位置已经不存在），不符就不推进阶段；
//! 3. **阶段推进与元数据事务同一顺序**：`prepared → published`（磁盘已改）→ 元数据事务把意图收尾到
//!    `finalized`；没有事务就没有 ACK（I01）；
//! 4. **失败不是 committed**：任何失败都返回 [`NamespaceFailure`]（`is_committed()` 恒为 `false`），已
//!    持久化意图保留为恢复证据，由 [`recover_namespace_vault`] 按证据重做或收尾（§11.3）。
//!
//! 本模块固定的判定：
//!
//! - **`EntryId` 不随路径变**（design §9.5）：移动只写父关系与名称，元数据事务不得写版本行、不得改
//!   `entries.content_version_id`，因此移动的内容流量为零、子内容不重新上传；
//! - **不覆盖目标**：目标位置已有条目、目标祖先已删除、目标就是自己的子树（目录环）时给重新规划类
//!   拒绝，不动磁盘（design §10.1“目标路径已存在，或目标祖先已删除 → 不覆盖目标”）；
//! - **父目录状态与基线必须在动磁盘之前核对**：epoch、条目 revision/父/名、目标父目录存在且是目录，
//!   任一不成立都不写意图；
//! - **故障不是不存在**：路径读不到是 [`NamespaceFailureReason::TargetUnreadable`]，不会被当成“目标空着
//!   可以创建”。恢复同理走向 `needs_recovery` 停写。

use std::collections::BTreeMap;
use std::fmt;

use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{EntryId, IntentId, VaultId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp};

use crate::commit::intent::{
    JsonValue, PutPhase, StateJsonError, finish_object, parse_flat_object, take_bool, take_entry,
    take_text, take_u64,
};
use crate::commit::put::{CommitGateError, VaultCommitGate};
use crate::ports::intent_log::{IntentLogError, NamespaceIntentLog};
use crate::ports::namespace::{NamespaceError, NamespacePort, PathState};
use crate::ports::namespace_store::{
    NamespaceCommitStore, NamespaceLocation, NamespaceRecord, NamespaceStoreError, NamespaceWrite,
};
use crate::ports::object_store::FileKey;
use crate::ports::put_store::{EntryBaseline, EntryKind};
use crate::recovery::NeedsRecoveryReason;

/// 命名空间变化的种类（与 `fs_intents.kind` 的 `create`/`move` 同一套取值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceOpKind {
    /// 新建目录。
    Create,
    /// 同 Vault 移动（文件或目录）。
    Move,
}

impl NamespaceOpKind {
    /// 数据库里的规范文本。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            NamespaceOpKind::Create => "create",
            NamespaceOpKind::Move => "move",
        }
    }

    /// 解析数据库列的值；未知文本返回 `None`，不猜操作种类。
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "create" => Some(NamespaceOpKind::Create),
            "move" => Some(NamespaceOpKind::Move),
            _ => None,
        }
    }
}

impl fmt::Display for NamespaceOpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_text())
    }
}

/// 意图开始时的位置快照（`expected_before` 的 `present: true` 形态）。
///
/// `identity` 让恢复能证明“目标上的条目就是被移动的那一个”（`rename` 保 inode），从而在不看内容的
/// 前提下判断移动是否已经落地。`revision` 是事务内比较并写的基线，`path` 是端口用的 Vault 内相对路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceSnapshot {
    /// 条目身份。
    pub entry: EntryId,
    /// 条目 revision（十进制无符号值）。
    pub revision: u64,
    /// 父目录条目。
    pub parent: EntryId,
    /// 条目在 Vault 内的相对路径（最后一段即名称）。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
    /// 实测身份（目录/文件的 `st_dev`/`st_ino`）。
    pub identity: FileKey,
}

/// 意图前提：目标位置在意图开始时是什么。
///
/// 新建目录的前提是 [`NamespaceBefore::Absent`]；移动的前提是 [`NamespaceBefore::Present`]（源位置与
/// 基线快照）。两种状态都由字段表达，不用缺行暗示（design §11.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceBefore {
    /// 目标位置当时没有条目（新建目录）。
    Absent,
    /// 条目当时挂在这份位置上（移动）。
    Present(NamespaceSnapshot),
}

impl NamespaceBefore {
    /// 移动的源快照（新建时为 `None`）。
    #[must_use]
    pub fn present(&self) -> Option<&NamespaceSnapshot> {
        match self {
            NamespaceBefore::Absent => None,
            NamespaceBefore::Present(snapshot) => Some(snapshot),
        }
    }

    /// `expected_before` 的规范 JSON 文本。
    ///
    /// 路径按 UTF-8 字节的十六进制编码：证据解析器刻意不接受转义序列（与 put 证据同一套编码器），
    /// 名称里合法出现的 `"`/`\` 必须由编码兜住，而不是靠“名字通常很干净”。
    #[must_use]
    pub fn to_json(&self) -> String {
        match self {
            NamespaceBefore::Absent => r#"{"present":false}"#.to_owned(),
            NamespaceBefore::Present(snapshot) => format!(
                concat!(
                    r#"{{"present":true"#,
                    r#","entry":"{}""#,
                    r#","revision":"{}""#,
                    r#","parent":"{}""#,
                    r#","path_hex":"{}""#,
                    r#","kind":"{}""#,
                    r#","size":"{}""#,
                    r#","device":"{}""#,
                    r#","inode":"{}""#,
                    r#","links":"{}"}}"#
                ),
                snapshot.entry.to_hex(),
                snapshot.revision,
                snapshot.parent.to_hex(),
                hex_encode(snapshot.path.as_bytes()),
                snapshot.kind.as_text(),
                snapshot.identity.bytes,
                snapshot.identity.device,
                snapshot.identity.inode,
                snapshot.identity.links
            ),
        }
    }

    /// 从 `expected_before` 的 JSON 文本还原；任何不符合本模块编码的文本都报错，不猜现场。
    ///
    /// # Errors
    ///
    /// 见 [`StateJsonError`]：不是 JSON object、缺字段、类型不符、未知键、非法值。
    pub fn from_json(text: &str) -> Result<Self, StateJsonError> {
        let mut fields = parse_flat_object(text)?;
        if !take_bool(&mut fields, "present")? {
            return finish_object(&fields, NamespaceBefore::Absent);
        }
        let entry = take_entry(&mut fields, "entry")?;
        let revision = take_u64(&mut fields, "revision")?;
        let parent = take_entry(&mut fields, "parent")?;
        let path = take_path(&mut fields, "path_hex")?;
        let kind = take_kind(&mut fields, "kind")?;
        let bytes = take_u64(&mut fields, "size")?;
        let device = take_u64(&mut fields, "device")?;
        let inode = take_u64(&mut fields, "inode")?;
        let links = u32::try_from(take_u64(&mut fields, "links")?).map_err(|_| {
            StateJsonError::InvalidValue {
                field: "links",
                detail: "超出 u32".to_owned(),
            }
        })?;
        finish_object(
            &fields,
            NamespaceBefore::Present(NamespaceSnapshot {
                entry,
                revision,
                parent,
                path,
                kind,
                identity: FileKey {
                    device,
                    inode,
                    links,
                    bytes,
                },
            }),
        )
    }
}

/// 意图目标：变化完成后条目应挂在哪个位置。
///
/// revision 由服务器在事务里分配，因此不属于目标；`entry` 是同一个身份（新建时是新身份）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceAfter {
    /// 条目身份。
    pub entry: EntryId,
    /// 目标父目录。
    pub parent: EntryId,
    /// 目标在 Vault 内的相对路径（最后一段即名称）。
    pub path: String,
    /// 条目种类。
    pub kind: EntryKind,
}

impl NamespaceAfter {
    /// `desired_after` 的规范 JSON 文本。
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                r#"{{"present":true"#,
                r#","entry":"{}""#,
                r#","parent":"{}""#,
                r#","path_hex":"{}""#,
                r#","kind":"{}"}}"#
            ),
            self.entry.to_hex(),
            self.parent.to_hex(),
            hex_encode(self.path.as_bytes()),
            self.kind.as_text()
        )
    }

    /// 从 `desired_after` 的 JSON 文本还原；命名空间操作的目标必须是“存在”的形态。
    ///
    /// # Errors
    ///
    /// 见 [`StateJsonError`]；`{"present":false}` 不是合法的命名空间目标。
    pub fn from_json(text: &str) -> Result<Self, StateJsonError> {
        let mut fields = parse_flat_object(text)?;
        if !take_bool(&mut fields, "present")? {
            return Err(StateJsonError::InvalidValue {
                field: "present",
                detail: "命名空间操作的目标必须存在".to_owned(),
            });
        }
        let entry = take_entry(&mut fields, "entry")?;
        let parent = take_entry(&mut fields, "parent")?;
        let path = take_path(&mut fields, "path_hex")?;
        let kind = take_kind(&mut fields, "kind")?;
        finish_object(
            &fields,
            NamespaceAfter {
                entry,
                parent,
                path,
                kind,
            },
        )
    }

    /// 目标位置的名称（路径最后一段）。
    #[must_use]
    pub fn name(&self) -> &str {
        name_of(&self.path)
    }
}

/// 一条新命名空间意图的输入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewNamespaceIntent {
    /// 意图身份（由调用方生成，重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 所属 Vault。
    pub vault: VaultId,
    /// 操作种类。
    pub op: NamespaceOpKind,
    /// 现场前提。
    pub before: NamespaceBefore,
    /// 目标位置。
    pub after: NamespaceAfter,
}

impl NewNamespaceIntent {
    /// 用 `prepared` 阶段表示这条意图（写库成功后磁盘尚未改动时的形态）。
    #[must_use]
    pub fn prepared(&self) -> NamespaceIntent {
        NamespaceIntent {
            intent_id: self.intent_id,
            vault: self.vault,
            op: self.op,
            before: self.before.clone(),
            after: self.after.clone(),
            phase: PutPhase::Prepared,
        }
    }
}

/// 一条已持久化的命名空间意图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceIntent {
    /// 意图身份。
    pub intent_id: IntentId,
    /// 所属 Vault。
    pub vault: VaultId,
    /// 操作种类。
    pub op: NamespaceOpKind,
    /// 现场前提（已解码）。
    pub before: NamespaceBefore,
    /// 目标位置（已解码）。
    pub after: NamespaceAfter,
    /// 持久化阶段。
    pub phase: PutPhase,
}

impl NamespaceIntent {
    /// 目标路径：`fs_intents.target_path` 列与恢复观察都用它。
    #[must_use]
    pub fn target_path(&self) -> &str {
        &self.after.path
    }

    /// 恢复判断用的一句话现场描述。
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.before {
            NamespaceBefore::Absent => format!(
                "意图 {} 阶段 {} 新建{} {}",
                self.intent_id,
                self.phase.as_text(),
                self.after.kind.as_text(),
                self.after.path
            ),
            NamespaceBefore::Present(snapshot) => format!(
                "意图 {} 阶段 {} 移动 {} {} → {}",
                self.intent_id,
                self.phase.as_text(),
                self.after.kind.as_text(),
                snapshot.path,
                self.after.path
            ),
        }
    }
}

/// 移动的一个端点：父目录 + 名称 + Vault 内相对路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceEndpoint {
    /// 父目录条目。
    pub parent: EntryId,
    /// 名称。
    pub name: String,
    /// Vault 内相对路径（最后一段必须等于 `name`）。
    pub path: String,
}

impl NamespaceEndpoint {
    /// 一条基线上的“当前位置”。
    #[must_use]
    pub fn of(baseline: &EntryBaseline, path: &str) -> Self {
        NamespaceEndpoint {
            parent: baseline.parent,
            name: baseline.name.clone(),
            path: path.to_owned(),
        }
    }
}

/// 新建目录的请求：在已存在的父目录下挂一个空目录条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDirectoryRequest {
    /// 意图身份（重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 所属 Vault。
    pub vault: VaultId,
    /// 新条目的身份（由调用方分配；已存在即拒绝，不复用别人的身份）。
    pub entry: EntryId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 目标父目录（必须已存在且是目录）。
    pub parent: EntryId,
    /// 目标名称。
    pub name: String,
    /// 目标在 Vault 内的相对路径（最后一段必须等于 `name`）。
    pub target_path: String,
}

/// 同 Vault 移动的请求：条目的父关系与名称改变，身份与内容引用不变。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRequest {
    /// 意图身份（重试必须复用同一个身份）。
    pub intent_id: IntentId,
    /// 所属 Vault。
    pub vault: VaultId,
    /// 被移动的条目。
    pub entry: EntryId,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 客户端计划时的条目 revision。
    pub expected_revision: Revision,
    /// 当前位置。
    pub from: NamespaceEndpoint,
    /// 目标位置。
    pub to: NamespaceEndpoint,
}

/// 一次命名空间变化的请求。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceRequest {
    /// 新建目录。
    CreateDirectory(CreateDirectoryRequest),
    /// 同 Vault 移动。
    Move(MoveRequest),
}

impl NamespaceRequest {
    /// 所属 Vault。
    #[must_use]
    pub fn vault(&self) -> VaultId {
        match self {
            NamespaceRequest::CreateDirectory(request) => request.vault,
            NamespaceRequest::Move(request) => request.vault,
        }
    }

    /// 意图身份。
    #[must_use]
    pub fn intent_id(&self) -> IntentId {
        match self {
            NamespaceRequest::CreateDirectory(request) => request.intent_id,
            NamespaceRequest::Move(request) => request.intent_id,
        }
    }

    /// 客户端事实基础所属的服务器 epoch。
    #[must_use]
    pub fn expected_epoch(&self) -> ServerEpoch {
        match self {
            NamespaceRequest::CreateDirectory(request) => request.expected_epoch,
            NamespaceRequest::Move(request) => request.expected_epoch,
        }
    }
}

/// 一次成功提交：元数据事务已提交，意图已收尾，目标位置实测等于意图目标。
#[derive(Clone, Debug)]
pub struct CommittedNamespace {
    /// 元数据事务结果（`phase` 必须是 `finalized`）。
    pub record: NamespaceRecord,
    /// 提交后目标位置的实测现场。
    pub target: PathState,
}

impl CommittedNamespace {
    /// 已收尾的意图身份。
    #[must_use]
    pub fn intent_id(&self) -> IntentId {
        self.record.intent_id
    }
}

/// 命名空间提交失败的原因。
#[derive(Clone, Debug)]
pub enum NamespaceFailureReason {
    /// 请求本身不成立（名称、路径、目录环、自指），未做任何 I/O。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
    /// 同一 Vault 已有提交在进行。
    Gate(CommitGateError),
    /// Vault epoch 与请求预期不符（`EPOCH_CHANGED`）。
    EpochChanged {
        /// 请求携带的 epoch。
        expected: ServerEpoch,
        /// Vault 当前的 epoch。
        actual: ServerEpoch,
    },
    /// 条目 revision/父目录/名称已不是请求基线（`BASE_MISMATCH`，I04）。
    BaseMismatch {
        /// 原因文本。
        detail: String,
    },
    /// 父目录或目标位置在计划后变化（`SUBTREE_CHANGED`）。
    SubtreeChanged {
        /// 原因文本。
        detail: String,
    },
    /// 新建请求的条目身份已经被占用：不复用别人的身份。
    EntryAlreadyExists {
        /// 冲突的条目。
        entry: EntryId,
    },
    /// 路径读不到（权限、挂载、IO）：故障不是“不存在”。
    TargetUnreadable(NamespaceError),
    /// 目标位置已经被别的条目占用：不覆盖，重新规划另名（design §10.1）。
    TargetOccupied {
        /// 目标路径。
        path: String,
        /// 实测现场。
        observed: PathState,
    },
    /// 目标祖先目录不存在：不重建、不覆盖，重新规划（design §10.1）。
    TargetAncestorMissing {
        /// 目标路径。
        path: String,
    },
    /// 移动的源位置与请求基线不符（`RECONCILE_REQUIRED`）：先采集外部变化再重新计划。
    SourceChanged {
        /// 源路径。
        path: String,
        /// 实测现场。
        observed: PathState,
    },
    /// 意图日志失败。
    Intent(IntentLogError),
    /// 命名空间动作失败（含 `ENOSPC` 等存储分类）。
    Apply(NamespaceError),
    /// 动作报告成功，但实测现场不是意图目标（没有换 inode、源还在、种类不符）：不推进阶段。
    AppliedStateMismatch {
        /// 原因文本。
        detail: String,
        /// 实测现场。
        observed: PathState,
    },
    /// 存储层失败（基线读取或元数据事务）。
    Store(NamespaceStoreError),
    /// 存储层报告事务成功，但返回的证据与请求不一致：适配器缺陷，必须停写人工核对。
    StoreContract {
        /// 原因文本。
        detail: String,
    },
}

impl NamespaceFailureReason {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            NamespaceFailureReason::InvalidRequest { .. } => "invalid_request",
            NamespaceFailureReason::Gate(error) => error.code(),
            NamespaceFailureReason::EpochChanged { .. } => "epoch_changed",
            NamespaceFailureReason::BaseMismatch { .. } => "base_mismatch",
            NamespaceFailureReason::SubtreeChanged { .. } => "subtree_changed",
            NamespaceFailureReason::EntryAlreadyExists { .. } => "entry_already_exists",
            NamespaceFailureReason::TargetUnreadable(error) => error.code(),
            NamespaceFailureReason::TargetOccupied { .. } => "target_occupied",
            NamespaceFailureReason::TargetAncestorMissing { .. } => "target_ancestor_missing",
            NamespaceFailureReason::SourceChanged { .. } => "source_changed",
            NamespaceFailureReason::Intent(error) => error.code(),
            NamespaceFailureReason::Apply(error) => error.code(),
            NamespaceFailureReason::AppliedStateMismatch { .. } => "applied_state_mismatch",
            NamespaceFailureReason::Store(error) => error.code(),
            NamespaceFailureReason::StoreContract { .. } => "store_contract_violation",
        }
    }

    /// wire 错误码（design §15.4）；服务端内部原因没有对应 wire 码时返回 `None`。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            NamespaceFailureReason::EpochChanged { .. } => Some(ErrorCode::EpochChanged),
            NamespaceFailureReason::BaseMismatch { .. }
            | NamespaceFailureReason::EntryAlreadyExists { .. } => Some(ErrorCode::BaseMismatch),
            NamespaceFailureReason::SubtreeChanged { .. }
            | NamespaceFailureReason::TargetOccupied { .. }
            | NamespaceFailureReason::TargetAncestorMissing { .. } => {
                Some(ErrorCode::SubtreeChanged)
            }
            NamespaceFailureReason::SourceChanged { .. } => Some(ErrorCode::ReconcileRequired),
            NamespaceFailureReason::StoreContract { .. } => Some(ErrorCode::NeedsRecovery),
            _ => None,
        }
    }
}

impl fmt::Display for NamespaceFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceFailureReason::InvalidRequest { detail } => write!(f, "请求不成立：{detail}"),
            NamespaceFailureReason::Gate(error) => write!(f, "{error}"),
            NamespaceFailureReason::EpochChanged { expected, actual } => write!(
                f,
                "Vault epoch 已是 {actual}，请求预期是 {expected}：服务器事实已重建，旧提交作废"
            ),
            NamespaceFailureReason::BaseMismatch { detail } => {
                write!(f, "基线已不是请求的 revision：{detail}")
            }
            NamespaceFailureReason::SubtreeChanged { detail } => {
                write!(f, "命名空间基线已变化：{detail}")
            }
            NamespaceFailureReason::EntryAlreadyExists { entry } => {
                write!(f, "条目身份 {entry} 已被占用：新建不得复用别人的身份")
            }
            NamespaceFailureReason::TargetUnreadable(error) => write!(f, "{error}"),
            NamespaceFailureReason::TargetOccupied { path, observed } => write!(
                f,
                "目标 {path} 已被占用（{observed}）：不覆盖目标，重新规划另名"
            ),
            NamespaceFailureReason::TargetAncestorMissing { path } => {
                write!(f, "目标 {path} 的祖先目录不存在：不重建、不覆盖，重新规划")
            }
            NamespaceFailureReason::SourceChanged { path, observed } => write!(
                f,
                "源位置 {path} 现在是 {observed}，与请求基线不符：先采集外部变化再重新计划"
            ),
            NamespaceFailureReason::Intent(error) => write!(f, "{error}"),
            NamespaceFailureReason::Apply(error) => write!(f, "{error}"),
            NamespaceFailureReason::AppliedStateMismatch { detail, observed } => write!(
                f,
                "命名空间动作报告成功，但实测 {observed} 与意图目标不符：{detail}"
            ),
            NamespaceFailureReason::Store(error) => write!(f, "{error}"),
            NamespaceFailureReason::StoreContract { detail } => write!(
                f,
                "存储层报告提交成功但证据不一致：{detail}（必须停写人工核对）"
            ),
        }
    }
}

/// 命名空间提交失败：原因 + 已持久化的意图身份与阶段。
#[derive(Clone, Debug)]
pub struct NamespaceFailure {
    /// 失败原因。
    pub reason: NamespaceFailureReason,
    /// 已持久化的意图（未写库时为 `None`）。
    pub intent_id: Option<IntentId>,
    /// 最近一次成功持久化的阶段。
    pub phase: Option<PutPhase>,
}

impl NamespaceFailure {
    /// 用原因构造“尚未写库”的失败。
    #[must_use]
    pub fn new(reason: NamespaceFailureReason) -> Self {
        NamespaceFailure {
            reason,
            intent_id: None,
            phase: None,
        }
    }

    /// 用原因与该意图已持久化的阶段构造失败。
    #[must_use]
    pub fn after_intent(reason: NamespaceFailureReason, intent: &NamespaceIntent) -> Self {
        NamespaceFailure {
            reason,
            intent_id: Some(intent.intent_id),
            phase: Some(intent.phase),
        }
    }

    /// 失败是否产生了已提交结果：恒为 `false`（没有元数据事务就没有 committed）。
    #[must_use]
    pub fn is_committed(&self) -> bool {
        false
    }

    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.reason.code()
    }

    /// wire 错误码（design §15.4）。
    #[must_use]
    pub fn error_code(&self) -> Option<ErrorCode> {
        self.reason.error_code()
    }
}

impl fmt::Display for NamespaceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.intent_id, self.phase) {
            (Some(id), Some(phase)) => {
                write!(f, "{}（意图 {id} 停在 {}）", self.reason, phase.as_text())
            }
            _ => write!(f, "{}", self.reason),
        }
    }
}

impl std::error::Error for NamespaceFailure {}

/// 提交一次目录创建或同 Vault 移动。
///
/// 顺序：请求形状（名称/路径/目录环）→ Vault 串行化 → epoch → 条目与父目录基线 → 目标现场 →
/// 意图（`prepared`）→ 命名空间动作与复核 → `published` → 元数据事务 → ACK。无论成功还是失败，退出前
/// 都会释放 Vault 票据。
///
/// # Errors
///
/// 见 [`NamespaceFailure`]：请求不成立、Vault 忙、epoch 变化、基线过期、目标占用/祖先缺失/源不一致、
/// 意图/动作/存储失败、存储契约不一致。失败都不是 committed，已持久化的意图行保留为恢复证据。
pub async fn commit_namespace<S, L, N>(
    gate: &mut VaultCommitGate,
    store: &mut S,
    log: &mut L,
    ns: &mut N,
    at: UtcTimestamp,
    request: &NamespaceRequest,
) -> Result<CommittedNamespace, Box<NamespaceFailure>>
where
    S: NamespaceCommitStore + ?Sized,
    L: NamespaceIntentLog + ?Sized,
    N: NamespacePort + ?Sized,
{
    validate_request(request)?;
    let ticket = gate
        .acquire(request.vault())
        .map_err(|error| failure(NamespaceFailureReason::Gate(error)))?;
    let outcome = commit_under_ticket(store, log, ns, at, request).await;
    gate.release(ticket);
    outcome
}

async fn commit_under_ticket<S, L, N>(
    store: &mut S,
    log: &mut L,
    ns: &mut N,
    at: UtcTimestamp,
    request: &NamespaceRequest,
) -> Result<CommittedNamespace, Box<NamespaceFailure>>
where
    S: NamespaceCommitStore + ?Sized,
    L: NamespaceIntentLog + ?Sized,
    N: NamespacePort + ?Sized,
{
    // 1. epoch：恢复过的服务器不是同一套事实。
    let facts = store
        .vault_facts(request.vault())
        .await
        .map_err(store_failure)?;
    if facts.epoch != request.expected_epoch() {
        return Err(failure(NamespaceFailureReason::EpochChanged {
            expected: request.expected_epoch(),
            actual: facts.epoch,
        }));
    }
    match request {
        NamespaceRequest::CreateDirectory(create) => {
            commit_create(store, log, ns, at, create).await
        }
        NamespaceRequest::Move(mv) => commit_move(store, log, ns, at, mv).await,
    }
}

async fn commit_create<S, L, N>(
    store: &mut S,
    log: &mut L,
    ns: &mut N,
    at: UtcTimestamp,
    request: &CreateDirectoryRequest,
) -> Result<CommittedNamespace, Box<NamespaceFailure>>
where
    S: NamespaceCommitStore + ?Sized,
    L: NamespaceIntentLog + ?Sized,
    N: NamespacePort + ?Sized,
{
    // 条目身份必须是新的：复用已有身份就是覆盖别人的条目。
    match store.entry_baseline(request.vault, request.entry).await {
        Err(NamespaceStoreError::EntryMissing { .. }) => {}
        Ok(_) => {
            return Err(failure(NamespaceFailureReason::EntryAlreadyExists {
                entry: request.entry,
            }));
        }
        Err(NamespaceStoreError::UnknownVault { vault }) => {
            return Err(base_mismatch(format!("Vault {vault} 已不存在")));
        }
        Err(error) => return Err(store_failure(error)),
    }
    check_directory_parent(store, request.vault, request.parent).await?;
    inspect_vacant(ns, &request.target_path)?;

    let intent = NewNamespaceIntent {
        intent_id: request.intent_id,
        vault: request.vault,
        op: NamespaceOpKind::Create,
        before: NamespaceBefore::Absent,
        after: NamespaceAfter {
            entry: request.entry,
            parent: request.parent,
            path: request.target_path.clone(),
            kind: EntryKind::Directory,
        },
    };
    let plan = NamespacePlan {
        intent,
        from_path: None,
    };
    execute_plan(store, log, ns, at, &plan).await
}

async fn commit_move<S, L, N>(
    store: &mut S,
    log: &mut L,
    ns: &mut N,
    at: UtcTimestamp,
    request: &MoveRequest,
) -> Result<CommittedNamespace, Box<NamespaceFailure>>
where
    S: NamespaceCommitStore + ?Sized,
    L: NamespaceIntentLog + ?Sized,
    N: NamespacePort + ?Sized,
{
    // 条目基线：身份、revision、父目录与名称都必须是请求的那一份。
    let baseline = match store.entry_baseline(request.vault, request.entry).await {
        Ok(baseline) => baseline,
        Err(NamespaceStoreError::EntryMissing { entry }) => {
            return Err(base_mismatch(format!("条目 {entry} 已不存在")));
        }
        Err(NamespaceStoreError::UnknownVault { vault }) => {
            return Err(base_mismatch(format!("Vault {vault} 已不存在")));
        }
        Err(error) => return Err(store_failure(error)),
    };
    check_move_baseline(&baseline, request)?;
    check_directory_parent(store, request.vault, request.to.parent).await?;

    // 源位置必须还是那个条目：现场变了就重新采集再计划（design §12.2）。
    let source = inspect(ns, &request.from.path)?;
    if !kind_matches(baseline.kind, &source) {
        return Err(failure(NamespaceFailureReason::SourceChanged {
            path: request.from.path.clone(),
            observed: source,
        }));
    }
    let source_identity = match source.identity() {
        Some(identity) => *identity,
        None => {
            return Err(failure(NamespaceFailureReason::SourceChanged {
                path: request.from.path.clone(),
                observed: source,
            }));
        }
    };
    inspect_vacant(ns, &request.to.path)?;

    let intent = NewNamespaceIntent {
        intent_id: request.intent_id,
        vault: request.vault,
        op: NamespaceOpKind::Move,
        before: NamespaceBefore::Present(NamespaceSnapshot {
            entry: baseline.entry,
            revision: baseline.revision.get(),
            parent: baseline.parent,
            path: request.from.path.clone(),
            kind: baseline.kind,
            identity: source_identity,
        }),
        after: NamespaceAfter {
            entry: baseline.entry,
            parent: request.to.parent,
            path: request.to.path.clone(),
            kind: baseline.kind,
        },
    };
    let plan = NamespacePlan {
        intent,
        from_path: Some(request.from.path.clone()),
    };
    execute_plan(store, log, ns, at, &plan).await
}

/// 一次提交的内部计划：意图证据 + 端口需要的源路径（事务的比较值全部来自证据，避免两处基线）。
struct NamespacePlan {
    intent: NewNamespaceIntent,
    from_path: Option<String>,
}

async fn execute_plan<S, L, N>(
    store: &mut S,
    log: &mut L,
    ns: &mut N,
    at: UtcTimestamp,
    plan: &NamespacePlan,
) -> Result<CommittedNamespace, Box<NamespaceFailure>>
where
    S: NamespaceCommitStore + ?Sized,
    L: NamespaceIntentLog + ?Sized,
    N: NamespacePort + ?Sized,
{
    // 步骤 5：先持久意图。
    let prepared = log
        .begin_namespace(&plan.intent, at)
        .await
        .map_err(|error| failure(NamespaceFailureReason::Intent(error)))?;

    // 步骤 6：改命名空间并复核。
    let target = match apply_change(ns, &prepared, plan.from_path.as_deref()) {
        Ok(state) => state,
        Err(reason) => return Err(Box::new(NamespaceFailure::after_intent(reason, &prepared))),
    };

    let published = log
        .advance_namespace(plan.intent.intent_id, PutPhase::Published, at)
        .await
        .map_err(|error| {
            NamespaceFailure::after_intent(NamespaceFailureReason::Intent(error), &prepared)
        })?;

    // 步骤 7：元数据事务与意图收尾同一事务，事务没提交就没有 ACK。
    let write = namespace_write(&published, at)?;
    let record = store
        .apply_namespace(&write)
        .await
        .map_err(|error| match error {
            NamespaceStoreError::RevisionChanged { expected, actual } => {
                NamespaceFailure::after_intent(
                    NamespaceFailureReason::BaseMismatch {
                        detail: format!("提交时条目 revision 已从 {expected} 变为 {actual}"),
                    },
                    &published,
                )
            }
            NamespaceStoreError::SlotOccupied { parent, name } => NamespaceFailure::after_intent(
                NamespaceFailureReason::SubtreeChanged {
                    detail: format!("提交时目标位置 {parent}/{name:?} 已被占用"),
                },
                &published,
            ),
            error => {
                NamespaceFailure::after_intent(NamespaceFailureReason::Store(error), &published)
            }
        })?;
    check_record(&record, &published)?;
    Ok(CommittedNamespace { record, target })
}

/// 在端口上执行命名空间动作并核对实测结果等于意图目标。
fn apply_change<N>(
    ns: &mut N,
    intent: &NamespaceIntent,
    from_path: Option<&str>,
) -> Result<PathState, NamespaceFailureReason>
where
    N: NamespacePort + ?Sized,
{
    match (intent.op, from_path) {
        (NamespaceOpKind::Create, None) => {
            let state = ns
                .create_directory(&intent.after.path)
                .map_err(NamespaceFailureReason::Apply)?;
            if !state.is_directory() {
                return Err(NamespaceFailureReason::AppliedStateMismatch {
                    detail: format!("新建目标 {} 不是目录", intent.after.path),
                    observed: state,
                });
            }
            Ok(state)
        }
        (NamespaceOpKind::Move, Some(from)) => {
            let state = ns
                .rename(from, &intent.after.path)
                .map_err(NamespaceFailureReason::Apply)?;
            let expected = intent
                .before
                .present()
                .map(|snapshot| snapshot.identity)
                .ok_or_else(|| NamespaceFailureReason::InvalidRequest {
                    detail: "移动意图缺少源快照".to_owned(),
                })?;
            if !kind_matches(intent.after.kind, &state) {
                return Err(NamespaceFailureReason::AppliedStateMismatch {
                    detail: format!(
                        "目标 {} 是{}，意图目标是{}",
                        intent.after.path,
                        state.state_name(),
                        intent.after.kind.as_text()
                    ),
                    observed: state,
                });
            }
            if !state.same_inode_as(&expected) {
                return Err(NamespaceFailureReason::AppliedStateMismatch {
                    detail: format!(
                        "目标 {} 的 inode 不是源位置那一个（移动必须保 inode）",
                        intent.after.path
                    ),
                    observed: state,
                });
            }
            // 源位置必须已经不在：否则这不是一次改名，而是复制。
            let source = ns.inspect(from).map_err(NamespaceFailureReason::Apply)?;
            if !matches!(source, PathState::Absent) {
                return Err(NamespaceFailureReason::AppliedStateMismatch {
                    detail: format!("改名后源位置 {from} 仍然存在"),
                    observed: source,
                });
            }
            Ok(state)
        }
        (NamespaceOpKind::Create, Some(_)) | (NamespaceOpKind::Move, None) => {
            Err(NamespaceFailureReason::InvalidRequest {
                detail: "操作种类与源路径不一致".to_owned(),
            })
        }
    }
}

/// 事务声称成功时核对返回的证据：不一致说明适配器违反端口契约，必须停写人工核对，不能 ACK。
fn check_record(
    record: &NamespaceRecord,
    intent: &NamespaceIntent,
) -> Result<(), Box<NamespaceFailure>> {
    let problem = if record.intent_id != intent.intent_id {
        Some(format!(
            "事务收尾的意图 {} 不是请求意图 {}",
            record.intent_id, intent.intent_id
        ))
    } else if !record.phase.is_finished() {
        Some(format!("意图阶段是 {} 而不是 finalized", record.phase))
    } else if record.entry != intent.after.entry {
        Some(format!(
            "事务后条目是 {}，不是意图条目 {}（移动不得换身份）",
            record.entry, intent.after.entry
        ))
    } else {
        match intent.before.present() {
            None if record.revision != Revision::new(0).expect("revision 0 合法") => Some(
                format!("新建条目的 revision 是 {}，不是 0", record.revision),
            ),
            Some(snapshot) if record.revision.get() <= snapshot.revision => Some(format!(
                "条目 revision 没有推进：{} 不大于基线 {}",
                record.revision, snapshot.revision
            )),
            _ => None,
        }
    };
    match problem {
        Some(detail) => Err(Box::new(NamespaceFailure::after_intent(
            NamespaceFailureReason::StoreContract { detail },
            intent,
        ))),
        None => Ok(()),
    }
}

/// 从意图证据拼出元数据事务的输入（提交与恢复共用，避免两条路径写出不同的事务）。
fn namespace_write(
    intent: &NamespaceIntent,
    at: UtcTimestamp,
) -> Result<NamespaceWrite, Box<NamespaceFailure>> {
    let (expected_revision, from) = match &intent.before {
        NamespaceBefore::Absent => (None, None),
        NamespaceBefore::Present(snapshot) => {
            let revision = Revision::new(snapshot.revision).map_err(|error| {
                NamespaceFailure::after_intent(
                    NamespaceFailureReason::StoreContract {
                        detail: format!("意图里的源 revision 不合法：{error}"),
                    },
                    intent,
                )
            })?;
            (
                Some(revision),
                Some(NamespaceLocation {
                    parent: snapshot.parent,
                    name: name_of(&snapshot.path).to_owned(),
                }),
            )
        }
    };
    Ok(NamespaceWrite {
        intent_id: intent.intent_id,
        vault: intent.vault,
        entry: intent.after.entry,
        kind: intent.after.kind,
        expected_revision,
        from,
        to: NamespaceLocation {
            parent: intent.after.parent,
            name: intent.after.name().to_owned(),
        },
        at,
    })
}

fn validate_request(request: &NamespaceRequest) -> Result<(), Box<NamespaceFailure>> {
    match request {
        NamespaceRequest::CreateDirectory(create) => {
            validate_name(&create.name).map_err(invalid_request)?;
            validate_path(&create.target_path).map_err(invalid_request)?;
            if name_of(&create.target_path) != create.name {
                return Err(invalid_request(format!(
                    "目标路径 {:?} 的最后一段不是请求名称 {:?}",
                    create.target_path, create.name
                )));
            }
            Ok(())
        }
        NamespaceRequest::Move(mv) => {
            validate_endpoint(&mv.from)?;
            validate_endpoint(&mv.to)?;
            if mv.from.path == mv.to.path {
                return Err(invalid_request(format!(
                    "源与目标是同一路径 {:?}：不构成移动",
                    mv.from.path
                )));
            }
            if mv.to.parent == mv.entry {
                return Err(invalid_request(format!(
                    "目标父目录 {} 就是被移动的条目：会形成目录环",
                    mv.entry
                )));
            }
            // 目录环：目标落在自己的子树里。文件没有子孙，但“路径在自己的路径之下”同样不成立。
            if is_descendant(&mv.to.path, &mv.from.path) {
                return Err(invalid_request(format!(
                    "目标路径 {:?} 在源路径 {:?} 之下：会形成目录环",
                    mv.to.path, mv.from.path
                )));
            }
            Ok(())
        }
    }
}

fn validate_endpoint(endpoint: &NamespaceEndpoint) -> Result<(), Box<NamespaceFailure>> {
    validate_name(&endpoint.name).map_err(invalid_request)?;
    validate_path(&endpoint.path).map_err(invalid_request)?;
    if name_of(&endpoint.path) != endpoint.name {
        return Err(invalid_request(format!(
            "端点路径 {:?} 的最后一段不是名称 {:?}",
            endpoint.path, endpoint.name
        )));
    }
    Ok(())
}

fn check_move_baseline(
    baseline: &EntryBaseline,
    request: &MoveRequest,
) -> Result<(), Box<NamespaceFailure>> {
    if baseline.entry != request.entry || baseline.vault != request.vault {
        return Err(base_mismatch(format!(
            "库里的条目 {}（Vault {}）不是请求条目 {}",
            baseline.entry, baseline.vault, request.entry
        )));
    }
    if baseline.revision != request.expected_revision {
        return Err(base_mismatch(format!(
            "条目 revision 已是 {}，请求基线是 {}",
            baseline.revision, request.expected_revision
        )));
    }
    if baseline.parent != request.from.parent || baseline.name != request.from.name {
        return Err(failure(NamespaceFailureReason::SubtreeChanged {
            detail: format!(
                "条目现在挂在 {}/{:?}，请求基线是 {}/{:?}",
                baseline.parent, baseline.name, request.from.parent, request.from.name
            ),
        }));
    }
    Ok(())
}

/// 目标父目录必须已存在且是目录：否则挂上去的条目会落在文件下或已删除的子树里。
async fn check_directory_parent<S>(
    store: &mut S,
    vault: VaultId,
    parent: EntryId,
) -> Result<(), Box<NamespaceFailure>>
where
    S: NamespaceCommitStore + ?Sized,
{
    match store.entry_baseline(vault, parent).await {
        Ok(baseline) if baseline.kind == EntryKind::Directory => Ok(()),
        Ok(baseline) => Err(failure(NamespaceFailureReason::SubtreeChanged {
            detail: format!(
                "目标父目录 {parent} 是{}，不是目录",
                baseline.kind.as_text()
            ),
        })),
        Err(NamespaceStoreError::EntryMissing { entry }) => {
            Err(failure(NamespaceFailureReason::TargetAncestorMissing {
                path: format!("条目 {entry}"),
            }))
        }
        Err(NamespaceStoreError::UnknownVault { vault }) => {
            Err(base_mismatch(format!("Vault {vault} 已不存在")))
        }
        Err(error) => Err(store_failure(error)),
    }
}

fn inspect<N>(ns: &mut N, path: &str) -> Result<PathState, Box<NamespaceFailure>>
where
    N: NamespacePort + ?Sized,
{
    ns.inspect(path)
        .map_err(|error| failure(NamespaceFailureReason::TargetUnreadable(error)))
}

/// 目标位置必须空着：已有条目与祖先缺失都不覆盖，只给重新规划类拒绝。
fn inspect_vacant<N>(ns: &mut N, path: &str) -> Result<(), Box<NamespaceFailure>>
where
    N: NamespacePort + ?Sized,
{
    match inspect(ns, path)? {
        PathState::Absent => Ok(()),
        PathState::AncestorMissing => Err(failure(NamespaceFailureReason::TargetAncestorMissing {
            path: path.to_owned(),
        })),
        observed => Err(failure(NamespaceFailureReason::TargetOccupied {
            path: path.to_owned(),
            observed,
        })),
    }
}

fn kind_matches(kind: EntryKind, state: &PathState) -> bool {
    matches!(
        (kind, state),
        (EntryKind::File, PathState::File { .. })
            | (EntryKind::Directory, PathState::Directory { .. })
    )
}

fn failure(reason: NamespaceFailureReason) -> Box<NamespaceFailure> {
    Box::new(NamespaceFailure::new(reason))
}

fn store_failure(error: NamespaceStoreError) -> Box<NamespaceFailure> {
    failure(NamespaceFailureReason::Store(error))
}

fn base_mismatch(detail: impl Into<String>) -> Box<NamespaceFailure> {
    failure(NamespaceFailureReason::BaseMismatch {
        detail: detail.into(),
    })
}

fn invalid_request(detail: impl Into<String>) -> Box<NamespaceFailure> {
    failure(NamespaceFailureReason::InvalidRequest {
        detail: detail.into(),
    })
}

/// 单个名称：非空、不含路径分隔符/NUL、不是 `.`/`..`。
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("名称不能为空".to_owned());
    }
    if name == "." || name == ".." {
        return Err(format!("名称 {name:?} 不是有效文件名称"));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!("名称 {name:?} 含路径分隔符或 NUL"));
    }
    Ok(())
}

/// Vault 内相对路径：非空、不以 `/` 起始、无空分量与 `.`/`..` 分量。
pub(crate) fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("路径不能为空".to_owned());
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(format!("路径 {path:?} 必须是 Vault 内的相对路径"));
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("路径 {path:?} 含空、`.` 或 `..` 分量"));
        }
    }
    Ok(())
}

pub(crate) fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(crate) fn is_descendant(path: &str, ancestor: &str) -> bool {
    path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.as_bytes().get(ancestor.len()) == Some(&b'/')
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

pub(crate) fn take_path(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<String, StateJsonError> {
    let text = take_text(fields, field)?;
    let bytes =
        hex_decode(&text).map_err(|detail| StateJsonError::InvalidValue { field, detail })?;
    String::from_utf8(bytes).map_err(|_| StateJsonError::InvalidValue {
        field,
        detail: "十六进制路径不是合法 UTF-8".to_owned(),
    })
}

pub(crate) fn take_kind(
    fields: &mut BTreeMap<String, JsonValue>,
    field: &'static str,
) -> Result<EntryKind, StateJsonError> {
    let text = take_text(fields, field)?;
    EntryKind::from_text(&text).ok_or_else(|| StateJsonError::InvalidValue {
        field,
        detail: format!("{text:?} 不是条目种类"),
    })
}

pub(crate) fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("十六进制文本长度必须是偶数".to_owned());
    }
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

fn hex_digit(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(format!("{byte:?} 不是小写十六进制数字")),
    }
}

/// 恢复时对两个位置的观察结果。
#[derive(Clone, Debug)]
pub struct ObservedNamespace {
    /// 目标位置的观察；错误必须原样传进来，不能先变成“不存在”。
    pub target: Result<PathState, NamespaceError>,
    /// 源位置的观察（新建时无关，可传 `Ok(PathState::Absent)`）。
    pub source: Result<PathState, NamespaceError>,
}

/// 现场是第三种状态时，需要协调的具体情形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceReconcileReason {
    /// 目标位置已经有别的条目（不是我们移过去的那个 inode）：不覆盖，重新规划另名。
    TargetOccupied,
    /// 目标祖先目录不存在：不重建，重新规划。
    TargetUnavailable,
    /// 阶段是 `published`，目标位置却没有这次变化：现场与数据库阶段矛盾。
    TargetReverted,
    /// 移动的源与目标都不见了：既没证明完成，也没证明没做，不能猜。
    MoveLost,
}

impl NamespaceReconcileReason {
    /// 稳定短名。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            NamespaceReconcileReason::TargetOccupied => "target_occupied",
            NamespaceReconcileReason::TargetUnavailable => "target_unavailable",
            NamespaceReconcileReason::TargetReverted => "target_reverted",
            NamespaceReconcileReason::MoveLost => "move_lost",
        }
    }
}

impl fmt::Display for NamespaceReconcileReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条未完成命名空间意图的恢复结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceRecoveryDecision {
    /// 意图已 `finalized`：不需要动作。
    AlreadyFinished,
    /// 现场满足前提：按同一意图重做命名空间动作（并补完事务）。
    Redo,
    /// 现场已经是意图目标：只补完元数据事务并收尾。
    Complete,
    /// 现场无法用意图解释但可以保留：等协调，不覆盖、不提交。
    Reconcile {
        /// 具体情形。
        reason: NamespaceReconcileReason,
    },
    /// 没有可证明的现场：落盘 `needs_recovery` 并暂停该 Vault 的破坏性写入。
    NeedsRecovery {
        /// 具体原因。
        reason: NeedsRecoveryReason,
    },
}

impl NamespaceRecoveryDecision {
    /// 稳定短名（恢复报告与日志用）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NamespaceRecoveryDecision::AlreadyFinished => "already_finished",
            NamespaceRecoveryDecision::Redo => "redo",
            NamespaceRecoveryDecision::Complete => "complete",
            NamespaceRecoveryDecision::Reconcile { reason } => reason.as_str(),
            NamespaceRecoveryDecision::NeedsRecovery { reason } => reason.as_str(),
        }
    }

    /// 是否必须暂停该 Vault 的破坏性写入（只有 `NeedsRecovery`）。
    #[must_use]
    pub fn pauses_writes(self) -> bool {
        matches!(self, NamespaceRecoveryDecision::NeedsRecovery { .. })
    }
}

impl fmt::Display for NamespaceRecoveryDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 按持久阶段与两个位置的现场判定一条命名空间意图该怎么处理。
///
/// 判定只读意图证据与实测现场，不写库、不动磁盘。移动的“完成”必须能证明目标上的条目就是被移动的那
/// 一个（同 inode）且源位置已空；证不出来时既不重做也不收尾，只给协调结论。
#[must_use]
pub fn classify_namespace(
    intent: &NamespaceIntent,
    observed: &ObservedNamespace,
) -> NamespaceRecoveryDecision {
    if intent.phase == PutPhase::Finalized {
        return NamespaceRecoveryDecision::AlreadyFinished;
    }
    if intent.phase == PutPhase::NeedsRecovery {
        return NamespaceRecoveryDecision::NeedsRecovery {
            reason: NeedsRecoveryReason::MarkedNeedsRecovery,
        };
    }
    let target = match &observed.target {
        Ok(state) => state,
        Err(_) => {
            return NamespaceRecoveryDecision::NeedsRecovery {
                reason: NeedsRecoveryReason::TargetUnreadable,
            };
        }
    };
    let source = match &observed.source {
        Ok(state) => state,
        Err(_) => {
            return NamespaceRecoveryDecision::NeedsRecovery {
                reason: NeedsRecoveryReason::TargetUnreadable,
            };
        }
    };
    match intent.op {
        NamespaceOpKind::Create => classify_create(intent, target),
        NamespaceOpKind::Move => classify_move(intent, target, source),
    }
}

fn classify_create(intent: &NamespaceIntent, target: &PathState) -> NamespaceRecoveryDecision {
    match intent.phase {
        PutPhase::Prepared => match target {
            PathState::Absent => NamespaceRecoveryDecision::Redo,
            PathState::Directory { .. } => NamespaceRecoveryDecision::Complete,
            PathState::File { .. } => NamespaceRecoveryDecision::Reconcile {
                reason: NamespaceReconcileReason::TargetOccupied,
            },
            PathState::AncestorMissing => NamespaceRecoveryDecision::Reconcile {
                reason: NamespaceReconcileReason::TargetUnavailable,
            },
        },
        _ => match target {
            PathState::Directory { .. } => NamespaceRecoveryDecision::Complete,
            PathState::Absent => NamespaceRecoveryDecision::Reconcile {
                reason: NamespaceReconcileReason::TargetReverted,
            },
            PathState::File { .. } => NamespaceRecoveryDecision::Reconcile {
                reason: NamespaceReconcileReason::TargetOccupied,
            },
            PathState::AncestorMissing => NamespaceRecoveryDecision::Reconcile {
                reason: NamespaceReconcileReason::TargetUnavailable,
            },
        },
    }
}

fn classify_move(
    intent: &NamespaceIntent,
    target: &PathState,
    source: &PathState,
) -> NamespaceRecoveryDecision {
    let moved = moved_into_place(intent, target);
    match intent.phase {
        PutPhase::Prepared => {
            if moved && matches!(source, PathState::Absent) {
                return NamespaceRecoveryDecision::Complete;
            }
            if matches!(target, PathState::Absent) && !matches!(source, PathState::Absent) {
                return NamespaceRecoveryDecision::Redo;
            }
            if matches!(target, PathState::Absent) && matches!(source, PathState::Absent) {
                return NamespaceRecoveryDecision::Reconcile {
                    reason: NamespaceReconcileReason::MoveLost,
                };
            }
            if matches!(target, PathState::AncestorMissing) {
                return NamespaceRecoveryDecision::Reconcile {
                    reason: NamespaceReconcileReason::TargetUnavailable,
                };
            }
            NamespaceRecoveryDecision::Reconcile {
                reason: NamespaceReconcileReason::TargetOccupied,
            }
        }
        _ => {
            if moved && matches!(source, PathState::Absent) {
                return NamespaceRecoveryDecision::Complete;
            }
            if matches!(source, PathState::Absent) && matches!(target, PathState::Absent) {
                return NamespaceRecoveryDecision::Reconcile {
                    reason: NamespaceReconcileReason::MoveLost,
                };
            }
            if !matches!(target, PathState::AncestorMissing) && matches!(source, PathState::Absent)
            {
                return NamespaceRecoveryDecision::Reconcile {
                    reason: NamespaceReconcileReason::TargetOccupied,
                };
            }
            NamespaceRecoveryDecision::Reconcile {
                reason: NamespaceReconcileReason::TargetReverted,
            }
        }
    }
}

/// 目标位置上的条目就是被移动的那一个：种类相符且同 inode（`rename` 保 inode）。
fn moved_into_place(intent: &NamespaceIntent, target: &PathState) -> bool {
    let Some(snapshot) = intent.before.present() else {
        return false;
    };
    kind_matches(intent.after.kind, target) && target.same_inode_as(&snapshot.identity)
}

/// 一条意图的恢复结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceRecoveryOutcome {
    /// 现场满足前提：重做了命名空间动作并补完事务。
    Redone {
        /// 条目身份（未变）。
        entry: EntryId,
        /// 条目新 revision。
        revision: Revision,
        /// 动作后目标位置的实测现场。
        target: PathState,
    },
    /// 现场已是意图目标：只补完元数据事务。
    Completed {
        /// 条目身份。
        entry: EntryId,
        /// 条目新 revision。
        revision: Revision,
    },
    /// 现场无法解释但可以保留：等协调，不覆盖、不提交。
    Preserved {
        /// 具体情形。
        reason: NamespaceReconcileReason,
    },
    /// 没有可证明的现场：意图已落 `needs_recovery`，该 Vault 暂停破坏性写入。
    NeedsRecovery {
        /// 具体原因。
        reason: NeedsRecoveryReason,
    },
    /// 恢复动作本身失败：没有完成也没有假装完成，Vault 保持关闭，下次启动重试。
    Unresolved {
        /// 稳定错误码。
        code: &'static str,
        /// 原因文本。
        detail: String,
    },
}

impl NamespaceRecoveryOutcome {
    /// 稳定短名。
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            NamespaceRecoveryOutcome::Redone { .. } => "redone",
            NamespaceRecoveryOutcome::Completed { .. } => "completed",
            NamespaceRecoveryOutcome::Preserved { reason } => reason.as_str(),
            NamespaceRecoveryOutcome::NeedsRecovery { reason } => reason.as_str(),
            NamespaceRecoveryOutcome::Unresolved { code, .. } => code,
        }
    }

    /// 是否产生了已提交的命名空间变化。
    #[must_use]
    pub fn is_committed(&self) -> bool {
        matches!(
            self,
            NamespaceRecoveryOutcome::Redone { .. } | NamespaceRecoveryOutcome::Completed { .. }
        )
    }

    /// 是否让该 Vault 保持关闭。
    #[must_use]
    pub fn pauses_writes(&self) -> bool {
        matches!(
            self,
            NamespaceRecoveryOutcome::NeedsRecovery { .. }
                | NamespaceRecoveryOutcome::Unresolved { .. }
        )
    }
}

impl fmt::Display for NamespaceRecoveryOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceRecoveryOutcome::Redone {
                entry, revision, ..
            } => {
                write!(f, "已重做命名空间动作，条目 {entry} 到 revision {revision}")
            }
            NamespaceRecoveryOutcome::Completed { entry, revision } => {
                write!(f, "已补完事务，条目 {entry} 到 revision {revision}")
            }
            NamespaceRecoveryOutcome::Preserved { reason } => {
                write!(f, "保留现场等协调（{reason}）")
            }
            NamespaceRecoveryOutcome::NeedsRecovery { reason } => {
                write!(f, "暂停该 Vault 的破坏性写入（{reason}）")
            }
            NamespaceRecoveryOutcome::Unresolved { code, detail } => {
                write!(f, "恢复未完成（{code}）：{detail}")
            }
        }
    }
}

/// 一条被处理过的未完成命名空间意图。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredNamespaceIntent {
    /// 恢复前的意图记录。
    pub intent: NamespaceIntent,
    /// 按恢复前证据得到的判定。
    pub decision: NamespaceRecoveryDecision,
    /// 实际执行的恢复结果。
    pub outcome: NamespaceRecoveryOutcome,
}

/// 一个 Vault 的命名空间恢复报告。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceRecoveryReport {
    /// 被恢复的 Vault。
    pub vault: VaultId,
    /// 每条未完成意图的处理结果（保持意图写入顺序）。
    pub intents: Vec<RecoveredNamespaceIntent>,
}

impl NamespaceRecoveryReport {
    /// 是否有任何一条意图要求暂停该 Vault 的破坏性写入。
    #[must_use]
    pub fn pauses_writes(&self) -> bool {
        self.intents
            .iter()
            .any(|entry| entry.outcome.pauses_writes())
    }

    /// 是否有需要协调（保留双方）的意图。
    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.intents
            .iter()
            .any(|entry| matches!(entry.outcome, NamespaceRecoveryOutcome::Preserved { .. }))
    }

    /// 本次恢复产生提交的意图数。
    #[must_use]
    pub fn committed(&self) -> usize {
        self.intents
            .iter()
            .filter(|entry| entry.outcome.is_committed())
            .count()
    }
}

/// 命名空间恢复的失败。
#[derive(Clone, Debug)]
pub enum NamespaceRecoveryError {
    /// 读不出未完成意图（越权、存储失败）。
    Intent(IntentLogError),
}

impl NamespaceRecoveryError {
    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            NamespaceRecoveryError::Intent(error) => error.code(),
        }
    }
}

impl fmt::Display for NamespaceRecoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamespaceRecoveryError::Intent(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for NamespaceRecoveryError {}

/// 按未完成命名空间意图恢复一个 Vault。
///
/// 与 [`crate::recovery::startup::recover_vault`] 同一套顺序与不变量：只重放能证明的动作（现场满足前提
/// 才重做，现场已是目标只补事务），现场无法解释就保留双方，读不到就落盘 `needs_recovery` 停写，动作
/// 本身失败就保持关闭等下次启动。装配时必须在本函数与 put 的恢复**都**完成后再开放写入口
/// （[`crate::recovery::startup::WriteAdmission`]）。
///
/// # Errors
///
/// 见 [`NamespaceRecoveryError`]：读不出未完成意图。单条意图的失败作为
/// [`NamespaceRecoveryOutcome::Unresolved`] 留在报告里。
pub async fn recover_namespace_vault<L, N, S>(
    log: &mut L,
    ns: &mut N,
    store: &mut S,
    at: UtcTimestamp,
    vault: VaultId,
) -> Result<NamespaceRecoveryReport, NamespaceRecoveryError>
where
    L: NamespaceIntentLog + ?Sized,
    N: NamespacePort + ?Sized,
    S: NamespaceCommitStore + ?Sized,
{
    let unfinished = log
        .unfinished_namespaces(vault)
        .await
        .map_err(NamespaceRecoveryError::Intent)?;
    let mut intents = Vec::with_capacity(unfinished.len());
    for intent in unfinished {
        if !intent.phase.is_open() {
            continue;
        }
        let observed = ObservedNamespace {
            target: ns.inspect(intent.target_path()),
            source: match intent.before.present() {
                Some(snapshot) => ns.inspect(&snapshot.path),
                None => Ok(PathState::Absent),
            },
        };
        let decision = classify_namespace(&intent, &observed);
        let outcome = apply_decision(log, ns, store, at, &intent, decision).await;
        intents.push(RecoveredNamespaceIntent {
            intent,
            decision,
            outcome,
        });
    }
    Ok(NamespaceRecoveryReport { vault, intents })
}

async fn apply_decision<L, N, S>(
    log: &mut L,
    ns: &mut N,
    store: &mut S,
    at: UtcTimestamp,
    intent: &NamespaceIntent,
    decision: NamespaceRecoveryDecision,
) -> NamespaceRecoveryOutcome
where
    L: NamespaceIntentLog + ?Sized,
    N: NamespacePort + ?Sized,
    S: NamespaceCommitStore + ?Sized,
{
    match decision {
        NamespaceRecoveryDecision::Redo => {
            let from = intent
                .before
                .present()
                .map(|snapshot| snapshot.path.clone());
            let target = match apply_change(ns, intent, from.as_deref()) {
                Ok(state) => state,
                Err(reason) => {
                    return unresolved(reason.code(), reason.to_string());
                }
            };
            match advance_if_prepared(log, intent, at).await {
                Ok(published) => match finish_metadata(store, at, &published).await {
                    NamespaceRecoveryOutcome::Completed { entry, revision } => {
                        NamespaceRecoveryOutcome::Redone {
                            entry,
                            revision,
                            target,
                        }
                    }
                    other => other,
                },
                Err(outcome) => outcome,
            }
        }
        NamespaceRecoveryDecision::Complete => match advance_if_prepared(log, intent, at).await {
            Ok(published) => finish_metadata(store, at, &published).await,
            Err(outcome) => outcome,
        },
        NamespaceRecoveryDecision::Reconcile { reason } => {
            NamespaceRecoveryOutcome::Preserved { reason }
        }
        NamespaceRecoveryDecision::NeedsRecovery { reason } => {
            if intent.phase == PutPhase::NeedsRecovery {
                NamespaceRecoveryOutcome::NeedsRecovery { reason }
            } else {
                match log
                    .advance_namespace(intent.intent_id, PutPhase::NeedsRecovery, at)
                    .await
                {
                    Ok(_) => NamespaceRecoveryOutcome::NeedsRecovery { reason },
                    Err(error) => unresolved(error.code(), error.to_string()),
                }
            }
        }
        NamespaceRecoveryDecision::AlreadyFinished => unresolved(
            "intent_already_finished",
            "未完成清单里出现已收尾意图".to_owned(),
        ),
    }
}

/// 阶段还停在 `prepared` 时先按现场证据补记 `published`（崩溃可能停在“动作完成、阶段没记下来”）。
async fn advance_if_prepared<L>(
    log: &mut L,
    intent: &NamespaceIntent,
    at: UtcTimestamp,
) -> Result<NamespaceIntent, NamespaceRecoveryOutcome>
where
    L: NamespaceIntentLog + ?Sized,
{
    if intent.phase != PutPhase::Prepared {
        return Ok(intent.clone());
    }
    log.advance_namespace(intent.intent_id, PutPhase::Published, at)
        .await
        .map_err(|error| unresolved(error.code(), error.to_string()))
}

/// 补完崩溃前没提交的元数据事务（entries/changes/意图收尾，同一事务）。
async fn finish_metadata<S>(
    store: &mut S,
    at: UtcTimestamp,
    intent: &NamespaceIntent,
) -> NamespaceRecoveryOutcome
where
    S: NamespaceCommitStore + ?Sized,
{
    let write = match namespace_write(intent, at) {
        Ok(write) => write,
        Err(failure) => return unresolved(failure.code(), failure.to_string()),
    };
    match store.apply_namespace(&write).await {
        Ok(record) => match check_record(&record, intent) {
            Ok(()) => NamespaceRecoveryOutcome::Completed {
                entry: record.entry,
                revision: record.revision,
            },
            Err(failure) => unresolved(failure.code(), failure.to_string()),
        },
        Err(error) => unresolved(error.code(), error.to_string()),
    }
}

fn unresolved(code: &'static str, detail: String) -> NamespaceRecoveryOutcome {
    NamespaceRecoveryOutcome::Unresolved { code, detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ports::namespace_store::NamespaceLocation;
    use crate::test_support::{
        FakeIntentLog, FakeNamespace, FakeNamespaceStore, Trace, entry_id, epoch, intent_id,
        revision, user_time, vault_id, version_id,
    };

    fn vault() -> VaultId {
        vault_id(0x21)
    }

    /// 移动场景的源条目：目录 `docs`，父目录 0x61，revision 3。
    fn docs() -> EntryBaseline {
        EntryBaseline {
            entry: entry_id(0x62),
            vault: vault(),
            parent: entry_id(0x61),
            name: "docs".to_owned(),
            revision: revision(3),
            kind: EntryKind::Directory,
        }
    }

    /// 目标父目录 `archive`。
    fn archive() -> EntryBaseline {
        EntryBaseline {
            entry: entry_id(0x63),
            vault: vault(),
            parent: entry_id(0x60),
            name: "archive".to_owned(),
            revision: revision(1),
            kind: EntryKind::Directory,
        }
    }

    fn move_request() -> MoveRequest {
        MoveRequest {
            intent_id: intent_id(0x41),
            vault: vault(),
            entry: entry_id(0x62),
            expected_epoch: epoch(3),
            expected_revision: revision(3),
            from: NamespaceEndpoint {
                parent: entry_id(0x61),
                name: "docs".to_owned(),
                path: "docs".to_owned(),
            },
            to: NamespaceEndpoint {
                parent: entry_id(0x63),
                name: "docs".to_owned(),
                path: "archive/docs".to_owned(),
            },
        }
    }

    fn create_request() -> CreateDirectoryRequest {
        CreateDirectoryRequest {
            intent_id: intent_id(0x42),
            vault: vault(),
            entry: entry_id(0x66),
            expected_epoch: epoch(3),
            parent: entry_id(0x61),
            name: "photos".to_owned(),
            target_path: "photos".to_owned(),
        }
    }

    type MoveFixture = (
        VaultCommitGate,
        FakeNamespaceStore,
        FakeIntentLog,
        FakeNamespace,
    );

    /// 共享一条轨迹的移动场景：库里 `docs` 与 `archive`，工作目录里也有这两个目录。
    fn move_fixture() -> MoveFixture {
        let store = FakeNamespaceStore::new()
            .with_baseline(docs())
            .with_baseline(archive());
        let trace: Trace = store.trace.clone();
        let log = FakeIntentLog {
            trace: trace.clone(),
            ..FakeIntentLog::default()
        };
        let ns = FakeNamespace::new()
            .with_trace(trace)
            .with_directory("docs")
            .with_directory("archive");
        (VaultCommitGate::new(), store, log, ns)
    }

    /// 共享一条轨迹的“新建目录”场景：库里只有父目录 0x61。
    fn create_fixture() -> MoveFixture {
        let store = FakeNamespaceStore::new().with_baseline(EntryBaseline {
            entry: entry_id(0x61),
            vault: vault(),
            parent: entry_id(0x60),
            name: "notes".to_owned(),
            revision: revision(2),
            kind: EntryKind::Directory,
        });
        let trace: Trace = store.trace.clone();
        let log = FakeIntentLog {
            trace: trace.clone(),
            ..FakeIntentLog::default()
        };
        let ns = FakeNamespace::new()
            .with_trace(trace)
            .with_directory("notes");
        (VaultCommitGate::new(), store, log, ns)
    }

    async fn run(
        gate: &mut VaultCommitGate,
        store: &mut FakeNamespaceStore,
        log: &mut FakeIntentLog,
        ns: &mut FakeNamespace,
        request: &NamespaceRequest,
    ) -> Result<CommittedNamespace, Box<NamespaceFailure>> {
        commit_namespace(gate, store, log, ns, user_time(100), request).await
    }

    #[tokio::test]
    async fn directory_move_changes_only_the_parent_relation() {
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        let request = NamespaceRequest::Move(move_request());
        let committed = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect("目录移动成功");

        assert_eq!(
            store.trace.entries(),
            vec![
                format!("store vault_facts {}", vault()),
                format!("store entry_baseline {} {}", vault(), entry_id(0x62)),
                format!("store entry_baseline {} {}", vault(), entry_id(0x63)),
                "namespace inspect docs".to_owned(),
                "namespace inspect archive/docs".to_owned(),
                "intent begin namespace prepared".to_owned(),
                "namespace rename docs archive/docs".to_owned(),
                "namespace inspect docs".to_owned(),
                "intent advance published".to_owned(),
                "store apply_namespace".to_owned(),
            ],
            "顺序固定：epoch → 条目基线 → 父目录 → 源/目标现场 → 意图 → 改名 → 事务"
        );

        // 身份不变、revision 推进、父关系换到 archive 下。
        assert_eq!(committed.record.entry, entry_id(0x62));
        assert_eq!(committed.intent_id(), intent_id(0x41));
        assert_eq!(committed.record.revision, revision(4));
        assert_eq!(committed.record.phase, PutPhase::Finalized);
        let write = &store.writes[0];
        assert_eq!(write.entry, entry_id(0x62));
        assert_eq!(write.kind, EntryKind::Directory);
        assert_eq!(write.expected_revision, Some(revision(3)));
        assert_eq!(
            write.from,
            Some(NamespaceLocation {
                parent: entry_id(0x61),
                name: "docs".to_owned()
            })
        );
        assert_eq!(
            write.to,
            NamespaceLocation {
                parent: entry_id(0x63),
                name: "docs".to_owned()
            }
        );
        // 工作目录里确实是“改名”而不是复制：源空了、目标在、inode 是同一个。
        assert_eq!(ns.state_of("docs"), PathState::Absent);
        assert_eq!(committed.target, ns.state_of("archive/docs"));
        assert!(committed.target.is_directory());
        // 内容流量为零：轨迹里没有发布、也没有对象操作。
        let trace = store.trace.entries();
        assert!(
            !trace
                .iter()
                .any(|line| line.contains("target publish") || line.contains("object")),
            "移动不得触碰内容对象：{trace:?}"
        );
        assert!(!gate.is_held(vault()), "退出前必须释放 Vault 票据");
    }

    #[tokio::test]
    async fn moving_a_file_keeps_the_entry_and_its_content_reference() {
        let note = EntryBaseline {
            entry: entry_id(0x52),
            vault: vault(),
            parent: entry_id(0x61),
            name: "note.txt".to_owned(),
            revision: revision(7),
            kind: EntryKind::File,
        };
        let mut store = FakeNamespaceStore::new()
            .with_baseline(note)
            .with_baseline(archive());
        let content_before = store.content_versions.get(&entry_id(0x52)).copied();
        assert_eq!(content_before, Some(version_id(0x91)));
        let mut gate = VaultCommitGate::new();
        let mut log = FakeIntentLog::default();
        let mut ns = FakeNamespace::new()
            .with_file("note.txt")
            .with_directory("archive");
        let request = NamespaceRequest::Move(MoveRequest {
            intent_id: intent_id(0x43),
            vault: vault(),
            entry: entry_id(0x52),
            expected_epoch: epoch(3),
            expected_revision: revision(7),
            from: NamespaceEndpoint {
                parent: entry_id(0x61),
                name: "note.txt".to_owned(),
                path: "note.txt".to_owned(),
            },
            to: NamespaceEndpoint {
                parent: entry_id(0x63),
                name: "note.txt".to_owned(),
                path: "archive/note.txt".to_owned(),
            },
        });
        let committed = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect("文件移动成功");
        assert_eq!(committed.record.entry, entry_id(0x52));
        assert_eq!(committed.record.revision, revision(8));
        assert_eq!(
            store.content_versions.get(&entry_id(0x52)).copied(),
            content_before,
            "移动只改父关系：内容版本引用必须原样保留"
        );
        assert!(store.writes[0].expected_revision == Some(revision(7)));
        assert_eq!(ns.state_of("archive/note.txt"), committed.target);
    }

    #[tokio::test]
    async fn create_directory_persists_a_new_directory_entry() {
        let (mut gate, mut store, mut log, mut ns) = create_fixture();
        let request = NamespaceRequest::CreateDirectory(create_request());
        let committed = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect("新建目录成功");
        assert_eq!(
            store.trace.entries(),
            vec![
                format!("store vault_facts {}", vault()),
                format!("store entry_baseline {} {}", vault(), entry_id(0x66)),
                format!("store entry_baseline {} {}", vault(), entry_id(0x61)),
                "namespace inspect photos".to_owned(),
                "intent begin namespace prepared".to_owned(),
                "namespace mkdir photos".to_owned(),
                "intent advance published".to_owned(),
                "store apply_namespace".to_owned(),
            ],
            "新建与移动共用同一条顺序：先核对基线与目标，再写意图，最后动磁盘"
        );
        let write = &store.writes[0];
        assert_eq!(write.entry, entry_id(0x66));
        assert_eq!(write.kind, EntryKind::Directory);
        assert_eq!(write.expected_revision, None, "新建没有基线 revision");
        assert_eq!(write.from, None, "新建没有源位置");
        assert_eq!(
            committed.record.revision,
            revision(0),
            "新条目从 revision 0 开始"
        );
        assert!(ns.state_of("photos").is_directory());
    }

    #[tokio::test]
    async fn create_refuses_an_identity_that_already_exists() {
        let (mut gate, mut store, mut log, mut ns) = create_fixture();
        // 库里已经有这个身份（另一个条目）：不能把别人的条目“新建”成自己的。
        store.baselines.insert(
            entry_id(0x66),
            EntryBaseline {
                entry: entry_id(0x66),
                vault: vault(),
                parent: entry_id(0x61),
                name: "old".to_owned(),
                revision: revision(5),
                kind: EntryKind::Directory,
            },
        );
        let request = NamespaceRequest::CreateDirectory(create_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("身份已占用");
        assert_eq!(failure.code(), "entry_already_exists");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
        assert!(!failure.is_committed());
        assert_eq!(failure.intent_id, None, "基线不符不得写意图");
        assert!(log.namespace_intents.is_empty());
        assert!(!ns.state_of("photos").is_directory());
    }

    #[tokio::test]
    async fn create_never_overwrites_an_existing_target_or_its_missing_parent() {
        // 目标位置已有文件：不覆盖，重新规划。
        let (mut gate, mut store, mut log, mut ns) = create_fixture();
        ns = ns.with_file("photos");
        let request = NamespaceRequest::CreateDirectory(create_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("目标已占用");
        assert_eq!(failure.code(), "target_occupied");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert!(log.namespace_intents.is_empty());
        assert!(
            log.trace
                .entries()
                .iter()
                .all(|line| !line.contains("mkdir"))
        );

        // 目标祖先已删除：不重建目录树。
        let (mut gate, mut store, mut log, mut ns) = create_fixture();
        ns.ancestor_missing.push("photos".to_owned());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("祖先缺失");
        assert_eq!(failure.code(), "target_ancestor_missing");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert!(log.namespace_intents.is_empty());
    }

    #[tokio::test]
    async fn create_requires_an_existing_directory_parent() {
        let (mut gate, mut store, mut log, mut ns) = create_fixture();
        // 父目录基线是普通文件：挂不上去。
        store.baselines.insert(
            entry_id(0x61),
            EntryBaseline {
                entry: entry_id(0x61),
                vault: vault(),
                parent: entry_id(0x60),
                name: "notes".to_owned(),
                revision: revision(2),
                kind: EntryKind::File,
            },
        );
        let request = NamespaceRequest::CreateDirectory(create_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("父目录不是目录");
        assert_eq!(failure.code(), "subtree_changed");
        assert!(log.namespace_intents.is_empty());
        assert!(
            log.trace
                .entries()
                .iter()
                .all(|line| !line.contains("mkdir"))
        );
    }

    #[tokio::test]
    async fn stale_move_baseline_is_rejected_before_any_disk_change() {
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        let mut docs = docs();
        docs.revision = revision(4);
        store.baselines.insert(entry_id(0x62), docs);
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("过期基线");
        assert_eq!(failure.code(), "base_mismatch");
        assert_eq!(failure.error_code(), Some(ErrorCode::BaseMismatch));
        assert!(!failure.is_committed());
        assert!(log.namespace_intents.is_empty());
        assert!(ns.renames.is_empty(), "基线不符不得改名");
        assert!(ns.state_of("docs").is_directory());
    }

    #[tokio::test]
    async fn move_rejects_a_changed_source_location_or_a_non_directory_parent() {
        // 条目已经不在请求的父目录/名称下：SUBREE_CHANGED。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        let mut moved = docs();
        moved.parent = entry_id(0x64);
        store.baselines.insert(entry_id(0x62), moved);
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("源位置已变");
        assert_eq!(failure.code(), "subtree_changed");
        assert!(log.namespace_intents.is_empty());

        // 目标父目录是普通文件：不能把一个目录挂到文件下面。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        let mut archive = archive();
        archive.kind = EntryKind::File;
        store.baselines.insert(entry_id(0x63), archive);
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("父目录是文件");
        assert_eq!(failure.code(), "subtree_changed");
        assert!(ns.renames.is_empty());
    }

    #[tokio::test]
    async fn move_rejects_directory_cycles_before_touching_anything() {
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        let mut request = move_request();
        request.to.path = "docs/inner".to_owned();
        request.to.name = "inner".to_owned();
        let failure = run(
            &mut gate,
            &mut store,
            &mut log,
            &mut ns,
            &NamespaceRequest::Move(request.clone()),
        )
        .await
        .expect_err("目标在自己的子树里");
        assert_eq!(failure.code(), "invalid_request");
        assert!(failure.error_code().is_none());
        assert!(log.namespace_intents.is_empty());
        assert!(ns.renames.is_empty());

        // 目标父目录就是自己：同样是环。
        let mut self_parent = move_request();
        self_parent.to.parent = entry_id(0x62);
        let failure = run(
            &mut gate,
            &mut store,
            &mut log,
            &mut ns,
            &NamespaceRequest::Move(self_parent),
        )
        .await
        .expect_err("自己挂在自己下面");
        assert_eq!(failure.code(), "invalid_request");

        // 源与目标同一路径不构成移动。
        let mut same = move_request();
        same.to = same.from.clone();
        let failure = run(
            &mut gate,
            &mut store,
            &mut log,
            &mut ns,
            &NamespaceRequest::Move(same),
        )
        .await
        .expect_err("同路径");
        assert_eq!(failure.code(), "invalid_request");
        assert!(ns.renames.is_empty());
    }

    #[tokio::test]
    async fn move_never_overwrites_an_occupied_target() {
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        ns = ns.with_directory("archive/docs");
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("目标已占用");
        assert_eq!(failure.code(), "target_occupied");
        assert_eq!(failure.error_code(), Some(ErrorCode::SubtreeChanged));
        assert!(log.namespace_intents.is_empty());
        assert!(ns.renames.is_empty(), "目标冲突不得改名");
        assert!(ns.state_of("docs").is_directory(), "源位置必须原样保留");
    }

    #[tokio::test]
    async fn move_reports_a_changed_or_unreadable_source_without_guessing() {
        // 工作目录里源已经不在了：先采集外部变化再重新计划。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        ns.paths.remove("docs");
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("源不见了");
        assert_eq!(failure.code(), "source_changed");
        assert_eq!(failure.error_code(), Some(ErrorCode::ReconcileRequired));
        assert!(log.namespace_intents.is_empty());

        // 源种类与基线不符（目录变成文件）：同样不猜。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        ns.paths.remove("docs");
        ns = ns.with_file("docs");
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("种类不符");
        assert_eq!(failure.code(), "source_changed");

        // 读不到不是“不存在”。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        ns.inspect_error = Some(NamespaceError::Unreadable {
            detail: "EACCES".to_owned(),
        });
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("不可读");
        assert_eq!(failure.code(), "target_unreadable");
        assert!(log.namespace_intents.is_empty());
    }

    #[tokio::test]
    async fn a_failed_rename_leaves_a_prepared_intent_and_reports_it() {
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        ns.rename_error = Some(NamespaceError::Storage {
            stage: "rename",
            failure: crate::ports::object_store::StorageFailure::NoSpace,
            detail: "No space left on device".to_owned(),
        });
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("满盘");
        assert_eq!(failure.code(), "no_space");
        assert!(!failure.is_committed());
        assert_eq!(failure.intent_id, Some(intent_id(0x41)));
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        assert_eq!(log.namespace_intents[0].phase, PutPhase::Prepared);
        assert!(store.writes.is_empty(), "动作失败不得写元数据事务");
        assert!(ns.state_of("docs").is_directory());
    }

    #[tokio::test]
    async fn a_misreported_rename_does_not_advance_the_phase() {
        // 端口报告“成功”，但目标上的 inode 不是源那一个：这不是移动。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        ns.rename_reports = Some(PathState::Directory {
            identity: FileKey {
                device: 7,
                inode: 999,
                links: 2,
                bytes: 64,
            },
        });
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("inode 不符");
        assert_eq!(failure.code(), "applied_state_mismatch");
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
        assert!(!failure.is_committed());
        assert!(store.writes.is_empty());

        // 改名后源位置还在（被实现成复制）：同样不推进阶段。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        ns.rename_keeps_source = true;
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("源仍在");
        assert_eq!(failure.code(), "applied_state_mismatch");
        assert_eq!(failure.phase, Some(PutPhase::Prepared));
    }

    #[tokio::test]
    async fn store_rejections_are_not_reported_as_committed() {
        // 事务内发现 revision 已变：过期基线，不是提交成功。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        store.apply_error = Some(NamespaceStoreError::RevisionChanged {
            expected: revision(3),
            actual: revision(4),
        });
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("revision 已变");
        assert_eq!(failure.code(), "base_mismatch");
        assert!(!failure.is_committed());
        assert_eq!(failure.phase, Some(PutPhase::Published));

        // 适配器声称成功但阶段不是 finalized：停写人工核对。
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        store.record = Some(NamespaceRecord {
            intent_id: intent_id(0x41),
            phase: PutPhase::Published,
            entry: entry_id(0x62),
            revision: revision(4),
            seq: crate::test_support::vault_seq(11),
            tx: crate::test_support::tx_id(0x71),
        });
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("证据不符");
        assert_eq!(failure.code(), "store_contract_violation");
        assert_eq!(failure.error_code(), Some(ErrorCode::NeedsRecovery));
        assert!(!failure.is_committed());
    }

    #[tokio::test]
    async fn a_busy_vault_serializes_namespace_commits() {
        let (mut gate, mut store, mut log, mut ns) = move_fixture();
        let ticket = gate.acquire(vault()).expect("先占用");
        let request = NamespaceRequest::Move(move_request());
        let failure = run(&mut gate, &mut store, &mut log, &mut ns, &request)
            .await
            .expect_err("同一 Vault 已有提交");
        assert_eq!(failure.code(), "vault_commit_busy");
        assert!(log.namespace_intents.is_empty());
        gate.release(ticket);
    }

    fn move_intent(phase: PutPhase, identity: FileKey) -> NamespaceIntent {
        let mut intent = NewNamespaceIntent {
            intent_id: intent_id(0x41),
            vault: vault(),
            op: NamespaceOpKind::Move,
            before: NamespaceBefore::Present(NamespaceSnapshot {
                entry: entry_id(0x62),
                revision: 3,
                parent: entry_id(0x61),
                path: "docs".to_owned(),
                kind: EntryKind::Directory,
                identity,
            }),
            after: NamespaceAfter {
                entry: entry_id(0x62),
                parent: entry_id(0x63),
                path: "archive/docs".to_owned(),
                kind: EntryKind::Directory,
            },
        }
        .prepared();
        intent.phase = phase;
        intent
    }

    fn created_intent(phase: PutPhase) -> NamespaceIntent {
        let mut intent = NewNamespaceIntent {
            intent_id: intent_id(0x42),
            vault: vault(),
            op: NamespaceOpKind::Create,
            before: NamespaceBefore::Absent,
            after: NamespaceAfter {
                entry: entry_id(0x66),
                parent: entry_id(0x61),
                path: "photos".to_owned(),
                kind: EntryKind::Directory,
            },
        }
        .prepared();
        intent.phase = phase;
        intent
    }

    #[tokio::test]
    async fn recovery_redoes_a_move_that_never_happened() {
        let mut store = FakeNamespaceStore::new()
            .with_baseline(docs())
            .with_baseline(archive());
        let mut log = FakeIntentLog::default();
        // 崩溃发生在改名之前：源还在、目标空着。
        let mut ns = FakeNamespace::new()
            .with_directory("docs")
            .with_directory("archive");
        let identity = *ns.state_of("docs").identity().expect("源目录在");
        log.namespace_intents
            .push(move_intent(PutPhase::Prepared, identity));
        let report =
            recover_namespace_vault(&mut log, &mut ns, &mut store, user_time(200), vault())
                .await
                .expect("恢复成功");
        assert_eq!(report.intents.len(), 1);
        assert_eq!(report.intents[0].decision, NamespaceRecoveryDecision::Redo);
        assert!(matches!(
            report.intents[0].outcome,
            NamespaceRecoveryOutcome::Redone { entry, .. } if entry == entry_id(0x62)
        ));
        assert_eq!(report.committed(), 1);
        assert!(!report.pauses_writes());
        assert_eq!(ns.state_of("docs"), PathState::Absent);
        assert!(ns.state_of("archive/docs").is_directory());
        assert_eq!(store.writes[0].expected_revision, Some(revision(3)));
    }

    #[tokio::test]
    async fn recovery_completes_a_move_that_already_happened_without_renaming_again() {
        let mut store = FakeNamespaceStore::new()
            .with_baseline(docs())
            .with_baseline(archive());
        let mut log = FakeIntentLog::default();
        let identity = FileKey {
            device: 7,
            inode: 300,
            links: 2,
            bytes: 64,
        };
        // 崩溃发生在“改名完成、阶段还没记下来”：源空了、目标是同一个 inode。
        log.namespace_intents
            .push(move_intent(PutPhase::Prepared, identity));
        let mut ns = FakeNamespace::new().with_directory("archive");
        ns.paths.insert(
            "archive/docs".to_owned(),
            (
                EntryKind::Directory,
                FileKey {
                    device: 7,
                    inode: 300,
                    links: 2,
                    bytes: 64,
                },
            ),
        );
        let report =
            recover_namespace_vault(&mut log, &mut ns, &mut store, user_time(200), vault())
                .await
                .expect("恢复成功");
        assert_eq!(
            report.intents[0].decision,
            NamespaceRecoveryDecision::Complete
        );
        assert!(matches!(
            report.intents[0].outcome,
            NamespaceRecoveryOutcome::Completed { entry, .. } if entry == entry_id(0x62)
        ));
        assert!(ns.renames.is_empty(), "已完成的移动不得再改一次名");
        assert_eq!(log.namespace_intents[0].phase, PutPhase::Published);
        assert_eq!(store.writes.len(), 1);
    }

    #[tokio::test]
    async fn recovery_preserves_both_sides_when_the_target_is_taken() {
        let mut store = FakeNamespaceStore::new()
            .with_baseline(docs())
            .with_baseline(archive());
        let mut log = FakeIntentLog::default();
        let mut ns = FakeNamespace::new()
            .with_directory("docs")
            .with_directory("archive");
        let identity = *ns.state_of("docs").identity().expect("源目录在");
        log.namespace_intents
            .push(move_intent(PutPhase::Prepared, identity));
        // 目标位置被别的条目占了：不覆盖、不提交。
        ns = ns.with_directory("archive/docs");
        let report =
            recover_namespace_vault(&mut log, &mut ns, &mut store, user_time(200), vault())
                .await
                .expect("恢复成功");
        assert_eq!(
            report.intents[0].outcome,
            NamespaceRecoveryOutcome::Preserved {
                reason: NamespaceReconcileReason::TargetOccupied
            }
        );
        assert!(report.needs_reconciliation());
        assert!(!report.pauses_writes());
        assert!(ns.renames.is_empty());
        assert!(store.writes.is_empty(), "协调中的意图不得写事务");
        assert!(ns.state_of("docs").is_directory());
    }

    #[tokio::test]
    async fn recovery_pauses_writes_when_the_working_tree_is_unreadable() {
        let mut store = FakeNamespaceStore::new()
            .with_baseline(docs())
            .with_baseline(archive());
        let mut log = FakeIntentLog::default();
        let mut ns = FakeNamespace::new()
            .with_directory("docs")
            .with_directory("archive");
        let identity = *ns.state_of("docs").identity().expect("源目录在");
        log.namespace_intents
            .push(move_intent(PutPhase::Prepared, identity));
        ns.inspect_error = Some(NamespaceError::Unreadable {
            detail: "EACCES".to_owned(),
        });
        let report =
            recover_namespace_vault(&mut log, &mut ns, &mut store, user_time(200), vault())
                .await
                .expect("恢复成功");
        assert_eq!(
            report.intents[0].outcome,
            NamespaceRecoveryOutcome::NeedsRecovery {
                reason: NeedsRecoveryReason::TargetUnreadable
            }
        );
        assert!(report.pauses_writes());
        assert_eq!(log.namespace_intents[0].phase, PutPhase::NeedsRecovery);
        assert!(ns.renames.is_empty());
        assert!(store.writes.is_empty());
    }

    #[tokio::test]
    async fn recovery_of_a_create_reads_the_same_evidence() {
        // 目录没建：重做 mkdir。
        let mut store = FakeNamespaceStore::new();
        let mut log = FakeIntentLog::default();
        log.namespace_intents
            .push(created_intent(PutPhase::Prepared));
        let mut ns = FakeNamespace::new();
        let report =
            recover_namespace_vault(&mut log, &mut ns, &mut store, user_time(200), vault())
                .await
                .expect("恢复成功");
        assert_eq!(report.intents[0].decision, NamespaceRecoveryDecision::Redo);
        assert!(ns.state_of("photos").is_directory());
        assert_eq!(store.writes[0].expected_revision, None);

        // 目录已建、阶段还停在 published：只补事务。
        let mut store = FakeNamespaceStore::new();
        let mut log = FakeIntentLog::default();
        log.namespace_intents
            .push(created_intent(PutPhase::Published));
        let mut ns = FakeNamespace::new().with_directory("photos");
        let report =
            recover_namespace_vault(&mut log, &mut ns, &mut store, user_time(200), vault())
                .await
                .expect("恢复成功");
        assert!(matches!(
            report.intents[0].outcome,
            NamespaceRecoveryOutcome::Completed { entry, .. } if entry == entry_id(0x66)
        ));
        assert_eq!(store.writes.len(), 1);
        assert_eq!(
            store
                .trace
                .entries()
                .iter()
                .filter(|line| line.contains("mkdir"))
                .count(),
            0,
            "已完成的创建不得再建一次"
        );
    }

    #[test]
    fn namespace_evidence_round_trips_through_the_canonical_encoding() {
        let absent = NamespaceBefore::Absent;
        assert_eq!(absent.to_json(), r#"{"present":false}"#);
        assert_eq!(NamespaceBefore::from_json(&absent.to_json()), Ok(absent));

        let present = NamespaceBefore::Present(NamespaceSnapshot {
            entry: entry_id(0x62),
            revision: 3,
            parent: entry_id(0x61),
            // 名称里带引号与反斜杠：证据编码不能靠“名字通常很干净”。
            path: "docs/a\"b\\c".to_owned(),
            kind: EntryKind::Directory,
            identity: FileKey {
                device: 7,
                inode: 300,
                links: 2,
                bytes: 64,
            },
        });
        let text = present.to_json();
        assert!(
            text.contains(r#""path_hex":"646f63732f6122625c63""#),
            "{text}"
        );
        assert_eq!(NamespaceBefore::from_json(&text), Ok(present));

        let after = NamespaceAfter {
            entry: entry_id(0x66),
            parent: entry_id(0x61),
            path: "photos".to_owned(),
            kind: EntryKind::Directory,
        };
        assert_eq!(NamespaceAfter::from_json(&after.to_json()), Ok(after));

        // 非规范写法一律拒绝，不猜现场。
        for (text, code) in [
            (r#"[]"#, "state_not_object"),
            (r#"{"present":false,"extra":"x"}"#, "state_unknown_field"),
            (r#"{"present":true}"#, "state_missing_field"),
            (r#"{"present":"yes"}"#, "state_type_mismatch"),
            (
                r#"{"present":true,"entry":"62626262626262626262626262626262","revision":"3","parent":"61616161616161616161616161616161","path_hex":"zz","kind":"directory","size":"64","device":"7","inode":"300","links":"2"}"#,
                "state_invalid_value",
            ),
            (
                r#"{"present":true,"entry":"62626262626262626262626262626262","revision":"3","parent":"61616161616161616161616161616161","path_hex":"646f6373","kind":"socket","size":"64","device":"7","inode":"300","links":"2"}"#,
                "state_invalid_value",
            ),
        ] {
            let error = NamespaceBefore::from_json(text).expect_err("必须拒绝");
            assert_eq!(error.code(), code, "文本 {text}");
        }
        assert_eq!(
            NamespaceAfter::from_json(r#"{"present":false}"#)
                .expect_err("目标不能为空")
                .code(),
            "state_invalid_value"
        );
    }

    #[test]
    fn operation_kinds_match_the_intent_log_check_values() {
        for kind in [NamespaceOpKind::Create, NamespaceOpKind::Move] {
            assert_eq!(NamespaceOpKind::from_text(kind.as_text()), Some(kind));
        }
        assert_eq!(NamespaceOpKind::from_text("delete"), None);
        assert_eq!(NamespaceOpKind::from_text("put_content"), None);
    }
}
