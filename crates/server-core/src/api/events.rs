//! `changes` 拉取与 SSE 提示的用例规则（design §9.3、§9.4、I09）。
//!
//! 边界：纯规则，不读库、不读时钟、不做 HTTP。批次读取在 `crates/server-store::changes`，端点与 SSE
//! 流在 `apps/server`；这里决定三件事：
//!
//! - **游标是否还能服务**（[`judge_cursor`]）：`after` 早于保留窗口，或晚于服务端当前头，都是
//!   [`ChangesFailure::CursorExpired`]（410 `CURSOR_EXPIRED`）。**不返回空列表**：空列表会让客户端
//!   以为“已追平”，把丢失的事件永远错过（design §9.4）。`after` 等于窗口下沿是合法的——它要的是
//!   下沿之后的批次，仍然全部保留。
//! - **保留窗口的截止时间**（[`retention_cutoff`]）：事件保留期是协议参数，与 30 天内容历史无关，
//!   首发默认 90 天（design §9.4，D，可调）。
//! - **SSE 何时提示**（[`next_hint`]）：SSE 只说“有新 seq”，不携带事件内容、不是数据来源。只要头与
//!   上次提示的值不同就提示一次（包括头反而变小——服务端被回退时客户端同样需要立刻重新拉取并
//!   走 `CURSOR_EXPIRED` 路径）。提示可以丢、重、乱序，客户端仍凭 `changes` 的游标与 seq 去重修复。

use vaultsyncd_domain::version::{RootSeq, UtcTimestamp};

/// 事件保留期默认值：90 天（design §9.4）。
pub const DEFAULT_EVENT_RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;

/// 一次拉取缺省的批次数。
pub const DEFAULT_CHANGES_LIMIT: u32 = 100;

/// 一次拉取的批次数上限（与 `server-store::changes::MAX_CHANGE_PAGE` 同值，由端点测试核对）。
pub const MAX_CHANGES_LIMIT: u32 = 500;

/// SSE 保活注释的间隔（秒）：代理与负载均衡不会把空闲连接当成死连接。
pub const SSE_KEEPALIVE_SECONDS: u64 = 15;

/// SSE 轮询 `roots.seq` 的默认间隔（毫秒）。
pub const SSE_POLL_INTERVAL_MILLIS: u64 = 1000;

/// 事件保留窗口的截止时间：创建时间早于它的批次视为已出窗口。
#[must_use]
pub fn retention_cutoff(now: UtcTimestamp, retention_seconds: i64) -> UtcTimestamp {
    let seconds = now
        .as_unix_seconds()
        .saturating_sub(retention_seconds.max(0));
    UtcTimestamp::from_unix_seconds(seconds.max(0)).unwrap_or(now)
}

/// 游标不能被服务的原因（进错误信封的 `details.reason`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorProblem {
    /// 游标早于事件保留窗口。
    BeforeRetention {
        /// 窗口下沿：仍保留的最早批次是 `expired_through + 1`。
        expired_through: RootSeq,
    },
    /// 游标晚于服务端当前头：服务端回退（备份恢复）或客户端伪造；不能当作“已追平”。
    AheadOfHead {
        /// 服务端当前头。
        head: RootSeq,
    },
    /// 快照不存在或已过期。
    SnapshotExpired,
}

impl CursorProblem {
    /// `details.reason` 的稳定文本。
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::BeforeRetention { .. } => "before_retention_window",
            Self::AheadOfHead { .. } => "ahead_of_head",
            Self::SnapshotExpired => "snapshot_expired",
        }
    }
}

/// `changes` 与快照翻页的失败。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangesFailure {
    /// 查询参数不合法（游标、分页大小）。
    InvalidRequest {
        /// 面向调用方的原因。
        detail: String,
    },
    /// 游标或快照已失效，客户端必须重启快照或重建（410 `CURSOR_EXPIRED`）。
    CursorExpired(CursorProblem),
    /// 存储故障。
    Storage {
        /// 日志用的细节，不原样回给客户端。
        detail: String,
    },
}

