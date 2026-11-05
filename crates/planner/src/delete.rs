//! 删除、恢复冲突与子树前提（`docs/design.md` §10.1 的删除行、§10.3、T11/T12、I04）。
//!
//! 本模块把“删除”的两类输入分成明确结果，两种方向都不丢内容：
//!
//! | 输入 | 结果 |
//! | --- | --- |
//! | 远端仍是基线：本地删除待提交 | [`DeleteOutcome::Upload`]：提交目标修订，目录另带子树 token（design §10.3） |
//! | 远端已提交删除、本地未变 | [`DeleteOutcome::ApplyLocally`]：本地撤掉已删除身份，不再次清理服务端 |
//! | 远端已提交删除、本地已修改或已移走 | [`DeleteOutcome::Preserve`]：先把本地内容提交为恢复冲突条目，再撤掉旧身份（T11） |
//! | 两端删除同一修订 | [`DeleteOutcome::AlreadyDeleted`]：幂等成功，不再次清理 |
//! | 远端在基线之后修改或移动 | [`DeleteOutcome::ConflictRequired`]：旧 Delete 返回需要冲突处理，不删新内容 |
//! | 子树 token 失效或其他前提过期 | [`DeleteOutcome::Replan`]：拉取新事实重新规划（T12 的 `SUBTREE_CHANGED`） |
//!
//! 三条判定立场：
//!
//! 1. **删除依据是身份 + 修订，不是路径**：同一 `EntryId` 移动后仍是该条目，但删除命令必须携带它当时的
//!    修订；远端修订已经不是基线修订时，本卡不静默换成新修订，而是给出重规划或冲突结果。
//! 2. **目录删除必须带子树 token**：token 由子树任一变更更新，失效时不能把并发新增的子项一起删掉
//!    （design §10.3、T12）；文件带 token 同样是调用方错误，两者都返回明确错误而不是忽略。
//! 3. **本地内容先保住再撤身份**：远端已提交删除而本地仍有不同内容时，恢复冲突条目必须先提交
//!    （[`Dependency::ConflictCopyCommitted`]），主计划才允许撤掉本地旧身份（I02、T11）。
//!
//! 本模块不做 I/O：不枚举子树、不读时钟、不分配身份。目录子树的保留（另一端移动了整个目录）需要目录级
//! 计划，本卡只给重规划结果，不假装能用一个文件副本表达。

use core::fmt;

use vaultsyncd_domain::ids::EntryId;
use vaultsyncd_domain::version::Revision;

use crate::conflict::{
    CommittedConflict, ConflictCopy, ConflictNameError, ConflictNameRequest, conflict_entry_path,
    create_copy_plan,
};
use crate::reconcile::{
    Action, ContentFingerprint, Dependency, EntryKind, LocalFact, LocalProtection, Plan,
    Prerequisite, ServerFact,
};

/// 服务端生成的子树 token（design §10.3）：目录删除的并发前提，子树任一变更都会更新它。
///
/// 不透明值：planner 只比较相等，不从路径、时间或内容推导，也不解释它的内部结构。首发固定 16 字节，
/// 与服务端分配的其它身份同宽；长度变化属于协议契约变化，需要同步 `api/` 与迁移。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubtreeToken([u8; 16]);

impl SubtreeToken {
    /// 由服务端给出的 16 字节构造。
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 原始字节；wire 编码由传输层决定。
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// 服务端当前的删除状态：两种状态对应的裁决方向不同，因此不合并成 `Option`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerDeletion<'a> {
    /// 条目仍存在（`ServerFact::revision` 是当前提交修订）。
    Present(&'a ServerFact),
    /// 已提交删除；`at_revision` 是删除所依据的修订，用于幂等判据（design §10.1“两端删除同一 Revision”）。
    Deleted {
        /// 删除提交时所依据的修订。
        at_revision: Revision,
    },
}

/// 恢复冲突条目所需的稳定身份与命名（远端已提交删除、本地仍有不同内容时必需）。
///
/// 复用了内容冲突的身份与命名规则（[`crate::conflict`]）：恢复条目同样是“带来源元数据的普通可同步
/// 条目”，幂等依据是 `OpId` 而不是文件名（design §10.1 末行、§10.2）。
#[derive(Clone, Copy, Debug)]
pub struct PreservedContent<'a> {
    /// 服务端为本次操作首次分配的恢复条目身份；重试必须沿用同一值（I06）。
    pub entry_id: EntryId,
    /// 冲突名称的组成输入（`OpId`、设备短标识、服务器时间、预算）。
    pub naming: ConflictNameRequest<'a>,
    /// 同一 `OpId` 已记录的恢复条目；重试时传入，首次提交为 `None`。
    pub committed: Option<&'a CommittedConflict>,
}

