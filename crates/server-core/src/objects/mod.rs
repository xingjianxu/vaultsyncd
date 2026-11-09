//! 不可变内容对象（design §11.1—11.2、I12、C37）：内容地址、目录布局与读写规则。
//!
//! 本模块只规定**顺序与判据**，不直接访问文件系统：字节动作全部走
//! [`crate::ports::object_store::ObjectBlobPort`]，平台适配器负责实现。这样“用哪个 syscall
//! 建副本”由平台层决定，“什么样的副本才允许成为对象”由核心决定。
//!
//! 三条规则在这里固定下来：
//!
//! 1. 对象副本必须是新 inode，且不得有其他硬链接名字（design §11.1、C37）；
//! 2. 副本长度与 SHA-256 必须与期望内容一致（design §11.2 步骤 2—3）；
//! 3. 发布后必须按内容地址复核一次，复核不通过不算对象（I12：已提交版本字节不再改变）。
//!
//! 失败一律不返回可提交的结果，并报告暂存副本是否已清理；没有“默认成功”的路径。

use std::fmt;
use std::path::{Path, PathBuf};

use vaultsyncd_domain::ids::VaultId;

pub mod capture;
pub mod error;
pub mod read;

pub use capture::{
    CaptureRequest, CapturedObject, ExpectedContent, ReuseOutcome, capture_object,
    capture_observed_object,
};
pub use error::{
    CaptureFailure, CaptureFailureReason, DiscardOutcome, ReadFailure, ReadFailureReason,
};
pub use read::{ObjectBytes, read_object};

/// 内容地址长度（SHA-256 原始 32 字节）。
pub const DIGEST_BYTES: usize = 32;

/// 内容地址：SHA-256 原始 32 字节，与 `objects.hash`/`versions.object_hash` 同形。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentDigest([u8; DIGEST_BYTES]);

impl ContentDigest {
    /// 由原始 32 字节构造。
    #[must_use]
    pub fn from_bytes(bytes: [u8; DIGEST_BYTES]) -> Self {
        ContentDigest(bytes)
    }

    /// 由 64 位小写十六进制文本解析；其它长度/字符一律拒绝（不接受大小写混写或 `0x` 前缀）。
    ///
    /// # Errors
    ///
    /// 长度不是 64 个字符或含非小写十六进制字符时返回 [`DigestParseError`]。
    pub fn from_hex(text: &str) -> Result<Self, DigestParseError> {
        if text.len() != DIGEST_BYTES * 2 {
            return Err(DigestParseError::WrongLength { found: text.len() });
        }
        let mut bytes = [0u8; DIGEST_BYTES];
        let raw = text.as_bytes();
        for (index, chunk) in raw.chunks_exact(2).enumerate() {
            let high = hex_value(chunk[0]).ok_or(DigestParseError::NotHex)?;
            let low = hex_value(chunk[1]).ok_or(DigestParseError::NotHex)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(ContentDigest(bytes))
    }

    /// 原始字节。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; DIGEST_BYTES] {
        &self.0
    }

    /// 64 位小写十六进制文本。
    #[must_use]
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(DIGEST_BYTES * 2);
        for byte in self.0 {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        text
    }

    /// 前两个十六进制字符（目录 fanout 用）。
    #[must_use]
    pub fn fanout(&self) -> String {
        let hex = self.to_hex();
        hex[..2].to_owned()
    }
}

impl fmt::Debug for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentDigest({})", self.to_hex())
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// 十六进制文本解析错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DigestParseError {
    /// 长度不是 64 个字符。
    WrongLength {
        /// 实际字符数。
        found: usize,
    },
    /// 含非小写十六进制字符。
    NotHex,
}

impl fmt::Display for DigestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DigestParseError::WrongLength { found } => {
                write!(f, "内容地址必须是 64 个十六进制字符，实际 {found} 个")
            }
            DigestParseError::NotHex => f.write_str("内容地址含非小写十六进制字符"),
        }
    }
}