impl ChangesFailure {
    /// HTTP 状态码。
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::InvalidRequest { .. } => 400,
            Self::CursorExpired(_) => 410,
            Self::Storage { .. } => 503,
        }
    }

    /// design §15.4 的错误码文本。
    #[must_use]
    pub const fn design_error_code(&self) -> &'static str {
        match self {
            Self::InvalidRequest { .. } => "INVALID_REQUEST",
            Self::CursorExpired(_) => "CURSOR_EXPIRED",
            Self::Storage { .. } => "NEEDS_RECOVERY",
        }
    }

    /// 同一请求原样重试是否可能成功：失效的游标永远不会自己复活，存储故障可以重试。
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Storage { .. })
    }
}

impl std::fmt::Display for ChangesFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest { detail } => f.write_str(detail),
            Self::CursorExpired(CursorProblem::BeforeRetention { expired_through }) => write!(
                f,
                "游标早于事件保留窗口（仍保留 seq {} 之后的批次）；请重新获取快照",
                expired_through.get()
            ),
            Self::CursorExpired(CursorProblem::AheadOfHead { head }) => write!(
                f,
                "游标晚于服务端当前头 {}；服务端可能已回退，请重新获取快照",
                head.get()
            ),
            Self::CursorExpired(CursorProblem::SnapshotExpired) => {
                f.write_str("快照不存在或已过期；请重新创建快照")
            }
            Self::Storage { .. } => f.write_str("服务端存储不可用"),
        }
    }
}

impl std::error::Error for ChangesFailure {}

/// 判定游标能否被服务。
///
/// `expired_through` 是保留窗口下沿（`<=` 它的批次不再保证可拉取），`head` 是 Root 当前头。
pub fn judge_cursor(
    after: RootSeq,
    head: RootSeq,
    expired_through: RootSeq,
) -> Result<(), ChangesFailure> {
    if after < expired_through {
        return Err(ChangesFailure::CursorExpired(
            CursorProblem::BeforeRetention { expired_through },
        ));
    }
    if after > head {
        return Err(ChangesFailure::CursorExpired(CursorProblem::AheadOfHead {
            head,
        }));
    }
    Ok(())
}

/// 解析并检查 `limit`：缺省取 [`DEFAULT_CHANGES_LIMIT`]，`0` 或超过 [`MAX_CHANGES_LIMIT`] 是 400
/// （不悄悄截断：客户端按返回条数推进游标，截断会让它误判是否还有更多）。
pub fn changes_limit(requested: Option<u64>) -> Result<u32, ChangesFailure> {
    let Some(value) = requested else {
        return Ok(DEFAULT_CHANGES_LIMIT);
    };
    match u32::try_from(value) {
        Ok(limit) if (1..=MAX_CHANGES_LIMIT).contains(&limit) => Ok(limit),
        _ => Err(ChangesFailure::InvalidRequest {
            detail: format!("limit 必须在 1..={MAX_CHANGES_LIMIT} 之内"),
        }),
    }
}

/// 一页 `changes` 的推进信息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageProgress {
    /// 客户端下一次应带的 `after`：本页最后一个批次的 seq；空页时是原游标。
    pub next_after: RootSeq,
    /// 读取时头之后是否还有批次。
    pub has_more: bool,
}

/// 由本页最后一个 seq（空页为 `None`）、原游标与读取时的头算出推进信息。
#[must_use]
pub fn page_progress(after: RootSeq, last: Option<RootSeq>, head: RootSeq) -> PageProgress {
    let next_after = last.unwrap_or(after);
    PageProgress {
        next_after,
        has_more: next_after < head,
    }
}

