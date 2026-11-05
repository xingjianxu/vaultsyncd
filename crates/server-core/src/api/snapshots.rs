//! 一致性快照端点的用例规则（design §9.3、§15.2）。
//!
//! 边界：纯规则。清单的生成与翻页在 `crates/server-store::snapshots`，HTTP 在 `apps/server`。
//!
//! 规则：快照是**短期只读清单**——创建时固定 `snapshot_id` 与 `high_water_seq`，之后翻页只读清单，
//! 与翻页期间的写入无关；清单过期后翻页是明确的 410（[`ChangesFailure::CursorExpired`] 的
//! `SnapshotExpired`），客户端重启快照，而不是得到空页或错乱数据。分页大小有上限，越界是 400，
//! 不悄悄截断。

use vaultsyncd_domain::version::UtcTimestamp;

use super::events::ChangesFailure;

/// 快照清单的存活时间（秒）。客户端通常几分钟内翻完；翻不完就重启快照。
pub const SNAPSHOT_TTL_SECONDS: i64 = 600;

/// 一页缺省的条目数。
pub const DEFAULT_SNAPSHOT_PAGE: u32 = 500;

/// 一页条目数上限（与 `server-store::snapshots::MAX_SNAPSHOT_PAGE` 同值，由端点测试核对）。
pub const MAX_SNAPSHOT_PAGE: u32 = 1000;

/// 快照的过期时间点：创建时间 + [`SNAPSHOT_TTL_SECONDS`]。
#[must_use]
pub fn snapshot_expiry(created_at: UtcTimestamp) -> UtcTimestamp {
    let seconds = created_at
        .as_unix_seconds()
        .saturating_add(SNAPSHOT_TTL_SECONDS);
    UtcTimestamp::from_unix_seconds(seconds).unwrap_or(created_at)
}

/// 解析并检查快照翻页的 `limit`：缺省取 [`DEFAULT_SNAPSHOT_PAGE`]，`0` 或越界是 400。
pub fn snapshot_limit(requested: Option<u64>) -> Result<u32, ChangesFailure> {
    let Some(value) = requested else {
        return Ok(DEFAULT_SNAPSHOT_PAGE);
    };
    match u32::try_from(value) {
        Ok(limit) if (1..=MAX_SNAPSHOT_PAGE).contains(&limit) => Ok(limit),
        _ => Err(ChangesFailure::InvalidRequest {
            detail: format!("limit 必须在 1..={MAX_SNAPSHOT_PAGE} 之内"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_is_creation_plus_the_ttl() {
        let created = UtcTimestamp::from_unix_seconds(1_000).expect("时间");
        assert_eq!(
            snapshot_expiry(created).as_unix_seconds(),
            1_000 + SNAPSHOT_TTL_SECONDS
        );
    }

    #[test]
    fn limit_defaults_and_rejects_out_of_range_instead_of_clamping() {
        assert_eq!(snapshot_limit(None), Ok(DEFAULT_SNAPSHOT_PAGE));
        assert_eq!(snapshot_limit(Some(1)), Ok(1));
        assert_eq!(
            snapshot_limit(Some(u64::from(MAX_SNAPSHOT_PAGE))),
            Ok(MAX_SNAPSHOT_PAGE)
        );
        for bad in [0, u64::from(MAX_SNAPSHOT_PAGE) + 1, u64::MAX] {
            assert_eq!(
                snapshot_limit(Some(bad))
                    .expect_err("越界必须拒绝")
                    .http_status(),
                400,
                "limit={bad}"
            );
        }
    }
}
