//! 重启后真实内容核对。数据库 clean 只是历史观察，不能生成覆盖/删除/释放授权。
//! 每次破坏性动作（不只启动时）重新读取固定快照并流式计算摘要；未知、占用与读失败均关闭闸门。
pub mod resync;

use serde::{Deserialize, Serialize};
use vaultsyncd_domain::state::Usage;
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_transfer::hash::ContentHasher;
use vaultsyncd_transfer::ports::{PortError, SnapshotSource};

use crate::executor::apply::{ContentObservation, ControlledLocal};

/// 真实文件的可核对证据；不是数据库中的 clean 标记。Absent 也必须由健康根的实际观察证明。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentEvidence {
    /// 已证明不存在；平台替换时仍须在保护内检查未被创建。
    Absent,
    /// 已读取固定内容，并核对读取前后的身份/代际。
    File {
        /// 本地代际，不使用时间戳决胜。
        generation: u64,
        /// 平台稳定文件身份。
        identity: Vec<u8>,
        /// 完整内容 SHA-256。
        digest: [u8; 32],
        /// 完整长度。
        size: u64,
    },
}

impl ContentEvidence {
    /// 是否为特定完整内容（身份与代际仍由替换端口核对）。
    pub fn has_content(&self, digest: &[u8; 32], size: u64) -> bool {
        matches!(self, Self::File { digest: actual, size: length, .. } if actual == digest && *length == size)
    }
}

/// 对不能证明 clean 的完整文件做内容核对；任何错误都保留 unknown，不产生破坏授权。
pub async fn inspect(
    local: &dyn ControlledLocal,
    path: &RelativePath,
) -> Result<ContentEvidence, PortError> {
    match local.observe_content(path).await? {
        ContentObservation::Absent => Ok(ContentEvidence::Absent),
        ContentObservation::File { facts, snapshot } => {
            if facts.path != *path
                || facts.kind != crate::ports::local::LocalEntryKind::File
                || facts.usage != Usage::Idle
                || facts.file_identity.as_ref().is_none_or(Vec::is_empty)
            {
                return Err(PortError::rejected(
                    "LOCAL_IN_USE",
                    "使用状态或文件身份不能证明安全",
                ));
            }
            if facts.size != snapshot.total_size() {
                return Err(changed());
            }
            let digest = hash_snapshot(&*snapshot).await?;
            // 固定快照的哈希不能证明活动路径没变；读取后再次检查。
            match local.observe_content(path).await? {
                ContentObservation::File { facts: after, .. } if facts == after => {
                    Ok(ContentEvidence::File {
                        generation: facts.generation.get(),
                        identity: facts.file_identity.expect("已检查身份"),
                        digest,
                        size: facts.size,
                    })
                }
                _ => Err(changed()),
            }
        }
    }
}

async fn hash_snapshot(snapshot: &dyn SnapshotSource) -> Result<[u8; 32], PortError> {
    let mut hasher = ContentHasher::new();
    let mut offset = 0;
    while offset < snapshot.total_size() {
        let length = (snapshot.total_size() - offset).min(256 * 1024);
        let bytes = snapshot.read(offset, length).await?;
        if bytes.len() as u64 != length {
            return Err(PortError::rejected("HASH_MISMATCH", "核对快照短读"));
        }
        hasher.update(&bytes);
        offset += length;
    }
    Ok(hasher.finish())
}

pub(crate) fn changed() -> PortError {
    PortError::rejected(
        "LOCAL_CHANGED",
        "真实内容、文件身份、代际或基线已变化；保留现场重新规划",
    )
}
