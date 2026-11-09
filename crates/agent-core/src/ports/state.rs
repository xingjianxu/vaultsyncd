//! 持久状态端口（design §5.5、§7.3、§8.4、§9.3）。
//!
//! 三个端口各管一类事实，互不引用：
//!
//! - [`OperationStore`]：待执行队列（`pending_ops`）。它就是“远端动作的持久意图”：入队发生在观察
//!   到变更之后、执行之前；失败只累加尝试次数并记录错误码，不删除基线、依赖或固定版本；成功出队。
//! - [`IntentStore`]：本地写入意图（`local_intents`）。破坏性本地动作执行**之前**落库，记录路径、
//!   来源、期望摘要与 generation，供重启后辨认“文件已换但数据库未更新”的现场（design §8.4）。
//! - [`UploadResumeStore`]：上传续传记录（`upload_resumes`，S055 迁移 0003）。只保存固定快照与
//!   服务端会话；已收块以服务端 `upload_parts` 为准（design §9.3）。
//!
//! # 不变量
//!
//! - **队列顺序 = 插入顺序**，不使用客户端时钟（I04）：重试用同一个 `OpId`（I06），排序只看插入序。
//! - **失败不丢任务**：`record_failure` 只加次数与错误码；依赖方、基线、固定版本都保留。
//! - **意图先于磁盘改动**：`begin` 必须在平台改动之前成功；同绑定的同一路径最多一条意图
//!   （`UNIQUE (binding_id, target_path)`），因此重复尝试必须复用同一条意图而不是写第二条。
//! - **会话绑在快照上**：上传续传记录的 `snapshot_identity`/`total_size`/`digest` 任一变化都说明
//!   快照换了，旧会话必须丢弃（S054 `SnapshotChanged`），不能拼内容。

use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId};
use vaultsyncd_domain::version::Generation;
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_transfer::hash::DIGEST_BYTES;
use vaultsyncd_transfer::ports::{PortError, PortFuture};
use vaultsyncd_transfer::upload::{UploadResume, UploadSnapshot};

pub use vaultsyncd_client_store::queue::PendingOp as QueuedOperation;

/// 待执行队列端口。
pub trait OperationStore: Send + Sync {
    /// 列出该绑定按插入顺序排列的全部待执行操作。
    fn pending_ops(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Vec<QueuedOperation>, PortError>>;

    /// 记录一次失败：尝试次数 +1 并写入最近错误码，其余字段不变。
    fn record_failure(&self, op: OpId, code: ErrorCode) -> PortFuture<'_, Result<(), PortError>>;

    /// 出队：只应在服务器确认 committed 后调用（pending 不是 committed，I01）。
    fn complete(&self, op: OpId) -> PortFuture<'_, Result<(), PortError>>;
}

/// 本地写入意图的种类（与 `local_intents.kind` 一一对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalWriteKind {
    /// 用暂存内容替换目标文件（下载安装、远端内容落盘）。
    Replace,
    /// 同 Vault 本地移动。
    Move,
    /// 释放本地完整内容。
    Release,
    /// 导入本地新文件到同步树。
    Import,
}

impl LocalWriteKind {
    /// 数据库文本（与 `crates/client-store::intents::LocalIntentKind` 同一拼写）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Move => "move",
            Self::Release => "release",
            Self::Import => "import",
        }
    }
}

/// 一条待持久化的本地写入意图。
///
/// `generation` 是执行前观察到的本地代际：重启恢复时若本地代际更大，说明期间有更新的本地修改，
/// 旧意图不得覆盖它（design §8.4、I02）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionIntent {
    /// 触发这次本地改动的操作身份。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 动作种类。
    pub kind: LocalWriteKind,
    /// 目标条目；本地新文件在提交前没有服务器身份。
    pub entry: Option<EntryId>,
    /// 动作作用的位置（相对绑定根）。
    pub target_path: RelativePath,
    /// 来源位置：替换的暂存文件、移动的旧路径、导入的来源文件。
    pub source_path: Option<String>,
    /// 期望内容摘要（SHA-256 原始字节）。
    pub content_hash: Option<[u8; DIGEST_BYTES]>,
    /// 执行前观察到的本地代际。
    pub generation: Generation,
}

/// 已持久化的一条意图（读回形态，供重启恢复使用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredIntent {
    /// 本地意图序号（插入序，恢复时按它升序重放）。
    pub id: i64,
    /// 所属绑定。
    pub binding: BindingId,
    /// 动作种类。
    pub kind: LocalWriteKind,
    /// 目标条目。
    pub entry: Option<EntryId>,
    /// 动作作用的位置。
    pub target_path: String,
    /// 来源位置。
    pub source_path: Option<String>,
    /// 期望内容摘要。
    pub content_hash: Option<Vec<u8>>,
    /// 执行前观察到的本地代际。
    pub generation: Option<Generation>,
    /// 是否已标记失败（失败保留整条意图）。
    pub failed: bool,
    /// 失败原因（只有失败时出现）。
    pub last_error: Option<ErrorCode>,
}

/// 本地写入意图端口。
pub trait IntentStore: Send + Sync {
    /// 确保该路径的意图已落库，返回意图序号。
    ///
    /// 同一绑定同一路径已有**字段相同**的意图时返回它的序号（重复尝试复用，不写第二条）；字段
    /// 不同则报 `Rejected`（不覆盖另一条意图的期望）。
    fn begin_intent<'a>(
        &'a self,
        intent: &'a ActionIntent,
    ) -> PortFuture<'a, Result<i64, PortError>>;

    /// 标记意图失败并保留现场（路径、摘要、generation 都不删）。
    fn fail_intent(&self, intent: i64, code: ErrorCode) -> PortFuture<'_, Result<bool, PortError>>;

    /// 意图完成后删除（磁盘改动与基线都已更新）。
    fn complete_intent(&self, intent: i64) -> PortFuture<'_, Result<bool, PortError>>;

    /// 列出该绑定尚未完成的意图（按插入序），供重启恢复。
    fn pending_intents(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Vec<StoredIntent>, PortError>>;
}

