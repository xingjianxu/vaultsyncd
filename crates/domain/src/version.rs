//! 有界计数与 UTC 时间（`docs/design.md` §7.1、§7.2）。
//!
//! 服务器与客户端的单调计数在这里是同一个受约束形状：取值 `0..=i64::MAX`，即 SQLite `INTEGER`
//! 的有符号 64 位上限。上限在构造、解析与递增时检查，因此数据库列不会截断，递增也不会回绕。
//! 各计数是不同类型，不能互相赋值：把 `RootSeq` 当成 `Revision` 用必须在编译期失败。
//!
//! **wire 形式固定为十进制字符串**（design §7.1、§15.1）：计数一旦超过 2^53，JavaScript 的数字
//! 会静默丢精度，所以 API/JSON 只传 [`to_decimal`](Revision::to_decimal) 这类文本，不传 JSON
//! number。规范文本不带符号与前导零，`0` 写作 `"0"`；[`from_decimal`](Revision::from_decimal)
//! 只接受规范文本，同一个值因此只有一种写法。数据库读写用
//! [`to_db_integer`](Revision::to_db_integer)/[`from_db_integer`](Revision::from_db_integer)。
//!
//! 本模块不读时钟：当前 UTC 时间由调用方通过 [`Clock`] 注入（design §5.4），时间戳与计数共用
//! 同一套 i64 边界规则。

use core::fmt;
use core::str::FromStr;

/// SQLite `INTEGER` 的上限（有符号 64 位）。所有落库的计数与时间戳不得超过该值。
pub const DB_INTEGER_MAX: u64 = i64::MAX as u64;

/// 有界整数（计数与 UTC 时间戳）的构造、解析与递增错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundError {
    /// 文本为空。
    Empty,
    /// 文本不是纯十进制数字（`+`、空白、小数点、下划线等都不是合法字符）。
    InvalidDigit,
    /// 负数：文本以 `-` 开头，或数据库列里读出负值。
    Negative,
    /// 前导零；规范十进制只在值本身为 0 时写一个 `0`。
    LeadingZero,
    /// 超过 [`DB_INTEGER_MAX`]（或递增到该值之后）。
    AboveDbLimit,
}

impl fmt::Display for BoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("空文本不是合法的计数值"),
            Self::InvalidDigit => f.write_str("计数值只能由十进制数字组成"),
            Self::Negative => f.write_str("计数值不能为负"),
            Self::LeadingZero => f.write_str("计数值的规范文本不带前导零"),
            Self::AboveDbLimit => write!(f, "计数值超过数据库有符号 64 位上限 {DB_INTEGER_MAX}"),
        }
    }
}

impl core::error::Error for BoundError {}

/// 把规范十进制文本解析成 `u64`；范围与格式规则见 [`BoundError`]。
fn parse_decimal(text: &str) -> Result<u64, BoundError> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(BoundError::Empty);
    }
    if bytes[0] == b'-' {
        return Err(BoundError::Negative);
    }
    if bytes.len() > 1 && bytes[0] == b'0' {
        return Err(BoundError::LeadingZero);
    }
    let mut value: u64 = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(BoundError::InvalidDigit);
        }
        value = value
            .checked_mul(10)
            .and_then(|shifted| shifted.checked_add(u64::from(byte - b'0')))
            .ok_or(BoundError::AboveDbLimit)?;
    }
    Ok(value)
}

/// 声明一种有界计数：`0..=DB_INTEGER_MAX`，规范十进制文本，类型之间互不兼容。
///
/// 文档注释由调用方给出，说明该计数在 design 中的分配者与单调性要求。
macro_rules! db_counter {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// 零值：尚未分配任何修订/事件时的初值。
            pub const ZERO: Self = Self(0);

            /// 由 `u64` 构造；超过 [`DB_INTEGER_MAX`] 返回 [`BoundError::AboveDbLimit`]。
            pub fn new(value: u64) -> Result<Self, BoundError> {
                if value > DB_INTEGER_MAX {
                    Err(BoundError::AboveDbLimit)
                } else {
                    Ok(Self(value))
                }
            }

            /// 由数据库 `INTEGER` 列读回；负值返回 [`BoundError::Negative`]。
            pub fn from_db_integer(value: i64) -> Result<Self, BoundError> {
                if value < 0 {
                    Err(BoundError::Negative)
                } else {
                    Ok(Self(value as u64))
                }
            }

            /// 写入数据库 `INTEGER` 列的取值；构造时的上限检查保证不会截断。
            pub fn to_db_integer(self) -> i64 {
                self.0 as i64
            }

            /// 计数值。
            pub fn get(self) -> u64 {
                self.0
            }

            /// 递增 1；已到达 [`DB_INTEGER_MAX`] 时返回 [`BoundError::AboveDbLimit`]，不回绕。
            pub fn next(self) -> Result<Self, BoundError> {
                Self::new(self.0 + 1)
            }

            /// 解析规范十进制文本（wire 形式）；格式与范围规则见 [`BoundError`]。
            pub fn from_decimal(text: &str) -> Result<Self, BoundError> {
                Self::new(parse_decimal(text)?)
            }

            /// 规范十进制文本：无符号、无前导零，也是 wire 上的字符串形式。
            pub fn to_decimal(self) -> String {
                self.to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = BoundError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::from_decimal(text)
            }
        }
    };
}

