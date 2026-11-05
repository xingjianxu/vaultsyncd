//! wire 长整数的 serde 适配：十进制文本进出，JSON number 一律拒绝。
//!
//! 原因见 `docs/design.md` §7.1、§15.1：服务器与客户端的计数、字节数一旦超过 2^53，
//! JavaScript 的数字会静默丢精度，所以 wire 上只用规范十进制**字符串**（无符号、无前导零，
//! `0` 写作 `"0"`，上限为 SQLite `INTEGER` 的 [`DB_INTEGER_MAX`]）。
//!
//! 这里校验的是 wire 文本本身：`"01"`、`"-1"`、`1`（JSON number）与超出 64 位有符号范围的
//! 文本都是反序列化错误，而不是“先收下再慢慢看”。规则与 [`vaultsyncd_domain::version`] 的规范
//! 十进制一致；两侧的取值矩阵测试（`tests/contract.rs`）保证它们不会分叉。

use core::fmt;

use serde::{Deserialize, Deserializer, Serializer, de};

/// SQLite `INTEGER` 的上限（有符号 64 位），与 `vaultsyncd_domain::version::DB_INTEGER_MAX` 相同。
pub const DB_INTEGER_MAX: u64 = i64::MAX as u64;

/// 十进制 wire 文本的校验错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalTextError {
    /// 空文本。
    Empty,
    /// 负数文本，或以 `-` 开头的文本。
    Negative,
    /// 前导零：规范文本只在值为 0 时写一个 `0`。
    LeadingZero,
    /// 出现非十进制数字字符（`+`、空白、小数点、下划线、指数等）。
    InvalidDigit,
    /// 超过 [`DB_INTEGER_MAX`]。
    AboveDbLimit,
}

impl fmt::Display for DecimalTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("十进制文本为空"),
            Self::Negative => f.write_str("十进制文本不能为负"),
            Self::LeadingZero => f.write_str("十进制文本不带前导零"),
            Self::InvalidDigit => f.write_str("十进制文本只能由数字组成"),
            Self::AboveDbLimit => write!(f, "十进制文本超过 {DB_INTEGER_MAX}"),
        }
    }
}

impl core::error::Error for DecimalTextError {}

/// 校验规范十进制文本；成功时返回其数值（wire 仍保留文本形式）。
pub fn validate(text: &str) -> Result<u64, DecimalTextError> {
    let bytes = text.as_bytes();
    let Some(first) = bytes.first() else {
        return Err(DecimalTextError::Empty);
    };
    if *first == b'-' {
        return Err(DecimalTextError::Negative);
    }
    if bytes.len() > 1 && *first == b'0' {
        return Err(DecimalTextError::LeadingZero);
    }
    let mut value: u64 = 0;
    for byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => u64::from(byte - b'0'),
            _ => return Err(DecimalTextError::InvalidDigit),
        };
        value = value
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(digit))
            .ok_or(DecimalTextError::AboveDbLimit)?;
        if value > DB_INTEGER_MAX {
            return Err(DecimalTextError::AboveDbLimit);
        }
    }
    Ok(value)
}

/// 反序列化：只接受合法文本，JSON number/布尔/null 都不是“差不多能用”的替代。
pub fn deserialize<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    validate(&text).map_err(de::Error::custom)?;
    Ok(text)
}

/// 序列化：写之前再校验一次，保证本 crate 不会产出自己拒绝接受的 wire 文本。
pub fn serialize<S>(value: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match validate(value) {
        Ok(_) => serializer.serialize_str(value),
        Err(error) => Err(serde::ser::Error::custom(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{DB_INTEGER_MAX, DecimalTextError, validate};

    #[test]
    fn accepts_canonical_text() {
        assert_eq!(validate("0"), Ok(0));
        assert_eq!(validate("42"), Ok(42));
        assert_eq!(validate("9007199254740993"), Ok(9_007_199_254_740_993));
        assert_eq!(validate("9223372036854775807"), Ok(DB_INTEGER_MAX));
    }

    #[test]
    fn rejects_non_canonical_text() {
        assert_eq!(validate(""), Err(DecimalTextError::Empty));
        assert_eq!(validate("-1"), Err(DecimalTextError::Negative));
        assert_eq!(validate("01"), Err(DecimalTextError::LeadingZero));
        assert_eq!(validate("+1"), Err(DecimalTextError::InvalidDigit));
        assert_eq!(validate(" 1"), Err(DecimalTextError::InvalidDigit));
        assert_eq!(validate("1.0"), Err(DecimalTextError::InvalidDigit));
        assert_eq!(validate("1_000"), Err(DecimalTextError::InvalidDigit));
        assert_eq!(validate("1e3"), Err(DecimalTextError::InvalidDigit));
        assert_eq!(
            validate("9223372036854775808"),
            Err(DecimalTextError::AboveDbLimit)
        );
        assert_eq!(
            validate("99999999999999999999999999999999999999"),
            Err(DecimalTextError::AboveDbLimit)
        );
    }
}