/// 一条上传续传记录（`upload_resumes` 一行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredUploadResume {
    /// 所属排队操作。
    pub op: OpId,
    /// 所属绑定。
    pub binding: BindingId,
    /// 固定快照的本地身份。
    pub snapshot_identity: String,
    /// 快照总字节数。
    pub total_size: u64,
    /// 整份内容摘要。
    pub digest: [u8; DIGEST_BYTES],
    /// 服务端上传会话身份。
    pub session: String,
}

impl StoredUploadResume {
    /// 把传输执行器的续传结论转成可持久化记录。
    #[must_use]
    pub fn from_transfer(binding: BindingId, op: OpId, resume: &UploadResume) -> Self {
        let snapshot = resume.snapshot();
        Self {
            op,
            binding,
            snapshot_identity: snapshot.identity().to_owned(),
            total_size: snapshot.total_size(),
            digest: snapshot.digest(),
            session: resume.session().to_owned(),
        }
    }

    /// 还原成传输执行器可直接传入的续传信息。
    ///
    /// # Errors
    ///
    /// 数据库里的身份/会话形态不合规（空或含控制字符）时返回 [`PortError::Local`]：坏行必须显式
    /// 暴露，不能被静默忽略成“没有续传记录”。
    pub fn to_transfer(&self) -> Result<UploadResume, PortError> {
        let snapshot =
            UploadSnapshot::new(self.snapshot_identity.clone(), self.total_size, self.digest)
                .map_err(|error| {
                    PortError::local(
                        crate::ports::LocalErrorKind::Read,
                        format!("续传快照身份不合法：{error}"),
                    )
                })?;
        UploadResume::new(snapshot, self.session.clone()).map_err(|error| {
            PortError::local(
                crate::ports::LocalErrorKind::Read,
                format!("续传会话身份不合法：{error}"),
            )
        })
    }
}

/// 上传续传记录端口。
pub trait UploadResumeStore: Send + Sync {
    /// 读取该操作的续传记录；没有返回 `None`。
    fn load_resume(
        &self,
        op: OpId,
    ) -> PortFuture<'_, Result<Option<StoredUploadResume>, PortError>>;

    /// 写入或覆盖续传记录（快照与会话一起写）。
    fn save_resume<'a>(
        &'a self,
        resume: &'a StoredUploadResume,
    ) -> PortFuture<'a, Result<StoredUploadResume, PortError>>;

    /// 丢弃续传记录（放弃会话）；返回是否命中。
    fn discard_resume(&self, op: OpId) -> PortFuture<'_, Result<bool, PortError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> BindingId {
        BindingId::from_bytes([0x11; 16])
    }

    fn op() -> OpId {
        OpId::from_bytes([0x22; 16])
    }

    #[test]
    fn upload_resume_round_trips_through_the_transfer_type() {
        let resume = UploadResume::new(
            UploadSnapshot::new("generation-7", 12, [0x33; DIGEST_BYTES]).expect("快照"),
            "aabb",
        )
        .expect("续传");
        let stored = StoredUploadResume::from_transfer(binding(), op(), &resume);
        assert_eq!(stored.snapshot_identity, "generation-7");
        assert_eq!(stored.total_size, 12);
        assert_eq!(stored.session, "aabb");
        assert_eq!(stored.to_transfer().expect("还原"), resume);
    }

    #[test]
    fn a_malformed_stored_resume_is_an_error_not_an_empty_resume() {
        let stored = StoredUploadResume {
            op: op(),
            binding: binding(),
            snapshot_identity: String::new(),
            total_size: 0,
            digest: [0; DIGEST_BYTES],
            session: "aabb".to_owned(),
        };
        assert!(stored.to_transfer().is_err(), "坏行不能被当成“没有续传”");

        let stored = StoredUploadResume {
            snapshot_identity: "generation-7".to_owned(),
            session: "bad\0session".to_owned(),
            ..stored
        };
        assert!(stored.to_transfer().is_err());
    }

    #[test]
    fn local_write_kinds_use_the_published_database_spelling() {
        assert_eq!(LocalWriteKind::Replace.as_str(), "replace");
        assert_eq!(LocalWriteKind::Move.as_str(), "move");
        assert_eq!(LocalWriteKind::Release.as_str(), "release");
        assert_eq!(LocalWriteKind::Import.as_str(), "import");
        for (kind, text) in [
            (LocalWriteKind::Replace, "replace"),
            (LocalWriteKind::Move, "move"),
            (LocalWriteKind::Release, "release"),
            (LocalWriteKind::Import, "import"),
        ] {
            assert_eq!(
                vaultsyncd_client_store::intents::LocalIntentKind::from_text(text),
                Some(match kind {
                    LocalWriteKind::Replace => {
                        vaultsyncd_client_store::intents::LocalIntentKind::Replace
                    }
                    LocalWriteKind::Move => vaultsyncd_client_store::intents::LocalIntentKind::Move,
                    LocalWriteKind::Release => {
                        vaultsyncd_client_store::intents::LocalIntentKind::Release
                    }
                    LocalWriteKind::Import => {
                        vaultsyncd_client_store::intents::LocalIntentKind::Import
                    }
                })
            );
        }
    }
}