/// 删除规划的输入。
#[derive(Clone, Copy, Debug)]
pub struct DeleteRequest<'a> {
    /// 删除意图所依据的基线事实（身份、修订、类型与内容）。
    pub base: &'a ServerFact,
    /// 服务端当前状态：仍存在，或已提交删除。
    pub server: ServerDeletion<'a>,
    /// 本地当前观察；`None` 表示本地已无该条目（本地删除已发生）。
    pub local: Option<&'a LocalFact>,
    /// 目录删除的子树 token；文件删除必须为 `None`（design §10.3）。
    pub subtree: Option<SubtreeToken>,
    /// 提交前观察：子树在扫描之后被改动，删除意图携带的 token 已失效（T12）。
    pub subtree_changed: bool,
    /// 远端已提交删除而本地仍有不同内容时，恢复冲突条目所需的身份与命名。
    pub preserve: Option<&'a PreservedContent<'a>>,
}

/// 需要重新规划的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteReplanReason {
    /// 目录子树在删除期间被改动：token 失效，不能把并发新增一起删掉（T12、`SUBTREE_CHANGED`）。
    SubtreeChanged,
    /// 远端修订已不是删除依据的修订，且位置/内容没有给出更具体的冲突形态：拉取新事实重新规划。
    BaseRevisionMismatch,
    /// 本地在扫描之后又改过该条目：删除意图已过期，先重新规划（不静默删除新内容）。
    LocalChanged,
    /// 远端该身份已被替换：原条目被删除后在原路径新建属于新身份（I03）。
    IdentityReplaced,
    /// 远端已提交删除而本地仍有整个目录子树：保留子树需要目录级计划，本卡只给重规划结果。
    DirectoryPreserveRequired,
}

/// 保留本地内容的原因（design §10.1“一端删除、另一端修改/仅移动”）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreservedReason {
    /// 本地内容相对基线已修改：修改内容创建恢复冲突条目（T11）。
    Modified,
    /// 本地只把条目移到了别处：不用移动意图复活已删除身份，内容另存为恢复条目。
    Moved,
}

/// 旧删除需要冲突处理的原因（不删除新内容）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictReason {
    /// 远端在基线之后修改了内容：旧 Delete 不得删除新内容（design §10.1）。
    ContentModified,
    /// 远端在基线之后移动到别处：旧 Delete 依据的位置已不成立，交冲突处理。
    Moved,
}

/// “先提交恢复冲突条目，再撤掉已删除身份”的计划（design §10.1、T11）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreservePlan {
    /// 主条目：撤掉已删除身份在本地的绑定，依赖里的恢复条目必须先提交。
    pub main: Plan,
    /// 恢复冲突条目：新条目，或同一 `OpId` 的既有结果。
    pub preserve: ConflictCopy,
    /// 保留原因。
    pub reason: PreservedReason,
}

/// 删除规划的完整结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// 本地删除待提交：携带基线修订与（目录）子树 token。
    Upload(Plan),
    /// 远端已提交删除、本地内容未变：本地撤掉该身份并推进基线。
    ApplyLocally(Plan),
    /// 远端已提交删除、本地仍有不同内容：先提交恢复冲突条目。
    Preserve(PreservePlan),
    /// 幂等成功：同一修订的删除已生效、本地已无内容，不再次清理。
    AlreadyDeleted,
    /// 旧删除需要冲突处理：不生成任何删除动作，本地内容保持原样。
    ConflictRequired(ConflictReason),
    /// 前提失效：按新事实重新规划，不生成可执行动作。
    Replan(DeleteReplanReason),
}

