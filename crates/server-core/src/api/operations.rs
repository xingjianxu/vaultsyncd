//! 操作端点（design §15.2 `POST /v1/roots/{root}/operations`、`GET /v1/roots/{root}/operations/{op}`、
//! §15.3 修改提交示例、§15.4 错误契约、I06）。
//!
//! 边界：本模块决定“这次命令能不能受理、已有的操作记录怎么回答、失败怎么映射成错误码与状态码”，
//! 不做 HTTP、不写数据库、不碰工作目录。执行命令体（put/create/move/delete/restore 的提交链路）
//! 走各命令自己的端口与适配器；`operations` 行的读写由适配器完成（`server-store::operations`）。
//!
//! 固定的顺序与判据：
//!
//! 1. **先 epoch，后形状**：[`admit`] 先比较请求携带的服务器 epoch 与 Root 当前 epoch，不符即
//!    `EPOCH_CHANGED`（design §15.4：客户端进入保留基线的重建流程）；随后才是命令自身的形状校验。
//!    Root 当前不可写（挂载/身份核对失败）时同样在动任何状态之前返回 `ROOT_UNAVAILABLE`。
//! 2. **幂等键是 `(root, device, op_id)`**（design §7.2、I06）：同一键同一语义摘要就是重放，必须
//!    返回已记录的结果而不重做；同一键不同摘要说明客户端把同一个身份用于了两个请求，是协议错误
//!    `OP_ID_REUSED`，绝不用旧结果回答新请求（[`decide_replay`]）。
//! 3. **pending 不是 committed**（design I01）：`pending` 只表示“已受理、还没有业务结果”，它的回答
//!    必须是 202 且**不携带** revision/root_seq/version/entry 一类“已完成”字段
//!    （[`RecordedOutcome::http_status`]、[`is_synced`]）。
//! 4. **摘要只覆盖语义**：命令种类、身份、基线 revision、上传引用、本地代际、冲突策略与命名空间
//!    目标参与摘要（[`OperationRequest::digest_fields`]）；`op_id` 与 `device_id` 是幂等键本身、
//!    服务器 epoch 是客户端的事实基础而非请求语义，因此都不参与摘要——epoch 变化由 410 单独回答，
//!    不能让“重试时带来的新 epoch”变成 `OP_ID_REUSED`。
//! 5. **错误码由本层给出**：每个失败都带 design 的稳定错误码与 HTTP 状态码
//!    （[`OperationFailure::design_error_code`]、[`OperationFailure::http_status`]），上层只做文本到
//!    wire 枚举的翻译，不另写一套映射。

use std::fmt;

use vaultsyncd_domain::ids::{DeviceId, EntryId, OpId, UploadId, VersionId};
use vaultsyncd_domain::version::{Generation, Revision, RootSeq, ServerEpoch};

/// 命令种类闭集（design §15.2：Put/Create/Move/Delete/Restore）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    /// 用已上传并校验的内容提交某个条目的新版本（CAS）。
    PutContent,
    /// 在已有父目录下新建条目。
    Create,
    /// 同 Root 内移动条目（父关系/名称改变，内容引用不变）。
    Move,
    /// 删除条目；目录另带服务端给出的子树 token。
    Delete,
    /// 从回收站/历史版本恢复条目。
    Restore,
}

impl OperationKind {
    /// wire 与数据库里的规范文本。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::PutContent => "put_content",
            Self::Create => "create",
            Self::Move => "move",
            Self::Delete => "delete",
            Self::Restore => "restore",
        }
    }

    /// 解析 [`Self::as_text`]；未知文本返回 `None`，由调用方按请求错误处理，不猜默认命令。
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "put_content" => Some(Self::PutContent),
            "create" => Some(Self::Create),
            "move" => Some(Self::Move),
            "delete" => Some(Self::Delete),
            "restore" => Some(Self::Restore),
            _ => None,
        }
    }
}

/// 过期基线的冲突策略（design §15.3）。
///
/// 首发只有 `preserve_both`：过期修改可以提交为“服务器主条目 + 冲突副本”，两条都在，绝不丢内容。
/// 其它策略（例如直接失败）等客户端真正需要时再进契约，未知取值按请求错误处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// 保留双方：提交成功并给出冲突副本身份。
    PreserveBoth,
}