impl std::error::Error for DigestParseError {}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// 暂存文件名不合规。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidStagingName {
    /// 空名字。
    Empty,
    /// 含路径分隔符、NUL 或 `.`/`..` 分量：暂存只能落在对象区 staging 目录内。
    NotASingleName,
}

impl fmt::Display for InvalidStagingName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InvalidStagingName::Empty => f.write_str("暂存名为空"),
            InvalidStagingName::NotASingleName => {
                f.write_str("暂存名必须是单个文件名（不含分隔符、NUL 或 . / ..）")
            }
        }
    }
}

impl std::error::Error for InvalidStagingName {}

/// 内部对象区布局。
///
/// 已发布对象在 `<objects>/<VaultId 32 位 hex>/<摘要前 2 位 hex>/<64 位摘要 hex>`；未发布的
/// 暂存副本在 `<objects>/staging/<名字>`。两者都在对象区（内部状态目录）内，与 Vault 的可变工作
/// 文件不同目录、不同 inode（design §11.1）。32 位身份与 2 位 fanout 形状不同，不会混淆层级。
#[derive(Clone, Debug)]
pub struct ObjectLayout {
    root: PathBuf,
    vault: VaultId,
}

impl ObjectLayout {
    /// 某个 Vault 的对象布局；对象区根由调用方给出（服务端为 `<state>/objects`）。
    #[must_use]
    pub fn for_vault(root: impl Into<PathBuf>, vault: VaultId) -> Self {
        Self {
            root: root.into(),
            vault,
        }
    }

    /// 该 Vault 内一条内容的规范相对路径，同时也是 `objects.storage_path` 的登记值。
    ///
    /// 其它 Vault 的同一摘要得到不同路径，因此物理对象天然按 Vault 隔离。
    #[must_use]
    pub fn storage_path(&self, digest: &ContentDigest) -> PathBuf {
        Self::relative_object_path(self.vault, digest)
    }

    /// 规范相对路径：`<VaultId>/<aa>/<hash>`（相对对象区根）。
    #[must_use]
    pub fn relative_object_path(vault: VaultId, digest: &ContentDigest) -> PathBuf {
        PathBuf::from(vault.to_hex())
            .join(digest.fanout())
            .join(digest.to_hex())
    }

    /// 只接纳精确匹配该 Vault 与摘要的规范路径；拒绝任意路径、大小写与跨 Vault 前缀。
    #[must_use]
    pub fn valid_storage_path(vault: VaultId, digest: &ContentDigest, path: &str) -> bool {
        Self::relative_object_path(vault, digest).to_str() == Some(path)
    }

    /// 对象区根目录（不是 Vault 的工作目录）。
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 这份布局所属的 Vault。
    #[must_use]
    pub fn vault(&self) -> VaultId {
        self.vault
    }

    /// 暂存目录（`<objects>/staging`）。
    #[must_use]
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// 已发布对象的绝对路径。
    #[must_use]
    pub fn object_path(&self, digest: &ContentDigest) -> PathBuf {
        self.root.join(self.storage_path(digest))
    }

    /// 暂存副本路径（`<objects>/staging/<名字>`）。
    ///
    /// # Errors
    ///
    /// 名字不是单个文件名时返回 [`InvalidStagingName`]。
    pub fn staging_path(&self, name: &str) -> Result<PathBuf, InvalidStagingName> {
        Self::validate_staging_name(name)?;
        Ok(self.staging_dir().join(name))
    }