/// 删除规划被拒绝的原因：这些输入没有可执行语义，不能靠“忽略参数”继续。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteError {
    /// 目录删除没有携带子树 token（design §10.3 要求目录删除必须带 token）。
    MissingSubtreeToken,
    /// 文件删除携带了子树 token：调用方把文件当目录，删除语义不成立。
    UnexpectedSubtreeToken,
    /// 远端已提交删除，但本地仍有不同内容，而调用方没有给出恢复条目的身份与命名。
    PreserveSettingsMissing,
    /// 同一 `OpId` 已记录的恢复条目与本次请求不同（身份、操作或内容不一致）：按 I06 拒绝。
    OpIdRequestChanged,
    /// 已提交删除所依据的修订早于基线修订：不可能发生的事实组合，不猜测。
    InconsistentFacts,
    /// 恢复条目的冲突名称无法在预算内生成。
    Name(ConflictNameError),
}

impl fmt::Display for DeleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSubtreeToken => f.write_str("目录删除必须携带服务端子树 token"),
            Self::UnexpectedSubtreeToken => f.write_str("文件删除不应携带子树 token"),
            Self::PreserveSettingsMissing => {
                f.write_str("远端已提交删除且本地仍有不同内容，缺少恢复条目身份与命名")
            }
            Self::OpIdRequestChanged => {
                f.write_str("同一 OpId 已记录的恢复条目与本次请求不同，按幂等规则拒绝")
            }
            Self::InconsistentFacts => f.write_str("已提交删除的修订早于基线修订，事实不一致"),
            Self::Name(error) => write!(f, "恢复条目名称生成失败：{error}"),
        }
    }
}

impl core::error::Error for DeleteError {}

/// 规划一次删除（design §10.1 删除行、§10.3）。
///
/// # Errors
///
/// 见 [`DeleteError`]：目录删除缺 token、文件删除带 token、缺少恢复条目输入、同一 `OpId` 的请求不一致、
/// 事实自相矛盾，或恢复条目名称无法在预算内生成。
pub fn plan_delete(request: &DeleteRequest<'_>) -> Result<DeleteOutcome, DeleteError> {
    let DeleteRequest {
        base,
        server,
        local,
        subtree,
        subtree_changed,
        preserve,
    } = *request;

    match server {
        ServerDeletion::Present(remote) => {
            plan_pending_delete(base, remote, local, subtree, subtree_changed)
        }
        ServerDeletion::Deleted { at_revision } => {
            plan_committed_delete(base, at_revision, local, preserve)
        }
    }
}

/// 远端仍是基线：本地删除待提交，或输入已过期。
fn plan_pending_delete(
    base: &ServerFact,
    remote: &ServerFact,
    local: Option<&LocalFact>,
    subtree: Option<SubtreeToken>,
    subtree_changed: bool,
) -> Result<DeleteOutcome, DeleteError> {
    if remote.entry_id != base.entry_id {
        return Ok(DeleteOutcome::Replan(DeleteReplanReason::IdentityReplaced));
    }
    if !same_type(remote.kind, base.kind) {
        // 类型被替换（文件↔目录）不是本卡裁决的删除形态。
        return Ok(DeleteOutcome::Replan(
            DeleteReplanReason::BaseRevisionMismatch,
        ));
    }
    if remote.revision != base.revision {
        // 远端在基线之后变了：先认出更具体的冲突形态，再退回一般的重规划。
        if remote.kind != base.kind {
            return Ok(DeleteOutcome::ConflictRequired(
                ConflictReason::ContentModified,
            ));
        }
        if remote.path != base.path {
            return Ok(DeleteOutcome::ConflictRequired(ConflictReason::Moved));
        }
        return Ok(DeleteOutcome::Replan(
            DeleteReplanReason::BaseRevisionMismatch,
        ));
    }

    // 修订仍是基线修订：检查本地删除意图是否还新鲜。
    if let Some(local) = local
        && (local.path != base.path || local.kind != base.kind)
    {
        return Ok(DeleteOutcome::Replan(DeleteReplanReason::LocalChanged));
    }

    match (matches!(base.kind, EntryKind::Directory), subtree) {
        (true, None) => return Err(DeleteError::MissingSubtreeToken),
        (false, Some(_)) => return Err(DeleteError::UnexpectedSubtreeToken),
        _ => {}
    }

    if subtree_changed {
        return Ok(DeleteOutcome::Replan(DeleteReplanReason::SubtreeChanged));
    }

    let mut prerequisites = vec![Prerequisite::BaseRevisionCurrent {
        expected: base.revision,
    }];
    if let Some(local) = local {
        prerequisites.push(Prerequisite::GenerationUnchanged {
            expected: local.generation,
        });
    }
    Ok(DeleteOutcome::Upload(Plan {
        path: base.path.clone(),
        action: Action::DeleteEntry {
            entry_id: base.entry_id,
            base_revision: base.revision,
            subtree,
        },
        prerequisites,
        dependencies: Vec::new(),
        local_protection: LocalProtection::KeepLocalContent,
    }))
}