/// SSE 是否该发一条新提示：头与上次提示不同才发；还没发过时立即发当前头
/// （新连接的客户端因此马上知道要不要拉取）。
#[must_use]
pub fn next_hint(last_sent: Option<RootSeq>, head: RootSeq) -> Option<RootSeq> {
    (last_sent != Some(head)).then_some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(value: u64) -> RootSeq {
        RootSeq::new(value).expect("测试 seq 合法")
    }

    fn at(seconds: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_seconds(seconds).expect("测试时间合法")
    }

    #[test]
    fn cursor_inside_the_retained_window_is_served() {
        // 下沿 5：游标 5 要的是 6 之后的批次，全部保留；游标 = 头 = 已追平，是合法的空页。
        for after in [5, 6, 9, 10] {
            judge_cursor(seq(after), seq(10), seq(5)).expect("窗口内的游标必须被服务");
        }
        judge_cursor(seq(0), seq(0), seq(0)).expect("全新 Root 的零游标合法");
    }

    #[test]
    fn cursor_before_the_window_is_expired_not_empty() {
        let error = judge_cursor(seq(4), seq(10), seq(5)).expect_err("早于下沿必须失效");
        assert_eq!(
            error,
            ChangesFailure::CursorExpired(CursorProblem::BeforeRetention {
                expired_through: seq(5)
            })
        );
        assert_eq!(error.http_status(), 410);
        assert_eq!(error.design_error_code(), "CURSOR_EXPIRED");
        assert!(!error.is_retryable(), "失效游标重试不会复活");
    }

    #[test]
    fn cursor_ahead_of_the_head_is_expired_not_caught_up() {
        let error = judge_cursor(seq(11), seq(10), seq(0)).expect_err("晚于头必须失效");
        assert_eq!(
            error,
            ChangesFailure::CursorExpired(CursorProblem::AheadOfHead { head: seq(10) })
        );
        assert_eq!(error.http_status(), 410);
        assert!(error.to_string().contains("回退"));
    }

    #[test]
    fn retention_cutoff_is_now_minus_the_window_and_never_negative() {
        assert_eq!(
            retention_cutoff(at(10_000_000), DEFAULT_EVENT_RETENTION_SECONDS),
            at(10_000_000 - DEFAULT_EVENT_RETENTION_SECONDS)
        );
        assert_eq!(
            retention_cutoff(at(100), DEFAULT_EVENT_RETENTION_SECONDS),
            at(0)
        );
        assert_eq!(retention_cutoff(at(100), -5), at(100), "负保留期按 0 处理");
    }

    #[test]
    fn limit_defaults_and_rejects_out_of_range_instead_of_clamping() {
        assert_eq!(changes_limit(None), Ok(DEFAULT_CHANGES_LIMIT));
        assert_eq!(changes_limit(Some(1)), Ok(1));
        assert_eq!(
            changes_limit(Some(u64::from(MAX_CHANGES_LIMIT))),
            Ok(MAX_CHANGES_LIMIT)
        );
        for bad in [0, u64::from(MAX_CHANGES_LIMIT) + 1, u64::MAX] {
            let error = changes_limit(Some(bad)).expect_err("越界 limit 必须拒绝");
            assert_eq!(error.http_status(), 400, "limit={bad}");
        }
    }

    #[test]
    fn progress_follows_the_last_batch_and_reports_remaining() {
        let page = page_progress(seq(3), Some(seq(7)), seq(9));
        assert_eq!((page.next_after, page.has_more), (seq(7), true));
        let done = page_progress(seq(3), Some(seq(9)), seq(9));
        assert_eq!((done.next_after, done.has_more), (seq(9), false));
        let empty = page_progress(seq(9), None, seq(9));
        assert_eq!((empty.next_after, empty.has_more), (seq(9), false));
    }

    #[test]
    fn hint_is_sent_on_connect_and_only_when_the_head_changes() {
        assert_eq!(
            next_hint(None, seq(0)),
            Some(seq(0)),
            "新连接立即给出当前头"
        );
        assert_eq!(next_hint(Some(seq(4)), seq(4)), None, "头没变不重复提示");
        assert_eq!(
            next_hint(Some(seq(4)), seq(6)),
            Some(seq(6)),
            "跳号只提示最新头"
        );
        assert_eq!(
            next_hint(Some(seq(6)), seq(2)),
            Some(seq(2)),
            "头变小（服务端回退）同样提示，客户端重新拉取"
        );
    }
}
