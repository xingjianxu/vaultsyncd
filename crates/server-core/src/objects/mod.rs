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

use std::collections::HashSet;
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
/// 已发布对象在 `<vault>/objects/<前 2 位十六进制>/<64 位十六进制>`；未发布的暂存副本在
/// `<vault>/staging/<名字>`。两者都在对象区（内部状态目录）内，与 Vault 的可变工作文件不同
/// 目录、不同 inode（design §11.1）。
#[derive(Clone, Debug)]
pub struct ObjectLayout {
    vault: PathBuf,
    scope: Option<VaultId>,
    legacy: HashSet<ContentDigest>,
}

impl ObjectLayout {
    /// 以对象区根目录建立**未分区**布局：任意摘要都解析成历史全局路径。
    ///
    /// 只用于旧布局测试、迁移工具与独立对象区示例；服务端新写必须用
    /// [`ObjectLayout::for_vault`]，否则不同 Vault 会共用同一物理对象。
    #[must_use]
    pub fn new(vault: impl Into<PathBuf>) -> Self {
        ObjectLayout {
            vault: vault.into(),
            scope: None,
            legacy: HashSet::new(),
        }
    }

    /// Vault 隔离布局；旧路径仅对 `legacy` 中显式声明的摘要开放，不探测磁盘回退。
    ///
    /// **调用方义务**：`legacy` 必须覆盖本次布局将触碰的每个摘要。漏声明不会回退到旧文件，
    /// 而是静默按 Vault 分区的新路径解析：读会失败关闭，发布则先造出重复物理对象，再由
    /// `objects` 的唯一登记约束报 `ObjectConflict`（503）。因此热路径应按摘要解析
    /// （`for_digests`），整树路径才一次读取该 Vault 的全部旧登记。
    #[must_use]
    pub fn for_vault(
        vault: impl Into<PathBuf>,
        scope: VaultId,
        legacy: impl IntoIterator<Item = ContentDigest>,
    ) -> Self {
        Self {
            vault: vault.into(),
            scope: Some(scope),
            legacy: legacy.into_iter().collect(),
        }
    }

    /// 对象区内该 Vault 的规范位置；已有旧记录保持原身份，新对象按 Vault 分区。
    #[must_use]
    pub fn storage_path(&self, digest: &ContentDigest) -> PathBuf {
        match self.scope {
            Some(scope) if !self.legacy.contains(digest) => {
                Self::scoped_relative_object_path(scope, digest)
            }
            _ => Self::relative_object_path(digest),
        }
    }

    /// 对象区里按 Vault 分区的物理前缀。
    ///
    /// 保持 S136/ADR-020 已落地的 `roots/` 目录名：它是磁盘上的数据布局，不是产品术语；
    /// 改名要搬迁全部已发布对象，按 ADR-021 不在纯重命名范围内做。
    pub const SCOPED_PREFIX: &'static str = "roots";

    /// 新对象的全局唯一登记路径，同时也是对象区内的实际物理路径。
    #[must_use]
    pub fn scoped_relative_object_path(scope: VaultId, digest: &ContentDigest) -> PathBuf {
        Path::new(Self::SCOPED_PREFIX)
            .join(scope.to_hex())
            .join(Self::relative_object_path(digest))
    }

    /// 只接纳精确匹配摘要及 Vault 的新路径，或历史规范路径；拒绝任意路径和跨 Vault 前缀。
    #[must_use]
    pub fn valid_storage_path(scope: VaultId, digest: &ContentDigest, path: &str) -> bool {
        Self::relative_object_path(digest).to_str() == Some(path)
            || Self::scoped_relative_object_path(scope, digest).to_str() == Some(path)
    }

    /// 对象区根目录。
    #[must_use]
    pub fn vault(&self) -> &Path {
        &self.vault
    }

    /// 暂存目录（`<vault>/staging`）。
    #[must_use]
    pub fn staging_dir(&self) -> PathBuf {
        self.vault.join("staging")
    }