/// 远端已提交删除：本地应用，必要时先保留本地不同内容。
fn plan_committed_delete(
    base: &ServerFact,
    at_revision: Revision,
    local: Option<&LocalFact>,
    preserve: Option<&PreservedContent<'_>>,
) -> Result<DeleteOutcome, DeleteError> {
    if at_revision < base.revision {
        return Err(DeleteError::InconsistentFacts);
    }
    let Some(local) = local else {
        // 本地也删除了：同一修订的删除已生效，不再次清理（design §10.1）。
        return Ok(DeleteOutcome::AlreadyDeleted);
    };

    let remove = Plan {
        path: base.path.clone(),
        action: Action::ApplyRemoteDelete {
            entry_id: base.entry_id,
            revision: at_revision,
        },
        prerequisites: vec![
            Prerequisite::GenerationUnchanged {
                expected: local.generation,
            },
            Prerequisite::UsagePermitsChange,
        ],
        dependencies: Vec::new(),
        local_protection: LocalProtection::KeepLocalContent,
    };
    if local.path == base.path && local.kind == base.kind {
        return Ok(DeleteOutcome::ApplyLocally(remove));
    }

    // 本地仍有不同内容：修改内容或移动后的内容先成为恢复冲突条目，再撤掉已删除身份。
    let EntryKind::File(content) = local.kind else {
        return Ok(DeleteOutcome::Replan(
            DeleteReplanReason::DirectoryPreserveRequired,
        ));
    };
    let preserve = preserve.ok_or(DeleteError::PreserveSettingsMissing)?;
    let reason = if local.path != base.path {
        PreservedReason::Moved
    } else {
        PreservedReason::Modified
    };
    let copy = preserved_copy(base, local, content, preserve)?;
    let mut main = remove;
    main.dependencies.push(Dependency::ConflictCopyCommitted {
        entry_id: preserve.entry_id,
    });
    Ok(DeleteOutcome::Preserve(PreservePlan {
        main,
        preserve: copy,
        reason,
    }))
}

/// 恢复冲突条目：新条目（目标位置为空）或同一 `OpId` 的既有结果（不造第二份）。
fn preserved_copy(
    base: &ServerFact,
    local: &LocalFact,
    content: ContentFingerprint,
    preserve: &PreservedContent<'_>,
) -> Result<ConflictCopy, DeleteError> {
    if let Some(record) = preserve.committed {
        if record.op_id != preserve.naming.op_id
            || record.entry_id != preserve.entry_id
            || record.content != content
        {
            return Err(DeleteError::OpIdRequestChanged);
        }
        return Ok(ConflictCopy::Reuse(record.clone()));
    }
    // 条目还在原位置：恢复条目按 §10.2 加冲突后缀，不占回被删除名称；本地已移走时保留用户选的位置。
    let path = if local.path == base.path {
        conflict_entry_path(&base.path, &preserve.naming).map_err(DeleteError::Name)?
    } else {
        local.path.clone()
    };
    Ok(ConflictCopy::Create(create_copy_plan(
        path,
        None,
        local,
        preserve.entry_id,
        preserve.naming.op_id,
    )))
}

/// 类型相同（文件/目录）判定：类型互换不归本模块。
fn same_type(left: EntryKind, right: EntryKind) -> bool {
    core::mem::discriminant(&left) == core::mem::discriminant(&right)
}