impl ConflictPolicy {
    /// wire 与数据库里的规范文本。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::PreserveBoth => "preserve_both",
        }
    }

    /// 解析 [`Self::as_text`]；未知文本返回 `None`。
    #[must_use]
    pub fn from_text(text: &str) -> Option<Self> {
        match text {
            "preserve_both" => Some(Self::PreserveBoth),
            _ => None,
        }
    }
}

/// 一次命令的语义（wire 文本已在适配层解析成身份与计数）。
///
/// 不同命令用到的字段不同；用不到的字段必须为空，形状校验（[`Self::validate`]）会拒绝混用，
/// 因此“同一份请求里有两套命令语义”不会悄悄通过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRequest {
    /// 客户端生成的逻辑操作身份（重试必须复用它）。
    pub op: OpId,
    /// 发起操作的设备：幂等键的一部分（design §7.2）。
    pub device: DeviceId,
    /// 命令种类。
    pub kind: OperationKind,
    /// 客户端事实基础所属的服务器 epoch。
    pub expected_epoch: ServerEpoch,
    /// 被操作或被覆盖的条目（put/delete/move 的源/restore）。
    pub entry: Option<EntryId>,
    /// 客户端计划时的条目 revision（put/move/delete/restore 的 CAS 基线）。
    pub base_revision: Option<Revision>,
    /// 客户端计划时的版本身份（put 可选：声明“我看到的内容是这一版”）。
    pub base_version: Option<VersionId>,
    /// 已 finalize 的上传会话（put 必填；提交只引用它发布出的内容，不发字节）。
    pub upload: Option<UploadId>,
    /// 本地代际：服务器只把它原样回显，**不**用它证明客户端没有后续修改（design §15.3）。
    pub local_generation: Option<Generation>,
    /// 过期基线的冲突策略（缺省 `preserve_both`）。
    pub conflict_policy: ConflictPolicy,
    /// 目标父目录（create/move）。
    pub parent: Option<EntryId>,
    /// 目标名称（create/move）。
    pub name: Option<String>,
    /// 新建时必须为真的“目标原本不存在”声明（design §15.3）。
    pub expected_absent: Option<bool>,
    /// 目录删除的服务端子树 token（16 字节不透明值）。
    pub subtree_token: Option<[u8; 16]>,
    /// 恢复指定的历史版本；为空表示恢复回收站里的当前形态。
    pub version: Option<VersionId>,
}

impl OperationRequest {
    /// 检查请求自身的形状与命令前提，不做任何 I/O。
    ///
    /// 规则按命令分别固定：put 需要 entry/base_revision/upload 且不得带命名空间目标；create 需要
    /// parent/name 且 `expected_absent=true`、不得自带 entry（身份由服务器分配）；move 需要
    /// entry/base_revision/parent/name；delete 需要 entry/base_revision（token 按条目种类在执行前
    /// 由服务端事实判定，这里只校验形态）；restore 需要 entry/base_revision。
    ///
    /// # Errors
    ///
    /// 形状不符 → [`OperationFailure::InvalidRequest`]；名称不可用 → [`OperationFailure::InvalidName`]。
    pub fn validate(&self) -> Result<(), OperationFailure> {
        match self.kind {
            OperationKind::PutContent => {
                self.require_entry()?;
                self.require_revision()?;
                if self.upload.is_none() {
                    return Err(invalid("put_content 必须带 upload_id"));
                }
                self.reject_namespace_target()?;
                self.reject_subtree_token()?;
                self.reject_version()?;
            }
            OperationKind::Create => {
                if self.entry.is_some() {
                    return Err(invalid(
                        "create 的条目身份由服务器分配，请求不得自带 entry_id",
                    ));
                }
                if self.parent.is_none() {
                    return Err(invalid("create 必须带 parent_id"));
                }
                self.validate_name()?;
                if self.expected_absent != Some(true) {
                    return Err(invalid("create 必须声明 expected_absent=true"));
                }
                if self.base_revision.is_some() || self.base_version.is_some() {
                    return Err(invalid("create 不得带 base_revision/base_version_id"));
                }
                self.reject_subtree_token()?;
                self.reject_version()?;
            }
            OperationKind::Move => {
                self.require_entry()?;
                self.require_revision()?;
                if self.parent.is_none() {
                    return Err(invalid("move 必须带目标 parent_id"));
                }
                self.validate_name()?;
                if self.expected_absent.is_some() || self.upload.is_some() {
                    return Err(invalid("move 不得带 expected_absent/upload_id"));
                }
                self.reject_subtree_token()?;
                self.reject_version()?;
            }
            OperationKind::Delete => {
                self.require_entry()?;
                self.require_revision()?;
                if self.parent.is_some() || self.name.is_some() || self.expected_absent.is_some() {
                    return Err(invalid("delete 不得带命名空间目标"));
                }
                if self.upload.is_some() || self.version.is_some() {
                    return Err(invalid("delete 不得带 upload_id/version_id"));
                }
            }
            OperationKind::Restore => {
                self.require_entry()?;
                self.require_revision()?;
                if self.parent.is_some() || self.name.is_some() || self.expected_absent.is_some() {
                    return Err(invalid("restore 不得带命名空间目标"));
                }
                if self.upload.is_some() || self.base_version.is_some() {
                    return Err(invalid("restore 不得带 upload_id/base_version_id"));
                }
                self.reject_subtree_token()?;
            }
        }
        Ok(())
    }

