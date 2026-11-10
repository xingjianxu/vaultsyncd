//! 远端协议纯协商：major不兼容拒绝，minor增量只使用已知且已声明的功能。
use crate::generated::Capabilities;
use std::collections::BTreeSet;
/// 当前远端协议major，与产品和数据库版本独立。
pub const MAJOR: u32 = 2;
/// 当前实现的远端协议minor。
pub const MINOR: u32 = 0;
/// 当前实现认识的功能标记，未知标记不能自动启用动作。
pub const KNOWN_FEATURES: &[&str] = &[
    "auth",
    "vaults",
    "snapshots",
    "changes",
    "events",
    "uploads",
    "content",
    "operations",
    "history",
    "trash",
    "version_leases",
    "vault_admin",
    "backup_snapshots",
    "file_activity",
];
/// 协商失败不能发送登录凭据或数据请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 服务端不属于当前major。
    Major {
        /// 实际主版本。
        received: u32,
    },
    /// 身份、版本、epoch或限额非法。
    Malformed,
    /// 调用依赖的已知功能没有被服务端声明。
    MissingFeature(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}
/// 已经核对的功能交集；不是第二份wire DTO。
#[derive(Debug, Clone)]
pub struct Negotiated {
    features: BTreeSet<&'static str>,
}
impl Negotiated {
    /// 确认功能既已被实现认识，也已被对端显式声明。
    pub fn require(&self, feature: &str) -> Result<(), Error> {
        if self.features.contains(feature) {
            Ok(())
        } else {
            Err(Error::MissingFeature(feature.into()))
        }
    }
    /// 仅返回已认识交集，不能将未知标记当新行为执行。
    pub fn features(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.features.iter().copied()
    }
}
/// 服务端minor可以更高；不要求与产品版本或契约文件摘要相等。
pub fn negotiate(value: &Capabilities) -> Result<Negotiated, Error> {
    if value.protocol.major != MAJOR {
        return Err(Error::Major {
            received: value.protocol.major,
        });
    }
    if value.protocol.minor > u16::MAX as u32
        || crate::domain_map::server_id_from_wire(&value.server_id).is_err()
        || crate::decimal::validate(&value.epoch).is_err()
        || value.limits.max_page_size == 0
        || value.limits.max_page_size > 10000
        || value
            .limits
            .max_upload_part_bytes
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0 && *n <= i64::MAX as u64)
            .is_none()
    {
        return Err(Error::Malformed);
    }
    Ok(Negotiated {
        features: KNOWN_FEATURES
            .iter()
            .copied()
            .filter(|f| value.features.iter().any(|v| v == f))
            .collect(),
    })
}
