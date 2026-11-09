//! 本服务自己的写入：按**身份与意图**核对，不按时间窗口忽略（design §12.1、§12.2、T27）。
//!
//! 服务把内容发布到 Vault 里的某个路径之前，先登记一条 [`OwnWriteIntent`]：目标路径、将要落地的文件
//! 身份（设备与 inode，暂存文件在 `renameat2` 之后保持不变）以及内容（长度与 SHA-256）。观察器随后在
//! 该路径看到文件时，**只有同时满足“inode 相同且内容摘要相同”才认定是本服务的写入**，跳过采纳；
//! 其余一律按外部变化处理：
//!
//! - 路径上是别的 inode（用户的 rename-over、或意图发布之前就存在的旧文件）→ [`IntentVerdict::NotOurs`]，
//!   意图保留（服务的发布可能还没发生）；
//! - 同一个 inode 但内容已不同（用户在服务写完之后原地改了它）→ [`IntentVerdict::Modified`]，意图作废，
//!   这是一次真实的外部修改；
//! - 同一窗口内的用户修改因此不会被吞：判定只看身份与内容，不看“多久之前”。
//!
//! 意图由服务在提交完成（或放弃）后显式释放；账本有容量上限，满了拒绝登记（服务应暂缓该写入），不
//! 淘汰旧意图——淘汰会让自己的写入被当成外部采纳。

use std::collections::BTreeMap;
use std::fmt;

use crate::objects::ExpectedContent;
use crate::ports::work_tree::FileStamp;

/// 服务即将（或刚刚）发布到某路径的文件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnWriteIntent {
    /// 目标文件所在设备。
    pub device: u64,
    /// 目标文件的 inode（暂存文件的 inode，重命名后不变）。
    pub inode: u64,
    /// 将要落地的内容。
    pub content: ExpectedContent,
}

/// 登记失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentError {
    /// 账本已满：该写入应暂缓，不得淘汰别的意图。
    LedgerFull {
        /// 容量。
        limit: usize,
    },
    /// 该路径已有未释放的意图：同一路径的服务写入必须串行。
    AlreadyExpected,
}

impl fmt::Display for IntentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntentError::LedgerFull { limit } => write!(f, "自写意图账本已满（上限 {limit}）"),
            IntentError::AlreadyExpected => f.write_str("该路径已有未释放的自写意图"),
        }
    }
}

impl std::error::Error for IntentError {}

/// 对一个观察到的文件的判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentVerdict {
    /// 没有意图，或文件身份不同：按外部变化处理，意图（若有）保留。
    NotOurs,
    /// inode 与内容都与意图一致：是本服务的写入。
    Ours,
    /// inode 相同但内容已不同：外部在服务写入之后改了它，意图作废。
    Modified,
}

struct Slot {
    intent: OwnWriteIntent,
    /// 已核对为“本服务写入”的指纹：指纹不变就不必再核对。
    verified: Option<FileStamp>,
}

/// 自写意图账本。
pub struct OwnWriteLedger {
    limit: usize,
    slots: BTreeMap<String, Slot>,
}

impl OwnWriteLedger {
    /// 默认容量。
    pub const DEFAULT_LIMIT: usize = 4_096;

    /// 以容量建立空账本。
    #[must_use]
    pub fn new(limit: usize) -> Self {
        OwnWriteLedger {
            limit,
            slots: BTreeMap::new(),
        }
    }

    /// 未释放的意图数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// 是否没有未释放的意图。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 登记意图。
    ///
    /// # Errors
    ///
    /// 账本已满或路径已有意图。
    pub fn expect(&mut self, path: &str, intent: OwnWriteIntent) -> Result<(), IntentError> {
        if self.slots.contains_key(path) {
            return Err(IntentError::AlreadyExpected);
        }
        if self.slots.len() >= self.limit {
            return Err(IntentError::LedgerFull { limit: self.limit });
        }
        self.slots.insert(
            path.to_owned(),
            Slot {
                intent,
                verified: None,
            },
        );
        Ok(())
    }