db_counter! {
    /// 服务器对条目元数据/内容/位置变化分配的递增修订（design §7.1）。
    ///
    /// 只有服务器分配；客户端提交时携带预期基线，过期基线不会被当作最新修订接受。
    Revision
}

db_counter! {
    /// Root 事务事件序号：严格递增，只有服务器分配；外部采集也使用该序列（design §7.1、§7.2）。
    RootSeq
}

db_counter! {
    /// 客户端本地内容代际（design §8.3、§15.3 的 `local_generation`）。
    ///
    /// 本地内容每次成功替换后递增；旧 ACK 只能清除同代际的 dirty 状态（design I02）。
    Generation
}

db_counter! {
    /// 安装实例的恢复代际（`ServerEpoch`）。
    ///
    /// 备份恢复沿用 [`ServerId`](crate::ids::ServerId) 但更换本值；旧 epoch 的待提交操作不自动
    /// 重放，相同 `RootSeq` 在新 epoch 下不是旧事件（design §7.1、§9.4）。
    ServerEpoch
}

/// UTC 时间点：Unix 秒（design §7.1 要求存储 UTC，显示时再转本地时间）。
///
/// 保留期与过期判断只按服务端时间，不使用客户端 mtime；负数（1970 之前）拒绝，避免把
/// “没有时间”与真实时刻混在一起。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcTimestamp(i64);

impl UtcTimestamp {
    /// 由 Unix 秒构造；负值返回 [`BoundError::Negative`]。
    pub fn from_unix_seconds(seconds: i64) -> Result<Self, BoundError> {
        if seconds < 0 {
            Err(BoundError::Negative)
        } else {
            Ok(Self(seconds))
        }
    }

    /// 由数据库 `INTEGER` 列读回；负值返回 [`BoundError::Negative`]。
    pub fn from_db_integer(value: i64) -> Result<Self, BoundError> {
        Self::from_unix_seconds(value)
    }

    /// Unix 秒。
    pub fn as_unix_seconds(self) -> i64 {
        self.0
    }

    /// 写入数据库 `INTEGER` 列的取值；构造时已拒绝负值。
    pub fn to_db_integer(self) -> i64 {
        self.0
    }
}

impl fmt::Display for UtcTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 当前 UTC 时间来源端口（design §5.4：领域层不读时钟）。
///
/// 生产实现读系统时钟；测试实现返回固定或递增序列，使依赖时间的规则可复现。
pub trait Clock {
    /// 当前 UTC 时间点；系统时钟不可信（例如早于 1970）时必须返回错误而不是编造时刻。
    fn now_utc(&self) -> Result<UtcTimestamp, BoundError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对每种计数类型执行同一组检查：零值、边界往返、递增与排序。
    macro_rules! check_counter_type {
        ($ty:ty) => {{
            assert_eq!(<$ty>::ZERO.get(), 0);
            assert_eq!(<$ty>::ZERO.to_decimal(), "0");
            assert_eq!(<$ty>::ZERO.to_db_integer(), 0);
            assert_eq!(<$ty>::from_decimal("0").unwrap(), <$ty>::ZERO);
            assert_eq!("0".parse::<$ty>().unwrap(), <$ty>::ZERO);

            let max = <$ty>::from_decimal("9223372036854775807").unwrap();
            assert_eq!(max.get(), DB_INTEGER_MAX);
            assert_eq!(max.to_decimal(), "9223372036854775807");
            assert_eq!(max.to_string(), "9223372036854775807");
            assert_eq!(max.to_db_integer(), i64::MAX);
            assert_eq!(<$ty>::from_db_integer(i64::MAX).unwrap(), max);
            assert_eq!("9223372036854775807".parse::<$ty>().unwrap(), max);
            assert_eq!(max.next(), Err(BoundError::AboveDbLimit));

            let previous = <$ty>::from_decimal("9223372036854775806").unwrap();
            assert!(max > previous);
            assert_eq!(previous.next().unwrap(), max);
        }};
    }