    /// 参与请求摘要的语义字段（字段名固定、顺序无关；摘要算法由存储层给出）。
    ///
    /// 未设置的字段不参与：形状校验保证同一命令不会因为“多余的空字段”变成另一个请求。
    #[must_use]
    pub fn digest_fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            ("kind", self.kind.as_text().to_owned()),
            ("conflict_policy", self.conflict_policy.as_text().to_owned()),
        ];
        if let Some(entry) = self.entry {
            fields.push(("entry_id", entry.to_hex()));
        }
        if let Some(revision) = self.base_revision {
            fields.push(("base_revision", revision.to_string()));
        }
        if let Some(version) = self.base_version {
            fields.push(("base_version_id", version.to_hex()));
        }
        if let Some(upload) = self.upload {
            fields.push(("upload_id", upload.to_hex()));
        }
        if let Some(generation) = self.local_generation {
            fields.push(("local_generation", generation.to_string()));
        }
        if let Some(parent) = self.parent {
            fields.push(("parent_id", parent.to_hex()));
        }
        if let Some(name) = &self.name {
            fields.push(("name", name.clone()));
        }
        if let Some(expected_absent) = self.expected_absent {
            fields.push(("expected_absent", expected_absent.to_string()));
        }
        if let Some(token) = self.subtree_token {
            let mut text = String::with_capacity(32);
            for byte in token {
                text.push_str(&format!("{byte:02x}"));
            }
            fields.push(("subtree_token", text));
        }
        if let Some(version) = self.version {
            fields.push(("version_id", version.to_hex()));
        }
        fields
    }

    fn require_entry(&self) -> Result<(), OperationFailure> {
        if self.entry.is_none() {
            return Err(invalid(format!("{} 必须带 entry_id", self.kind.as_text())));
        }
        Ok(())
    }

    fn require_revision(&self) -> Result<(), OperationFailure> {
        if self.base_revision.is_none() {
            return Err(invalid(format!(
                "{} 必须带 base_revision",
                self.kind.as_text()
            )));
        }
        Ok(())
    }

    fn reject_namespace_target(&self) -> Result<(), OperationFailure> {
        if self.parent.is_some() || self.name.is_some() || self.expected_absent.is_some() {
            return Err(invalid(format!(
                "{} 不得带命名空间目标",
                self.kind.as_text()
            )));
        }
        Ok(())
    }

    fn reject_subtree_token(&self) -> Result<(), OperationFailure> {
        if self.subtree_token.is_some() {
            return Err(invalid(format!(
                "{} 不得带 subtree_token",
                self.kind.as_text()
            )));
        }
        Ok(())
    }

    fn reject_version(&self) -> Result<(), OperationFailure> {
        if self.version.is_some() {
            return Err(invalid(format!(
                "{} 不得带 version_id",
                self.kind.as_text()
            )));
        }
        Ok(())
    }

    /// 名称的服务端底线检查：非空、不是 `.`/`..`、不含路径分隔符与 NUL。
    ///
    /// 完整的平台名称等价与保留名规则属 `crates/fs-common`（S017）与平台层；服务端只拒绝那些
    /// 一定会把条目放到别的位置或让路径失去意义的输入（design §15.4 `INVALID_NAME`）。
    fn validate_name(&self) -> Result<(), OperationFailure> {
        let name = self
            .name
            .as_deref()
            .ok_or_else(|| invalid(format!("{} 必须带 name", self.kind.as_text())))?;
        if name.is_empty() || name == "." || name == ".." {
            return Err(OperationFailure::InvalidName {
                detail: format!("名称 {name:?} 不能作为条目名"),
            });
        }
        if name.contains('/') || name.contains('\\') || name.contains('\0') {
            return Err(OperationFailure::InvalidName {
                detail: "名称不得包含路径分隔符或 NUL".to_owned(),
            });
        }
        Ok(())
    }
}