    /// 已发布对象的绝对路径。
    #[must_use]
    pub fn object_path(&self, digest: &ContentDigest) -> PathBuf {
        self.vault.join(self.storage_path(digest))
    }

    /// 历史全局布局的相对路径（`objects/<aa>/<hex>`，不含对象区根）。
    ///
    /// 只用于已登记旧对象的兼容读取；新对象写 [`ObjectLayout::scoped_relative_object_path`]。
    #[must_use]
    pub fn relative_object_path(digest: &ContentDigest) -> PathBuf {
        Path::new("objects")
            .join(digest.fanout())
            .join(digest.to_hex())
    }

    /// 暂存副本路径（`<vault>/staging/<名字>`）。
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
    fn vault_layout_only_reuses_explicitly_registered_legacy_digests() {
        let a = VaultId::from_bytes([1; 16]);
        let b = VaultId::from_bytes([2; 16]);
        let digest = ContentDigest::from_hex(SAMPLE_HEX).expect("摘要");
        let old = ObjectLayout::for_vault("/state", a, [digest]);
        let new_a = ObjectLayout::for_vault("/state", a, []);
        let new_b = ObjectLayout::for_vault("/state", b, []);
        assert_eq!(
            old.storage_path(&digest),
            ObjectLayout::relative_object_path(&digest)
        );
        assert_ne!(new_a.object_path(&digest), new_b.object_path(&digest));
        assert_ne!(old.object_path(&digest), new_b.object_path(&digest));
        assert!(ObjectLayout::valid_storage_path(
            a,
            &digest,
            old.storage_path(&digest).to_str().unwrap()
        ));
        assert!(ObjectLayout::valid_storage_path(
            a,
            &digest,
            new_a.storage_path(&digest).to_str().unwrap()
        ));
        assert!(!ObjectLayout::valid_storage_path(
            a,
            &digest,
            new_b.storage_path(&digest).to_str().unwrap()
        ));
        assert!(!ObjectLayout::valid_storage_path(
            a,
            &digest,
            &format!("../{}", old.storage_path(&digest).display())
        ));
        assert!(!ObjectLayout::valid_storage_path(
            a,
            &digest,
            &format!(
                "roots/{}/objects/../objects/{}/{}",
                a.to_hex(),
                digest.fanout(),
                digest
            )
        ));
    }

    /// 漏声明旧登记时不会猜回旧文件：同一 Vault 也解析成 Vault 分区路径。
    /// 读会失败关闭；写会先造重复物理对象，再由登记唯一约束报冲突（见 store 用例）。
    #[test]
    fn undeclared_legacy_digest_resolves_to_scoped_path_even_for_same_vault() {
        let vault = VaultId::from_bytes([1; 16]);
        let digest = ContentDigest::from_hex(SAMPLE_HEX).expect("摘要");
        let declared = ObjectLayout::for_vault("/state", vault, [digest]);
        let omitted = ObjectLayout::for_vault("/state", vault, []);
        assert_eq!(
            declared.storage_path(&digest),
            ObjectLayout::relative_object_path(&digest)
        );
        assert_eq!(
            omitted.storage_path(&digest),
            ObjectLayout::scoped_relative_object_path(vault, &digest)
        );
        assert_ne!(declared.object_path(&digest), omitted.object_path(&digest));
        assert_ne!(
            omitted.object_path(&digest),
            Path::new("/state").join(ObjectLayout::relative_object_path(&digest))
        );
    }

    #[test]
    fn layout_puts_objects_and_staging_in_separate_subtrees() {
        let digest = ContentDigest::from_hex(SAMPLE_HEX).expect("合法摘要");
        let layout = ObjectLayout::new("/srv/vaultsyncd/objects");
        assert_eq!(
            ObjectLayout::relative_object_path(&digest),
            Path::new("objects").join("0a").join(SAMPLE_HEX)
        );
        assert_eq!(
            layout.object_path(&digest),
            Path::new("/srv/vaultsyncd/objects/objects/0a").join(SAMPLE_HEX)
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