    #[test]
    fn every_counter_type_round_trips_at_the_database_limit() {
        check_counter_type!(Revision);
        check_counter_type!(RootSeq);
        check_counter_type!(Generation);
        check_counter_type!(ServerEpoch);
    }

    #[test]
    fn wire_text_keeps_precision_beyond_javascript_integers() {
        // 2^53 是 JavaScript 能精确表示的最大连续整数；相邻两个值必须以字符串原样往返。
        let boundary = Revision::from_decimal("9007199254740992").unwrap();
        let beyond = Revision::from_decimal("9007199254740993").unwrap();
        assert_eq!(beyond.to_decimal(), "9007199254740993");
        assert_eq!(boundary.next().unwrap(), beyond);
        assert!(beyond.get() > 9_007_199_254_740_992);
    }

    #[test]
    fn counter_text_rejects_non_canonical_or_out_of_range() {
        let cases = [
            ("", BoundError::Empty),
            ("-1", BoundError::Negative),
            ("+1", BoundError::InvalidDigit),
            (" 17", BoundError::InvalidDigit),
            ("17\n", BoundError::InvalidDigit),
            ("1.0", BoundError::InvalidDigit),
            ("1_000", BoundError::InvalidDigit),
            ("017", BoundError::LeadingZero),
            ("00", BoundError::LeadingZero),
            ("9223372036854775808", BoundError::AboveDbLimit),
            ("18446744073709551616", BoundError::AboveDbLimit),
            ("99999999999999999999999999", BoundError::AboveDbLimit),
        ];
        for (text, expected) in cases {
            assert_eq!(RootSeq::from_decimal(text), Err(expected), "text={text:?}");
            assert_eq!(text.parse::<RootSeq>(), Err(expected), "text={text:?}");
        }
    }

    #[test]
    fn database_integer_bounds_are_checked_on_both_directions() {
        assert_eq!(Revision::from_db_integer(-1), Err(BoundError::Negative));
        assert_eq!(
            Revision::from_db_integer(i64::MIN),
            Err(BoundError::Negative)
        );
        assert_eq!(
            Revision::new(DB_INTEGER_MAX + 1),
            Err(BoundError::AboveDbLimit)
        );
        assert_eq!(Revision::new(u64::MAX), Err(BoundError::AboveDbLimit));
        assert_eq!(
            Revision::new(DB_INTEGER_MAX).unwrap().to_db_integer(),
            i64::MAX
        );
        assert_eq!(
            Revision::from_db_integer(i64::MAX).unwrap().to_db_integer(),
            i64::MAX
        );
    }

    #[test]
    fn counter_next_advances_without_wrapping() {
        let first = RootSeq::ZERO.next().unwrap();
        assert_eq!(first.get(), 1);
        assert_eq!(first.next().unwrap().get(), 2);
        assert_eq!(first.to_decimal(), "1");
    }

    #[test]
    fn bound_error_display_names_the_problem() {
        assert!(BoundError::Empty.to_string().contains("空"));
        assert!(BoundError::InvalidDigit.to_string().contains("十进制"));
        assert!(BoundError::Negative.to_string().contains("负"));
        assert!(BoundError::LeadingZero.to_string().contains("前导零"));
        assert!(
            BoundError::AboveDbLimit
                .to_string()
                .contains("9223372036854775807")
        );
    }

    /// 固定时间来源：注入后依赖时间的判断可复现。
    struct FixedClock(i64);

    impl Clock for FixedClock {
        fn now_utc(&self) -> Result<UtcTimestamp, BoundError> {
            UtcTimestamp::from_unix_seconds(self.0)
        }
    }

    #[test]
    fn clock_injection_yields_deterministic_utc_timestamps() {
        let clock: &dyn Clock = &FixedClock(1_700_000_000);
        let now = clock.now_utc().unwrap();
        assert_eq!(now.as_unix_seconds(), 1_700_000_000);
        assert_eq!(now.to_db_integer(), 1_700_000_000);
        assert_eq!(now.to_string(), "1700000000");
        assert_eq!(clock.now_utc().unwrap(), now);
    }

    #[test]
    fn utc_timestamp_rejects_pre_epoch_values() {
        assert_eq!(
            UtcTimestamp::from_unix_seconds(-1),
            Err(BoundError::Negative)
        );
        assert_eq!(
            UtcTimestamp::from_db_integer(i64::MIN),
            Err(BoundError::Negative)
        );
        assert_eq!(
            UtcTimestamp::from_unix_seconds(0)
                .unwrap()
                .as_unix_seconds(),
            0
        );
        assert_eq!(FixedClock(-1).now_utc(), Err(BoundError::Negative));
    }
}