/// 服务端为这次命令能确认的 Root 事实（适配器从 `roots` 行给出）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootFacts {
    /// Root 当前 epoch：恢复后更换，旧 epoch 的预期不得当成同一事实（design §7.1）。
    pub epoch: ServerEpoch,
    /// Root 当前是否可写（health 为 healthy 且写入门已开放）。
    pub accepts_writes: bool,
}

/// 受理判定：epoch 与 Root 可用性先于命令形状（[`admit`] 的注释给固定顺序）。
///
/// # Errors
///
/// Root 不可写 → [`OperationFailure::RootUnavailable`]；epoch 不符 →
/// [`OperationFailure::EpochChanged`]；形状不符 → [`OperationFailure::InvalidRequest`] 或
/// [`OperationFailure::InvalidName`]。
pub fn admit(facts: RootFacts, request: &OperationRequest) -> Result<(), OperationFailure> {
    if !facts.accepts_writes {
        return Err(OperationFailure::RootUnavailable);
    }
    if request.expected_epoch != facts.epoch {
        return Err(OperationFailure::EpochChanged {
            expected: request.expected_epoch,
            actual: facts.epoch,
        });
    }
    request.validate()
}

/// 操作日志里已有记录的业务结果（`operations` 行的 `state` 与结果列）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedOutcome {
    /// 已提交：结果与该批次占用的 RootSeq 都是终态。
    Committed {
        /// 该批次占用的 RootSeq。
        root_seq: RootSeq,
        /// 紧凑结果（wire `OperationResult` 的 JSON）。
        result_json: String,
    },
    /// 已受理、还没有业务结果：不占 RootSeq，不判已同步。
    Pending,
    /// 已稳定拒绝：同一 OpId 的重复请求得到同一个结论。
    Rejected {
        /// 稳定拒绝的紧凑结果（wire `OperationResult` 的 JSON），缺省表示只有状态。
        result_json: Option<String>,
    },
}

impl RecordedOutcome {
    /// 回答这条记录用的 HTTP 状态码：committed/rejected 是终态 200，pending 是 202。
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::Committed { .. } | Self::Rejected { .. } => 200,
            Self::Pending => 202,
        }
    }

    /// 是否已经产生业务结果（`pending` 为假：design I01 不把 pending 当 committed）。
    #[must_use]
    pub const fn is_synced(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }
}

/// 操作日志里已有的一条记录（摘要 + 业务结果）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedOperation {
    /// 记录下来的请求摘要（32 字节 SHA-256）。
    pub request_digest: [u8; 32],
    /// 记录下来的业务结果。
    pub outcome: RecordedOutcome,
}

/// 幂等判定：同一 OpId 同一摘要就是重放，不同摘要就是协议错误。
///
/// # Errors
///
/// 摘要不同 → [`OperationFailure::OpIdReused`]；调用方不得用旧结果回答新请求。
pub fn decide_replay<'a>(
    recorded: &'a RecordedOperation,
    request_digest: &[u8; 32],
) -> Result<&'a RecordedOutcome, OperationFailure> {
    if &recorded.request_digest != request_digest {
        return Err(OperationFailure::OpIdReused);
    }
    Ok(&recorded.outcome)
}

