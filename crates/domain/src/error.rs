//! 错误分类（`docs/design.md` §15.4、§18.2）。
//!
//! 领域层对每个错误只决定一件与恢复有关的事：**谁能让它恢复**。design §15.4 的 wire 错误码因此
//! 落在三个处理类别之一：
//!
//! - [`ErrorClass::Retryable`]：换一个事实基础或等一段时间就能自动推进（重新规划、退避重试）。
//! - [`ErrorClass::NeedsUserAction`]：停止相关操作，等用户或管理员处理（权限、挂载、额度、配置、
//!   历史过期、协议违规）。design §18.2 要求这类失败进入问题页并通知。
//! - [`ErrorClass::Corrupt`]：内容或元数据完整性失败。未通过校验的内容不得发布，只能报告损坏。
//!
//! 分类只由错误码本身决定，是纯函数；重试次数、退避时间与 `Retry-After` 属于调度器
//! （design §13.3），不在本模块建模。一次性的前置动作也由调用方在分类前完成——例如
//! [`ErrorCode::AuthExpired`] 先受控刷新一次，刷新失败后才按“需用户处理”提示重新登录。
//!
//! wire 上的 `retryable` 字段与本分类不矛盾：design §15.4 的示例对 `ROOT_UNAVAILABLE` 写
//! `retryable: true`，本模块仍把它归为 [`ErrorClass::NeedsUserAction`]，因为周期复查不能替代
//! 用户恢复挂载；`retryable` 只说明调度器可以低频重试，不说明错误会自行消失。

use core::fmt;

/// 错误的处理类别，决定界面是否进入问题页以及调度器能否自动重试。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// 可自动推进：换事实基础（重新规划）或退避后重试即可恢复。
    Retryable,
    /// 需用户或管理员处理：停止相关操作并保留本地内容，不盲目自动重试。
    NeedsUserAction,
    /// 完整性失败：不发布内容，报告损坏并保留证据。
    Corrupt,
}

impl ErrorClass {
    /// 是否属于 design §8.2 的“严重失败”，即 design §18.2 要求进入问题页并通知的类别。
    ///
    /// 普通断网重试（[`ErrorClass::Retryable`]）只是状态持续可见，不进入问题页。
    pub const fn is_severe(self) -> bool {
        matches!(self, Self::NeedsUserAction | Self::Corrupt)
    }
}

/// 声明 design §15.4 的错误码：wire 文本、处理类别与文档只有一个来源。
macro_rules! error_codes {
    ($(
        $(#[$meta:meta])*
        $variant:ident => $text:literal : $class:ident,
    )+) => {
        /// design §15.4 的错误码（服务端 wire、首发 Windows 本机错误、后续 Linux 挂载错误）。
        ///
        /// `as_str` 的拼写是协议的一部分：服务端、客户端与界面共享这一处，不另写字符串字面量。
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum ErrorCode {
            $( $(#[$meta])* $variant, )+
        }

        impl ErrorCode {
            /// 全部错误码，顺序与 design §15.4 的表格一致（服务端在前，本机与挂载在后）。
            pub const ALL: &[Self] = &[ $( Self::$variant, )+ ];

            /// wire 上的规范文本（全大写、下划线分隔）。
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $text, )+
                }
            }

            /// 按 wire 文本反查；未知代码返回 `None`，调用方不得把它默认当成可重试。
            pub fn from_code(text: &str) -> Option<Self> {
                match text {
                    $( $text => Some(Self::$variant), )+
                    _ => None,
                }
            }

            /// 处理类别（见模块文档）。
            pub const fn class(self) -> ErrorClass {
                match self {
                    $( Self::$variant => ErrorClass::$class, )+
                }
            }
        }
    };
}