    /// 释放意图（服务提交完成或放弃之后）。返回是否存在。
    pub fn release(&mut self, path: &str) -> bool {
        self.slots.remove(path).is_some()
    }

    /// 该指纹是否已被核对为本服务的写入（指纹未变，不必再采集）。
    #[must_use]
    pub fn is_verified(&self, path: &str, stamp: &FileStamp) -> bool {
        self.slots
            .get(path)
            .and_then(|slot| slot.verified.as_ref())
            .is_some_and(|verified| verified == stamp)
    }

    /// 对采集到的文件下判定。`Modified` 会作废意图；`Ours` 记下已核对的指纹。
    pub fn judge(
        &mut self,
        path: &str,
        stamp: &FileStamp,
        content: &ExpectedContent,
    ) -> IntentVerdict {
        let Some(slot) = self.slots.get_mut(path) else {
            return IntentVerdict::NotOurs;
        };
        let same_file =
            stamp.key.device == slot.intent.device && stamp.key.inode == slot.intent.inode;
        if !same_file {
            return IntentVerdict::NotOurs;
        }
        if *content == slot.intent.content {
            slot.verified = Some(*stamp);
            return IntentVerdict::Ours;
        }
        self.slots.remove(path);
        IntentVerdict::Modified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ContentDigest;
    use crate::ports::object_store::FileKey;

    fn content(seed: u8, bytes: u64) -> ExpectedContent {
        ExpectedContent::new(ContentDigest::from_bytes([seed; 32]), bytes)
    }

    fn stamp(inode: u64, ctime: i64) -> FileStamp {
        FileStamp {
            key: FileKey {
                device: 1,
                inode,
                links: 1,
                bytes: 3,
            },
            btime: None,
            mtime: (ctime, 0),
            ctime: (ctime, 0),
        }
    }

    fn intent() -> OwnWriteIntent {
        OwnWriteIntent {
            device: 1,
            inode: 7,
            content: content(1, 3),
        }
    }

    #[test]
    fn same_inode_and_content_is_ours_and_remembers_the_verified_stamp() {
        let mut ledger = OwnWriteLedger::new(4);
        ledger.expect("a", intent()).expect("登记");
        assert_eq!(
            ledger.judge("a", &stamp(7, 10), &content(1, 3)),
            IntentVerdict::Ours
        );
        assert!(ledger.is_verified("a", &stamp(7, 10)));
        assert!(
            !ledger.is_verified("a", &stamp(7, 11)),
            "指纹变了就要重新核对"
        );
    }

    #[test]
    fn other_inode_is_never_ours_and_keeps_the_intent_for_the_pending_publish() {
        let mut ledger = OwnWriteLedger::new(4);
        ledger.expect("a", intent()).expect("登记");
        assert_eq!(
            ledger.judge("a", &stamp(99, 10), &content(1, 3)),
            IntentVerdict::NotOurs,
            "内容相同但 inode 不同：用户的 rename-over，不能认作自己的"
        );
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn same_inode_with_different_content_is_an_external_edit_and_voids_the_intent() {
        let mut ledger = OwnWriteLedger::new(4);
        ledger.expect("a", intent()).expect("登记");
        assert_eq!(
            ledger.judge("a", &stamp(7, 10), &content(2, 3)),
            IntentVerdict::Modified
        );
        assert!(ledger.is_empty());
    }

    #[test]
    fn ledger_is_bounded_and_never_evicts() {
        let mut ledger = OwnWriteLedger::new(1);
        ledger.expect("a", intent()).expect("登记");
        assert_eq!(
            ledger.expect("b", intent()),
            Err(IntentError::LedgerFull { limit: 1 })
        );
        assert_eq!(
            ledger.expect("a", intent()),
            Err(IntentError::AlreadyExpected)
        );
        assert!(ledger.release("a"));
        assert!(!ledger.release("a"));
        ledger.expect("b", intent()).expect("释放后可登记");
    }
}