/// 命令受理或重放失败的分类（HTTP 层据此给状态码与错误码，不另写映射）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationFailure {
    /// Root 当前不可写（挂载/身份核对失败或写入门关闭）：暂停该 Root，保留队列。
    RootUnavailable,
    /// 请求携带的 epoch 不是服务器当前 epoch：客户端进入保留基线的重建流程。
    EpochChanged {
        /// 请求携带的 epoch。
        expected: ServerEpoch,
        /// Root 当前的 epoch。
        actual: ServerEpoch,
    },
    /// 同一 OpId 已被不同语义的请求占用（协议错误，停止而不是重试）。
    OpIdReused,
    /// 条目基线已不是请求的 revision：拉取最新事实后重新规划。
    BaseMismatch {
        /// 原因文本（不回显内容）。
        detail: String,
    },
    /// 父目录/目标位置或子树在计划后变化：重新规划。
    SubtreeChanged {
        /// 原因文本。
        detail: String,
    },
    /// 现场需要先采集外部变化（design §12.2）：不强制重试覆盖。
    ReconcileRequired {
        /// 原因文本。
        detail: String,
    },
    /// 请求形状不成立（命令前提、身份文本、区间）。
    InvalidRequest {
        /// 原因文本。
        detail: String,
    },
    /// 名称不可用（空、`.`/`..`、含分隔符）。
    InvalidName {
        /// 原因文本。
        detail: String,
    },
    /// 条目种类不支持该命令（例如对目录做内容提交）。
    UnsupportedEntry {
        /// 原因文本。
        detail: String,
    },
    /// 操作记录不存在、不属于该 Root 或不属于该登录会话（三者不区分）。
    UnknownOperation,
    /// 历史版本/回收站内容已过保留期或不可读：解释历史已过期，不拿同路径新内容冒充（410）。
    VersionGone {
        /// 原因文本。
        detail: String,
    },
    /// 本进程尚未装配命令执行器：命令没有被受理，也没有写任何状态。
    ExecutorUnavailable,
    /// 存储层失败（含需要恢复的坏状态），必须暴露原因而不是当作成功。
    Storage {
        /// 失败细节。
        detail: String,
    },
    /// 空间不足：保留脏数据，立即通知。
    NoSpace,
}

/// 本进程尚未装配命令执行器时使用的 wire 错误码。
///
/// design §15.4 的闭集里没有“执行器未装配”这一项：`NEEDS_RECOVERY` 专指“意图无法解释、需要人工
/// 介入”，拿它当“本卡还没接上执行链路”的通用码会误导客户端与运维。这个码因此**待与 design §15.4
/// 同步**（交接里登记），语义是“服务端这一版还不提供命令执行，重试要等服务端升级/装配完成”。
pub const EXECUTOR_UNAVAILABLE_CODE: &str = "SERVICE_NOT_READY";

const REQUEST_DIGEST_LEN: usize = 32;

impl OperationFailure {
    /// design §15.4 的稳定错误码文本。
    #[must_use]
    pub const fn design_error_code(&self) -> &'static str {
        match self {
            Self::RootUnavailable => "ROOT_UNAVAILABLE",
            Self::EpochChanged { .. } => "EPOCH_CHANGED",
            Self::OpIdReused => "OP_ID_REUSED",
            Self::BaseMismatch { .. } => "BASE_MISMATCH",
            Self::SubtreeChanged { .. } => "SUBTREE_CHANGED",
            Self::ReconcileRequired { .. } => "RECONCILE_REQUIRED",
            Self::InvalidRequest { .. } => "INVALID_REQUEST",
            Self::InvalidName { .. } => "INVALID_NAME",
            Self::UnsupportedEntry { .. } => "UNSUPPORTED_ENTRY",
            Self::UnknownOperation => "UNSUPPORTED_ENTRY",
            Self::VersionGone { .. } => "VERSION_GONE",
            Self::ExecutorUnavailable => EXECUTOR_UNAVAILABLE_CODE,
            Self::Storage { .. } => "NEEDS_RECOVERY",
            Self::NoSpace => "INSUFFICIENT_STORAGE",
        }
    }

    /// 与 [`Self::design_error_code`] 对应的 HTTP 状态码。
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::RootUnavailable | Self::Storage { .. } | Self::ExecutorUnavailable => 503,
            Self::EpochChanged { .. } | Self::VersionGone { .. } => 410,
            Self::OpIdReused
            | Self::BaseMismatch { .. }
            | Self::SubtreeChanged { .. }
            | Self::ReconcileRequired { .. } => 409,
            Self::InvalidRequest { .. } => 400,
            Self::InvalidName { .. } | Self::UnsupportedEntry { .. } => 422,
            Self::UnknownOperation => 404,
            Self::NoSpace => 507,
        }
    }

    /// 同一请求稍后原样重试是否可能成功。
    ///
    /// 基线/子树/epoch 类失败都要求客户端先刷新事实再重新规划，因此对“原样重试”为假；
    /// Root 不可用、执行器未装配、存储失败与空间不足是服务端状态，原样重试在服务端恢复后可能成功。
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::RootUnavailable
            | Self::Storage { .. }
            | Self::ExecutorUnavailable
            | Self::NoSpace => true,
            Self::EpochChanged { .. }
            | Self::OpIdReused
            | Self::BaseMismatch { .. }
            | Self::SubtreeChanged { .. }
            | Self::ReconcileRequired { .. }
            | Self::InvalidRequest { .. }
            | Self::InvalidName { .. }
            | Self::UnsupportedEntry { .. }
            | Self::VersionGone { .. }
            | Self::UnknownOperation => false,
        }
    }
}