error_codes! {
    /// 401：访问令牌过期，受控刷新一次后失败则提示重新登录。
    AuthExpired => "AUTH_EXPIRED": NeedsUserAction,
    /// 403：无权限；停止相关操作并保留本地内容。
    AccessDenied => "ACCESS_DENIED": NeedsUserAction,
    /// 409：本地基线不是服务器当前版本；拉取最新事实并重新规划。
    BaseMismatch => "BASE_MISMATCH": Retryable,
    /// 409：子树在计划后变化；重新规划，不覆盖服务器的新事实。
    SubtreeChanged => "SUBTREE_CHANGED": Retryable,
    /// 409：同一 `OpId` 对应不同请求；这是协议错误，停止并交给人排查。
    OpIdReused => "OP_ID_REUSED": NeedsUserAction,
    /// 409：等服务端采集外部变化后重新规划，不强制重试覆盖。
    ReconcileRequired => "RECONCILE_REQUIRED": Retryable,
    /// 410：游标已过期；进入保留基线的重建流程。
    CursorExpired => "CURSOR_EXPIRED": Retryable,
    /// 410：服务端 epoch 变化；重建本地事实后继续。
    EpochChanged => "EPOCH_CHANGED": Retryable,
    /// 410：请求的历史版本已被回收；解释历史过期，不用同路径新内容冒充旧版本。
    VersionGone => "VERSION_GONE": NeedsUserAction,
    /// 422：名称不合规；标记该条目，不拖垮整个 Root。
    InvalidName => "INVALID_NAME": NeedsUserAction,
    /// 422：条目不支持的正式范围；标记该条目，不拖垮整个 Root。
    UnsupportedEntry => "UNSUPPORTED_ENTRY": NeedsUserAction,
    /// 422：内容哈希不符；不发布内容，保留证据并报告损坏。
    HashMismatch => "HASH_MISMATCH": Corrupt,
    /// 422：服务器目录的存储后端不受支持；拒绝配置，不尝试回源或转换。
    UnsupportedStorageBackend => "UNSUPPORTED_STORAGE_BACKEND": NeedsUserAction,
    /// 422：服务器目录是占位/第三方 stub；拒绝配置，不尝试回源或转换。
    ServerPlaceholderUnsupported => "SERVER_PLACEHOLDER_UNSUPPORTED": NeedsUserAction,
    /// 429：被限流；按服务器给出的重试时间退避。
    RateLimited => "RATE_LIMITED": Retryable,
    /// 503：Root 当前不可用（如挂载身份改变）；暂停该 Root，通知并保留队列。
    RootUnavailable => "ROOT_UNAVAILABLE": NeedsUserAction,
    /// 503：服务端需要恢复；暂停该 Root，通知并保留队列。
    NeedsRecovery => "NEEDS_RECOVERY": NeedsUserAction,
    /// 507：空间不足；立即通知，保留脏数据并低频复查。
    InsufficientStorage => "INSUFFICIENT_STORAGE": NeedsUserAction,
    /// 本机：本地内容在操作期间变化；重新观察并重新规划。
    LocalChanged => "LOCAL_CHANGED": Retryable,
    /// 本机：文件被其它句柄占用；等占用结束后重试。
    FileBusy => "FILE_BUSY": Retryable,
    /// 本机：当前模式要求本地完整副本；需要用户改变同步模式。
    ModeRequiresLocalCopy => "MODE_REQUIRES_LOCAL_COPY": NeedsUserAction,
    /// 本机：占位符离线不可打开；当次操作即时说明，联网后重试。
    PlaceholderOffline => "PLACEHOLDER_OFFLINE": Retryable,
    /// 本机（后续 Linux 客户端）：挂载失败；必须由用户/管理员恢复挂载。
    MountFailed => "MOUNT_FAILED": NeedsUserAction,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorClass, ErrorCode};

    #[test]
    fn retryable_codes_are_automatic_progress() {
        for code in [
            ErrorCode::BaseMismatch,
            ErrorCode::SubtreeChanged,
            ErrorCode::ReconcileRequired,
            ErrorCode::CursorExpired,
            ErrorCode::EpochChanged,
            ErrorCode::RateLimited,
            ErrorCode::LocalChanged,
            ErrorCode::FileBusy,
            ErrorCode::PlaceholderOffline,
        ] {
            assert_eq!(code.class(), ErrorClass::Retryable, "{code}");
            assert!(!code.class().is_severe(), "{code}");
        }
    }

    #[test]
    fn user_action_codes_stop_and_surface() {
        for code in [
            ErrorCode::AuthExpired,
            ErrorCode::AccessDenied,
            ErrorCode::OpIdReused,
            ErrorCode::VersionGone,
            ErrorCode::InvalidName,
            ErrorCode::UnsupportedEntry,
            ErrorCode::UnsupportedStorageBackend,
            ErrorCode::ServerPlaceholderUnsupported,
            ErrorCode::RootUnavailable,
            ErrorCode::NeedsRecovery,
            ErrorCode::InsufficientStorage,
            ErrorCode::ModeRequiresLocalCopy,
            ErrorCode::MountFailed,
        ] {
            assert_eq!(code.class(), ErrorClass::NeedsUserAction, "{code}");
            assert!(code.class().is_severe(), "{code}");
        }
    }

    #[test]
    fn hash_mismatch_is_the_only_corrupt_class_in_the_design_table() {
        assert_eq!(ErrorCode::HashMismatch.class(), ErrorClass::Corrupt);
        assert!(ErrorCode::HashMismatch.class().is_severe());
        let corrupt = ErrorCode::ALL
            .iter()
            .filter(|code| code.class() == ErrorClass::Corrupt)
            .count();
        assert_eq!(corrupt, 1);
    }

    #[test]
    fn wire_text_round_trips_for_every_code() {
        for &code in ErrorCode::ALL {
            assert_eq!(ErrorCode::from_code(code.as_str()), Some(code));
            assert_eq!(code.to_string(), code.as_str());
            assert_eq!(code.as_str(), code.as_str().to_uppercase());
        }
    }

    #[test]
    fn unknown_codes_are_not_defaulted_to_retryable() {
        assert_eq!(ErrorCode::from_code(""), None);
        assert_eq!(ErrorCode::from_code("SOMETHING_NEW"), None);
        assert_eq!(ErrorCode::from_code("rate_limited"), None);
        assert_eq!(ErrorCode::from_code("RATE_LIMITED "), None);
    }
}