    /// 暂存名必须是单个文件名：暂存只能落在对象区 staging 目录内，不能借 `../` 逃出去。
    ///
    /// # Errors
    ///
    /// 空名字、含 `/`/NUL、是 `.` 或 `..` 时返回 [`InvalidStagingName`]。
    pub fn validate_staging_name(name: &str) -> Result<(), InvalidStagingName> {
        if name.is_empty() {
            return Err(InvalidStagingName::Empty);
        }
        if name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\0')
            || name.contains('\\')
        {
            return Err(InvalidStagingName::NotASingleName);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_HEX: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

    #[test]
    fn digest_hex_round_trips_and_rejects_non_canonical_forms() {
        let digest = ContentDigest::from_hex(SAMPLE_HEX).expect("合法 64 位小写十六进制");
        assert_eq!(digest.to_hex(), SAMPLE_HEX);
        assert_eq!(digest.fanout(), "0a");
        assert_eq!(digest.as_bytes().len(), DIGEST_BYTES);
        assert_eq!(
            ContentDigest::from_hex(&SAMPLE_HEX[..63]),
            Err(DigestParseError::WrongLength { found: 63 })
        );
        assert_eq!(
            ContentDigest::from_hex(&SAMPLE_HEX.to_uppercase()),
            Err(DigestParseError::NotHex)
        );
        assert_eq!(
            ContentDigest::from_hex(&format!("{SAMPLE_HEX}0")),
            Err(DigestParseError::WrongLength { found: 65 })
        );
    }

    #[test]
    fn same_digest_has_distinct_paths_per_vault() {
        let a = VaultId::from_bytes([1; 16]);
        let b = VaultId::from_bytes([2; 16]);
        let digest = ContentDigest::from_hex(SAMPLE_HEX).expect("摘要");
        let a_layout = ObjectLayout::for_vault("/state/objects", a);
        let b_layout = ObjectLayout::for_vault("/state/objects", b);
        assert_eq!(
            a_layout.storage_path(&digest),
            Path::new(&a.to_hex()).join("0a").join(SAMPLE_HEX)
        );
        assert_ne!(a_layout.object_path(&digest), b_layout.object_path(&digest));
        assert!(ObjectLayout::valid_storage_path(
            a,
            &digest,
            a_layout.storage_path(&digest).to_str().unwrap()
        ));
        assert!(!ObjectLayout::valid_storage_path(
            a,
            &digest,
            b_layout.storage_path(&digest).to_str().unwrap()
        ));
        assert!(!ObjectLayout::valid_storage_path(
            a,
            &digest,
            &format!("../{}", a_layout.storage_path(&digest).display())
        ));
        assert!(!ObjectLayout::valid_storage_path(
            a,
            &digest,
            &format!("objects/{}/0a/{SAMPLE_HEX}", a.to_hex())
        ));
        assert!(!ObjectLayout::valid_storage_path(
            a,
            &digest,
            &a_layout
                .storage_path(&digest)
                .to_str()
                .unwrap()
                .to_uppercase()
        ));
    }

    #[test]
    fn layout_puts_objects_and_staging_in_separate_subtrees() {
        let vault = VaultId::from_bytes([3; 16]);
        let digest = ContentDigest::from_hex(SAMPLE_HEX).expect("合法摘要");
        let layout = ObjectLayout::for_vault("/srv/vaultsyncd/objects", vault);
        assert_eq!(
            ObjectLayout::relative_object_path(vault, &digest),
            Path::new(&vault.to_hex()).join("0a").join(SAMPLE_HEX)
        );
        assert_eq!(
            layout.object_path(&digest),
            Path::new("/srv/vaultsyncd/objects")
                .join(vault.to_hex())
                .join("0a")
                .join(SAMPLE_HEX)
        );
        assert_eq!(
            layout.staging_path("cas-123"),
            Ok(PathBuf::from("/srv/vaultsyncd/objects/staging/cas-123"))
        );
        assert_eq!(
            layout.staging_path("../escape"),
            Err(InvalidStagingName::NotASingleName)
        );
        assert_eq!(
            layout.staging_path("."),
            Err(InvalidStagingName::NotASingleName)
        );
        assert_eq!(layout.staging_path(""), Err(InvalidStagingName::Empty));
        assert_eq!(
            ObjectLayout::validate_staging_name("a/b"),
            Err(InvalidStagingName::NotASingleName)
        );
    }
}