impl fmt::Display for OperationFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootUnavailable => f.write_str("Root 当前不可写，已暂停该目录的写操作"),
            Self::EpochChanged { expected, actual } => write!(
                f,
                "请求的服务器 epoch {} 不是当前 epoch {}",
                expected, actual
            ),
            Self::OpIdReused => f.write_str("同一 OpId 已用于不同语义的请求"),
            Self::BaseMismatch { detail } => write!(f, "条目基线已过期：{detail}"),
            Self::SubtreeChanged { detail } => write!(f, "子树在计划后变化：{detail}"),
            Self::ReconcileRequired { detail } => {
                write!(f, "现场需要先采集外部变化：{detail}")
            }
            Self::InvalidRequest { detail } => write!(f, "请求不成立：{detail}"),
            Self::InvalidName { detail } => write!(f, "名称不可用：{detail}"),
            Self::UnsupportedEntry { detail } => write!(f, "条目种类不支持该命令：{detail}"),
            Self::VersionGone { detail } => write!(f, "历史内容已过期或不可读：{detail}"),
            Self::UnknownOperation => f.write_str("操作记录不存在"),
            Self::ExecutorUnavailable => f.write_str("服务端尚未装配命令执行器，本次操作未被受理"),
            Self::Storage { detail } => write!(f, "服务端存储失败：{detail}"),
            Self::NoSpace => f.write_str("服务端空间不足"),
        }
    }
}

impl std::error::Error for OperationFailure {}

fn invalid(detail: impl Into<String>) -> OperationFailure {
    OperationFailure::InvalidRequest {
        detail: detail.into(),
    }
}