#[cfg(test)]
mod tests {
    use super::{
        ConflictReason, DeleteError, DeleteOutcome, DeleteReplanReason, DeleteRequest,
        PreservedContent, PreservedReason, ServerDeletion, SubtreeToken, plan_delete,
    };
    use crate::conflict::{
        CommittedConflict, ConflictCopy, ConflictNameRequest, NameBudget, ShortIdLen,
    };
    use crate::reconcile::{
        Action, ContentFingerprint, Dependency, LocalFact, LocalProtection, Prerequisite,
        ServerFact,
    };
    use vaultsyncd_domain::ids::{EntryId, OpId, VersionId};
    use vaultsyncd_domain::version::{Generation, Revision, UtcTimestamp};
    use vaultsyncd_fs_common::path::RelativePath;

    const ENTRY: EntryId = EntryId::from_bytes([0x11; 16]);
    const OTHER_ENTRY: EntryId = EntryId::from_bytes([0x22; 16]);
    const RECOVERY_ENTRY: EntryId = EntryId::from_bytes([0xd0; 16]);
    const VERSION_OLD: VersionId = VersionId::from_bytes([0xa1; 16]);
    const OP: OpId = OpId::from_bytes([0x5a; 16]);
    const OTHER_OP: OpId = OpId::from_bytes([0x5b; 16]);
    const TOKEN: SubtreeToken = SubtreeToken::from_bytes([0x7e; 16]);
    const OTHER_TOKEN: SubtreeToken = SubtreeToken::from_bytes([0x7f; 16]);
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

    fn naming() -> ConflictNameRequest<'static> {
        ConflictNameRequest {
            device_label: DEVICE_LABEL,
            op_id: OP,
            server_time: UtcTimestamp::from_unix_seconds(SERVER_TIME).expect("测试时间在范围内"),
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

    fn local_file(digest: u8) -> LocalFact {
        LocalFact::file(path("docs/report.txt"), content(digest, 120), generation(9))
    }

    fn preserve_settings() -> PreservedContent<'static> {
        PreservedContent {
            entry_id: RECOVERY_ENTRY,
            naming: naming(),
            committed: None,
        }
    }

