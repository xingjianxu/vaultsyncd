//! 流式 SHA-256（design §7.1 `ContentHash`、§13.1 块/整文件校验）。
//!
//! 上传端按块读文件、服务端按块接收字节，都不允许把整份内容读进内存（design §13.3 的“整文件
//! hash 流式计算”）。因此这里给出唯一一份增量摘要实现：调用方喂 `update` 片段，最后一次 `finish`
//! 取 32 字节摘要，比较用 [`verify`]。
//!
//! 摘要以**原始 32 字节**交付，不定义新的命名包装类型：`crates/server-core` 的内容地址
//! （`objects::ContentDigest`）与存储层的 `ObjectHash` 都已经有自己的类型，本模块只提供算法与比较，
//! 免得项目里出现第三份“摘要类型”拼写。

use std::fmt;

use sha2::{Digest, Sha256};

/// SHA-256 摘要长度（原始字节）。
pub const DIGEST_BYTES: usize = 32;

/// 增量 SHA-256 计算器：按片段喂入，最后取摘要。
#[derive(Debug, Default)]
pub struct ContentHasher {
    hasher: Sha256,
    hashed: u64,
}

impl ContentHasher {
    /// 新建一个空的计算器。
    #[must_use]
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            hashed: 0,
        }
    }

    /// 喂入一段字节。
    ///
    /// `hashed` 用 `saturating_add`：它只用于报告“已经算过多少字节”，溢出不能变成错误结论，
    /// 也不能回绕成一个看起来合法的小数字。
    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.hashed = self.hashed.saturating_add(bytes.len() as u64);
    }

    /// 到目前为止喂入的字节数。
    #[must_use]
    pub fn hashed_bytes(&self) -> u64 {
        self.hashed
    }

    /// 结束计算并取出 32 字节摘要。
    #[must_use]
    pub fn finish(self) -> [u8; DIGEST_BYTES] {
        self.hasher.finalize().into()
    }
}

/// 一次性算出一段字节的 SHA-256（只用于长度有界的块，不用于整文件）。
#[must_use]
pub fn digest_of(bytes: &[u8]) -> [u8; DIGEST_BYTES] {
    Sha256::digest(bytes).into()
}

/// 比较实测摘要与期望摘要。
///
/// # Errors
///
/// 两者不同时返回 [`DigestMismatch`]，里面同时带期望与实测摘要，便于诊断（调用方负责决定
/// 对外只报 `HASH_MISMATCH`、不回显内容）。
pub fn verify(
    actual: &[u8; DIGEST_BYTES],
    expected: &[u8; DIGEST_BYTES],
) -> Result<(), DigestMismatch> {
    if actual == expected {
        Ok(())
    } else {
        Err(DigestMismatch {
            expected: *expected,
            actual: *actual,
        })
    }
}

/// 摘要不符。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestMismatch {
    /// 期望的摘要（来自契约/会话）。
    pub expected: [u8; DIGEST_BYTES],
    /// 实测的摘要（来自收到的字节）。
    pub actual: [u8; DIGEST_BYTES],
}

impl fmt::Display for DigestMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "摘要不符：期望 {}，实测 {}",
            to_hex(&self.expected),
            to_hex(&self.actual)
        )
    }
}

impl std::error::Error for DigestMismatch {}

/// 32 字节摘要的小写十六进制文本（日志与错误信息用）。
#[must_use]
pub fn to_hex(digest: &[u8; DIGEST_BYTES]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(DIGEST_BYTES * 2);
    for byte in digest {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST 的公开测试向量（空内容与 "abc"）：算法实现不许被“凭感觉”实现对。
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn empty_input_matches_nist_vector() {
        assert_eq!(to_hex(&ContentHasher::new().finish()), EMPTY);
        assert_eq!(to_hex(&digest_of(b"")), EMPTY);
    }

    #[test]
    fn incremental_updates_equal_one_shot_digest() {
        let mut hasher = ContentHasher::new();
        hasher.update(b"a");
        hasher.update(b"bc");
        assert_eq!(hasher.hashed_bytes(), 3);
        assert_eq!(to_hex(&hasher.finish()), ABC);
        assert_eq!(to_hex(&digest_of(b"abc")), ABC);
    }

    #[test]
    fn verify_reports_both_digests_on_mismatch() {
        let expected = digest_of(b"abc");
        let actual = digest_of(b"abd");
        assert_eq!(verify(&expected, &expected), Ok(()));
        let mismatch = verify(&actual, &expected).expect_err("不同摘要");
        assert_eq!(mismatch.expected, expected);
        assert_eq!(mismatch.actual, actual);
        let message = mismatch.to_string();
        assert!(message.contains(&to_hex(&expected)), "{message}");
        assert!(message.contains(&to_hex(&actual)), "{message}");
    }

    #[test]
    fn hex_is_lowercase_and_fixed_width() {
        let mut padded = [0_u8; DIGEST_BYTES];
        padded[..3].copy_from_slice(&[0x00, 0xff, 0x1a]);
        let hex = to_hex(&padded);
        assert_eq!(hex.len(), DIGEST_BYTES * 2);
        assert_eq!(&hex[..6], "00ff1a");
    }
}