/// 请求摘要的字节长度（与 `server-store` 的 `RequestDigest` 一致）。
#[must_use]
pub const fn request_digest_len() -> usize {
    REQUEST_DIGEST_LEN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(hex: u8) -> OpId {
        OpId::from_bytes([hex; 16])
    }

    fn request(kind: OperationKind) -> OperationRequest {
        OperationRequest {
            op: op(0x01),
            device: DeviceId::from_bytes([0x02; 16]),
            kind,
            expected_epoch: ServerEpoch::new(7).expect("epoch"),
            entry: None,
            base_revision: None,
            base_version: None,
            upload: None,
            local_generation: None,
            conflict_policy: ConflictPolicy::PreserveBoth,
            parent: None,
            name: None,
            expected_absent: None,
            subtree_token: None,
            version: None,
        }
    }

    fn put() -> OperationRequest {
        OperationRequest {
            entry: Some(EntryId::from_bytes([0x11; 16])),
            base_revision: Some(Revision::new(17).expect("修订")),
            upload: Some(UploadId::from_bytes([0x22; 16])),
            ..request(OperationKind::PutContent)
        }
    }

    fn create() -> OperationRequest {
        OperationRequest {
            parent: Some(EntryId::from_bytes([0x33; 16])),
            name: Some("报告.docx".to_owned()),
            expected_absent: Some(true),
            ..request(OperationKind::Create)
        }
    }

    #[test]
    fn kinds_and_policies_round_trip_and_reject_unknown_text() {
        for kind in [
            OperationKind::PutContent,
            OperationKind::Create,
            OperationKind::Move,
            OperationKind::Delete,
            OperationKind::Restore,
        ] {
            assert_eq!(OperationKind::from_text(kind.as_text()), Some(kind));
        }
        assert_eq!(OperationKind::from_text("delete_recursive"), None);
        assert_eq!(
            ConflictPolicy::from_text(ConflictPolicy::PreserveBoth.as_text()),
            Some(ConflictPolicy::PreserveBoth)
        );
        assert_eq!(ConflictPolicy::from_text("last_write_wins"), None);
    }

    #[test]
    fn well_formed_commands_pass_shape_checks() {
        put().validate().expect("put 形状");
        create().validate().expect("create 形状");
        OperationRequest {
            entry: Some(EntryId::from_bytes([0x11; 16])),
            base_revision: Some(Revision::new(3).expect("修订")),
            parent: Some(EntryId::from_bytes([0x44; 16])),
            name: Some("新名字.txt".to_owned()),
            ..request(OperationKind::Move)
        }
        .validate()
        .expect("move 形状");
        OperationRequest {
            entry: Some(EntryId::from_bytes([0x11; 16])),
            base_revision: Some(Revision::new(5).expect("修订")),
            subtree_token: Some([0x5a; 16]),
            ..request(OperationKind::Delete)
        }
        .validate()
        .expect("delete 形状");
        OperationRequest {
            entry: Some(EntryId::from_bytes([0x11; 16])),
            base_revision: Some(Revision::new(5).expect("修订")),
            version: Some(VersionId::from_bytes([0x66; 16])),
            ..request(OperationKind::Restore)
        }
        .validate()
        .expect("restore 形状");
    }

    #[test]
    fn missing_prerequisites_and_mixed_commands_are_invalid_requests() {
        // put 缺 upload。
        let missing_upload = OperationRequest {
            upload: None,
            ..put()
        };
        assert_eq!(
            missing_upload.validate(),
            Err(OperationFailure::InvalidRequest {
                detail: "put_content 必须带 upload_id".to_owned()
            })
        );
        // put 带上命名空间目标：同一请求里有两套语义。
        let mixed = OperationRequest {
            name: Some("别处.txt".to_owned()),
            ..put()
        };
        assert!(mixed.validate().is_err());
        // create 没有 expected_absent=true。
        let no_declaration = OperationRequest {
            expected_absent: None,
            ..create()
        };
        assert!(no_declaration.validate().is_err());
        // create 自带 entry。
        let with_entry = OperationRequest {
            entry: Some(EntryId::from_bytes([0x77; 16])),
            ..create()
        };
        assert!(with_entry.validate().is_err());
        // delete 没有 revision。
        let no_revision = OperationRequest {
            entry: Some(EntryId::from_bytes([0x11; 16])),
            ..request(OperationKind::Delete)
        };
        assert!(no_revision.validate().is_err());
        // delete 带上 version。
        let delete_version = OperationRequest {
            entry: Some(EntryId::from_bytes([0x11; 16])),
            base_revision: Some(Revision::new(2).expect("修订")),
            version: Some(VersionId::from_bytes([0x88; 16])),
            ..request(OperationKind::Delete)
        };
        assert!(delete_version.validate().is_err());
    }

    #[test]
    fn names_with_separators_or_dots_are_rejected_as_invalid_names() {
        for name in ["", ".", "..", "a/b.txt", "a\\b.txt", "a\0b"] {
            let bad = OperationRequest {
                name: Some(name.to_owned()),
                ..create()
            };
            assert!(
                matches!(bad.validate(), Err(OperationFailure::InvalidName { .. })),
                "名称 {name:?} 必须被拒绝"
            );
        }
    }

    #[test]
    fn admit_reports_unavailable_root_and_stale_epoch_before_shape() {
        let stale = OperationRequest {
            expected_epoch: ServerEpoch::new(6).expect("epoch"),
            // 形状也不成立：epoch 必须先被回答。
            upload: None,
            ..put()
        };
        assert_eq!(
            admit(
                RootFacts {
                    epoch: ServerEpoch::new(7).expect("epoch"),
                    accepts_writes: true,
                },
                &stale
            ),
            Err(OperationFailure::EpochChanged {
                expected: ServerEpoch::new(6).expect("epoch"),
                actual: ServerEpoch::new(7).expect("epoch"),
            })
        );
        assert_eq!(
            admit(
                RootFacts {
                    epoch: ServerEpoch::new(7).expect("epoch"),
                    accepts_writes: false,
                },
                &put()
            ),
            Err(OperationFailure::RootUnavailable)
        );
        admit(
            RootFacts {
                epoch: ServerEpoch::new(7).expect("epoch"),
                accepts_writes: true,
            },
            &put(),
        )
        .expect("正常受理");
    }

    #[test]
    fn digest_fields_cover_semantics_but_not_op_device_or_epoch() {
        let first = put();
        let fields = first.digest_fields();
        let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"kind"));
        assert!(names.contains(&"entry_id"));
        assert!(names.contains(&"base_revision"));
        assert!(names.contains(&"upload_id"));
        assert!(names.contains(&"conflict_policy"));
        for absent in ["op_id", "device_id", "server_epoch"] {
            assert!(!names.contains(&absent), "{absent} 不得进入摘要");
        }

        // 语义相同 → 字段相同；任一语义字段变化 → 字段不同。
        assert_eq!(first.digest_fields(), put().digest_fields());
        let other_revision = OperationRequest {
            base_revision: Some(Revision::new(18).expect("修订")),
            ..put()
        };
        assert_ne!(first.digest_fields(), other_revision.digest_fields());
        // 本地代际参与摘要：同一 OpId 用不同代际重试是不同请求。
        let other_generation = OperationRequest {
            local_generation: Some(Generation::new(8).expect("generation")),
            ..put()
        };
        assert_ne!(first.digest_fields(), other_generation.digest_fields());
        // 未设置字段不参与。
        assert!(
            create()
                .digest_fields()
                .iter()
                .all(|(name, _)| *name != "upload_id")
        );
    }

    #[test]
    fn replay_requires_the_same_digest_and_pending_is_not_synced() {
        let recorded = RecordedOperation {
            request_digest: [0xab; REQUEST_DIGEST_LEN],
            outcome: RecordedOutcome::Pending,
        };
        assert_eq!(
            decide_replay(&recorded, &[0xab; REQUEST_DIGEST_LEN]),
            Ok(&RecordedOutcome::Pending)
        );
        let reused = decide_replay(&recorded, &[0xcd; REQUEST_DIGEST_LEN]);
        assert_eq!(reused, Err(OperationFailure::OpIdReused));
        assert_eq!(RecordedOutcome::Pending.http_status(), 202);
        assert!(!RecordedOutcome::Pending.is_synced());
        assert_eq!(request_digest_len(), REQUEST_DIGEST_LEN);
    }

    #[test]
    fn failure_codes_statuses_and_retryability_follow_the_contract_table() {
        let cases = [
            (
                OperationFailure::RootUnavailable,
                "ROOT_UNAVAILABLE",
                503,
                true,
            ),
            (
                OperationFailure::EpochChanged {
                    expected: ServerEpoch::ZERO,
                    actual: ServerEpoch::new(1).expect("epoch"),
                },
                "EPOCH_CHANGED",
                410,
                false,
            ),
            (OperationFailure::OpIdReused, "OP_ID_REUSED", 409, false),
            (
                OperationFailure::BaseMismatch {
                    detail: "revision 17 → 18".to_owned(),
                },
                "BASE_MISMATCH",
                409,
                false,
            ),
            (
                OperationFailure::SubtreeChanged {
                    detail: "子项变化".to_owned(),
                },
                "SUBTREE_CHANGED",
                409,
                false,
            ),
            (
                OperationFailure::ReconcileRequired {
                    detail: "外部写入".to_owned(),
                },
                "RECONCILE_REQUIRED",
                409,
                false,
            ),
            (
                OperationFailure::InvalidRequest {
                    detail: "形状".to_owned(),
                },
                "INVALID_REQUEST",
                400,
                false,
            ),
            (
                OperationFailure::InvalidName {
                    detail: "空名称".to_owned(),
                },
                "INVALID_NAME",
                422,
                false,
            ),
            (
                OperationFailure::UnsupportedEntry {
                    detail: "目录".to_owned(),
                },
                "UNSUPPORTED_ENTRY",
                422,
                false,
            ),
            (
                OperationFailure::UnknownOperation,
                "UNSUPPORTED_ENTRY",
                404,
                false,
            ),
            (
                OperationFailure::ExecutorUnavailable,
                EXECUTOR_UNAVAILABLE_CODE,
                503,
                true,
            ),
            (
                OperationFailure::Storage {
                    detail: "坏行".to_owned(),
                },
                "NEEDS_RECOVERY",
                503,
                true,
            ),
            (OperationFailure::NoSpace, "INSUFFICIENT_STORAGE", 507, true),
        ];
        for (failure, code, status, retryable) in cases {
            assert_eq!(failure.design_error_code(), code, "{failure:?}");
            assert_eq!(failure.http_status(), status, "{failure:?}");
            assert_eq!(failure.is_retryable(), retryable, "{failure:?}");
        }
        // 未装配执行器的码不能借用 NEEDS_RECOVERY 的语义。
        assert_ne!(
            OperationFailure::ExecutorUnavailable.design_error_code(),
            OperationFailure::Storage {
                detail: String::new()
            }
            .design_error_code()
        );
    }
}