    #[test]
    fn local_delete_uploads_base_revision_for_a_file() {
        let base = base_file();
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&base),
            local: None,
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        let DeleteOutcome::Upload(plan) = outcome else {
            panic!("本地删除应生成待提交计划");
        };
        assert_eq!(
            plan.action,
            Action::DeleteEntry {
                entry_id: ENTRY,
                base_revision: revision(4),
                subtree: None,
            }
        );
        assert_eq!(
            plan.prerequisites,
            [Prerequisite::BaseRevisionCurrent {
                expected: revision(4)
            }]
        );
        assert!(plan.dependencies.is_empty(), "删除不上传内容");
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
    }

    #[test]
    fn directory_delete_carries_subtree_token() {
        let base = base_dir();
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&base),
            local: None,
            subtree: Some(TOKEN),
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        let DeleteOutcome::Upload(plan) = outcome else {
            panic!("目录删除应生成待提交计划");
        };
        assert_eq!(
            plan.action,
            Action::DeleteEntry {
                entry_id: ENTRY,
                base_revision: revision(4),
                subtree: Some(TOKEN),
            }
        );
    }

    #[test]
    fn subtree_token_forms_are_enforced() {
        let file = base_file();
        let error = plan_delete(&DeleteRequest {
            base: &file,
            server: ServerDeletion::Present(&file),
            local: None,
            subtree: Some(TOKEN),
            subtree_changed: false,
            preserve: None,
        })
        .expect_err("文件删除不应携带 token");
        assert_eq!(error, DeleteError::UnexpectedSubtreeToken);

        let dir = base_dir();
        let error = plan_delete(&DeleteRequest {
            base: &dir,
            server: ServerDeletion::Present(&dir),
            local: None,
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect_err("目录删除必须携带 token");
        assert_eq!(error, DeleteError::MissingSubtreeToken);
    }

    #[test]
    fn stale_subtree_token_replans_instead_of_swallowing_new_children() {
        let base = base_dir();
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&base),
            local: None,
            subtree: Some(OTHER_TOKEN),
            subtree_changed: true,
            preserve: None,
        })
        .expect("可规划");
        assert_eq!(
            outcome,
            DeleteOutcome::Replan(DeleteReplanReason::SubtreeChanged),
            "T12：token 失效时不生成删除动作"
        );
    }

    #[test]
    fn delete_after_committed_modification_requires_conflict_handling() {
        let base = base_file();
        let remote = ServerFact::file(
            path("docs/report.txt"),
            content(0x32, 120),
            ENTRY,
            revision(5),
            VersionId::from_bytes([0xb2; 16]),
        );
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&remote),
            local: Some(&local_file(0x31)),
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        assert_eq!(
            outcome,
            DeleteOutcome::ConflictRequired(ConflictReason::ContentModified),
            "修改已提交时旧 Delete 不删除新内容"
        );
    }

    #[test]
    fn delete_after_committed_move_requires_conflict_handling() {
        let base = base_file();
        let remote = ServerFact::file(
            path("docs/archive/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&remote),
            local: Some(&local_file(0x31)),
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        assert_eq!(
            outcome,
            DeleteOutcome::ConflictRequired(ConflictReason::Moved)
        );
    }

    #[test]
    fn identical_revision_delete_twice_is_idempotent() {
        let base = base_file();
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(4),
            },
            local: None,
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        assert_eq!(outcome, DeleteOutcome::AlreadyDeleted);
    }

    #[test]
    fn committed_delete_removes_unchanged_local_copy() {
        let base = base_file();
        let local = local_file(0x31);
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&local),
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        let DeleteOutcome::ApplyLocally(plan) = outcome else {
            panic!("内容未变时应直接应用远端删除");
        };
        assert_eq!(
            plan.action,
            Action::ApplyRemoteDelete {
                entry_id: ENTRY,
                revision: revision(5),
            }
        );
        assert_eq!(
            plan.prerequisites,
            [
                Prerequisite::GenerationUnchanged {
                    expected: generation(9)
                },
                Prerequisite::UsagePermitsChange,
            ]
        );
        assert_eq!(plan.local_protection, LocalProtection::KeepLocalContent);
    }

    #[test]
    fn committed_delete_preserves_modified_content_as_recovery_entry() {
        let base = base_file();
        let local = local_file(0x33);
        let preserve = preserve_settings();
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&local),
            subtree: None,
            subtree_changed: false,
            preserve: Some(&preserve),
        })
        .expect("可规划");
        let DeleteOutcome::Preserve(plan) = outcome else {
            panic!("T11：修改内容必须保留");
        };
        assert_eq!(plan.reason, PreservedReason::Modified);
        assert_eq!(
            plan.main.action,
            Action::ApplyRemoteDelete {
                entry_id: ENTRY,
                revision: revision(5),
            }
        );
        assert_eq!(
            plan.main.dependencies,
            [Dependency::ConflictCopyCommitted {
                entry_id: RECOVERY_ENTRY
            }],
            "恢复条目提交后才能撤掉旧身份"
        );
        let ConflictCopy::Create(copy) = plan.preserve else {
            panic!("首次提交必须创建恢复条目");
        };
        assert_eq!(copy.path, path(&format!("docs/{EXPECTED_NAME}")));
        assert_eq!(
            copy.action,
            Action::CreateConflictCopy {
                entry_id: RECOVERY_ENTRY,
                op_id: OP,
                generation: generation(9),
            }
        );
        assert_eq!(
            copy.prerequisites,
            [
                Prerequisite::GenerationUnchanged {
                    expected: generation(9)
                },
                Prerequisite::TargetPathUsable,
                Prerequisite::ServerPathVacant,
            ]
        );
        assert_eq!(
            copy.local_protection,
            LocalProtection::UploadSnapshot {
                generation: generation(9)
            }
        );
    }

    #[test]
    fn committed_delete_keeps_content_the_user_moved_away() {
        let base = base_file();
        let moved = LocalFact::file(
            path("docs/archive/report.txt"),
            content(0x33, 120),
            generation(9),
        );
        let preserve = preserve_settings();
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&moved),
            subtree: None,
            subtree_changed: false,
            preserve: Some(&preserve),
        })
        .expect("可规划");
        let DeleteOutcome::Preserve(plan) = outcome else {
            panic!("移动意图不得复活已删除身份，但内容要保留");
        };
        assert_eq!(plan.reason, PreservedReason::Moved);
        let ConflictCopy::Create(copy) = plan.preserve else {
            panic!("首次提交必须创建恢复条目");
        };
        assert_eq!(
            copy.path,
            path("docs/archive/report.txt"),
            "用户选的位置保留，不占回已删除名称"
        );
    }

    #[test]
    fn committed_delete_of_moved_directory_replans() {
        let base = base_dir();
        let moved = LocalFact::directory(path("docs-old"), generation(9));
        let preserve = preserve_settings();
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&moved),
            subtree: Some(TOKEN),
            subtree_changed: false,
            preserve: Some(&preserve),
        })
        .expect("可规划");
        assert_eq!(
            outcome,
            DeleteOutcome::Replan(DeleteReplanReason::DirectoryPreserveRequired)
        );
    }

    #[test]
    fn stale_local_change_and_base_revision_ask_for_replan() {
        let base = base_file();
        let modified = local_file(0x33);
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&base),
            local: Some(&modified),
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        assert_eq!(
            outcome,
            DeleteOutcome::Replan(DeleteReplanReason::LocalChanged)
        );

        let remote = ServerFact::file(
            path("docs/report.txt"),
            content(0x31, 120),
            ENTRY,
            revision(5),
            VERSION_OLD,
        );
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&remote),
            local: None,
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        assert_eq!(
            outcome,
            DeleteOutcome::Replan(DeleteReplanReason::BaseRevisionMismatch),
            "修订前进但形状未变：先重新规划"
        );

        let replaced = ServerFact::file(
            path("docs/report.txt"),
            content(0x31, 120),
            OTHER_ENTRY,
            revision(4),
            VERSION_OLD,
        );
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Present(&replaced),
            local: None,
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect("可规划");
        assert_eq!(
            outcome,
            DeleteOutcome::Replan(DeleteReplanReason::IdentityReplaced)
        );
    }

    #[test]
    fn missing_recovery_settings_is_an_explicit_error() {
        let base = base_file();
        let local = local_file(0x33);
        let error = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&local),
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect_err("缺恢复条目输入不能默认成功");
        assert_eq!(error, DeleteError::PreserveSettingsMissing);
    }

    #[test]
    fn recovery_entry_retry_reuses_the_committed_identity() {
        let base = base_file();
        let local = local_file(0x33);
        let record = CommittedConflict {
            op_id: OP,
            entry_id: RECOVERY_ENTRY,
            path: path(&format!("docs/{EXPECTED_NAME}")),
            content: content(0x33, 120),
        };
        let preserve = PreservedContent {
            entry_id: RECOVERY_ENTRY,
            naming: naming(),
            committed: Some(&record),
        };
        let outcome = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&local),
            subtree: None,
            subtree_changed: false,
            preserve: Some(&preserve),
        })
        .expect("可规划");
        let DeleteOutcome::Preserve(plan) = outcome else {
            panic!("仍应保留内容");
        };
        assert_eq!(plan.preserve, ConflictCopy::Reuse(record.clone()));

        let changed = PreservedContent {
            entry_id: RECOVERY_ENTRY,
            naming: ConflictNameRequest {
                op_id: OTHER_OP,
                ..naming()
            },
            committed: Some(&record),
        };
        let error = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&local),
            subtree: None,
            subtree_changed: false,
            preserve: Some(&changed),
        })
        .expect_err("同 OpId 不同请求必须拒绝");
        assert_eq!(error, DeleteError::OpIdRequestChanged);
    }

    #[test]
    fn committed_revision_older_than_base_is_inconsistent() {
        let base = base_file();
        let error = plan_delete(&DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(3),
            },
            local: None,
            subtree: None,
            subtree_changed: false,
            preserve: None,
        })
        .expect_err("事实不可能成立");
        assert_eq!(error, DeleteError::InconsistentFacts);
    }

    #[test]
    fn same_input_yields_the_same_plan() {
        let base = base_file();
        let local = local_file(0x33);
        let preserve = preserve_settings();
        let request = DeleteRequest {
            base: &base,
            server: ServerDeletion::Deleted {
                at_revision: revision(5),
            },
            local: Some(&local),
            subtree: None,
            subtree_changed: false,
            preserve: Some(&preserve),
        };
        let first = plan_delete(&request).expect("可规划");
        let second = plan_delete(&request).expect("可规划");
        assert_eq!(first, second);
    }
}
