//! GC 的持久阶段判据；宽限从标记业务时间起算，不读取文件 mtime。
use vaultsyncd_domain::version::UtcTimestamp;

/// 固定最短宽限一小时；磁盘不足不得缩短。
pub const GRACE_SECONDS: i64 = 3600;

/// 标记后的引用复核结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// 新增或仍有有效引用：撤销候选。
    Keep,
    /// 宽限尚未过去（包括时钟回退）。
    Wait,
    /// 可在统一协调下持久化 deleting，再删除字节。
    Delete,
}

/// 满盘不参与判据，不能改变保留窗口或宽限。
pub fn decide(marked_at: UtcTimestamp, now: UtcTimestamp, referenced: bool) -> Decision {
    if referenced {
        Decision::Keep
    } else if now
        .as_unix_seconds()
        .saturating_sub(marked_at.as_unix_seconds())
        < GRACE_SECONDS
    {
        Decision::Wait
    } else {
        Decision::Delete
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(t: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(t).unwrap()
    }
    #[test]
    fn grace_and_new_references_are_mandatory() {
        assert_eq!(decide(at(100), at(99), false), Decision::Wait);
        assert_eq!(decide(at(100), at(3699), false), Decision::Wait);
        assert_eq!(decide(at(100), at(3700), false), Decision::Delete);
        assert_eq!(decide(at(100), at(i64::MAX), true), Decision::Keep);
    }
}
