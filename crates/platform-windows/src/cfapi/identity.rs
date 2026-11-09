//! CFAPI 占位符 `FileIdentity` 的编解码（纯逻辑，可在宿主测试）。
//!
//! `docs/design.md` §14.2 要求 `FileIdentity` 编码 schema_version、VaultId、EntryId 与版本提示，
//! 且不得装入密码、访问令牌或服务器绝对路径。因此本模块的结构里只有定长字节数组与整数：
//!
//! | 偏移 | 长度 | 字段 |
//! | --- | --- | --- |
//! | 0 | 2 | magic `b"VS"` |
//! | 2 | 1 | schema_version（当前 [`SCHEMA_VERSION`]） |
//! | 3 | 1 | 保留字节，必须为 0 |
//! | 4 | 16 | VaultId（不透明） |
//! | 20 | 16 | EntryId（不透明；design I03：路径改变不改变 EntryId） |
//! | 36 | 8 | 版本提示（不透明 u64，小端） |
//! | 44 | 4 | CRC-32/IEEE（覆盖偏移 0..44，小端） |
//!
//! 编码后固定 [`ENCODED_LEN`] 字节，远小于 CFAPI 的 [`CFAPI_MAX_FILE_IDENTITY_LEN`] 上限。
//! 解码对长度、magic、schema_version、保留字节与 CRC-32 全部校验，失败返回
//! [`IdentityDecodeError`] 的明确变体，不做“尽量解析”。

/// CFAPI 对单个占位符 `FileIdentity` 的硬上限（Windows 文档：4096 字节）。
///
/// 本模块的编码必须远小于它：占位符元数据随每个条目写入 NTFS，过大没有收益。
pub const CFAPI_MAX_FILE_IDENTITY_LEN: usize = 4096;

/// 本项目 `FileIdentity` 布局的 schema 版本。
///
/// 改变上面文档里的布局必须递增此值；解码遇到其他版本返回
/// [`IdentityDecodeError::UnsupportedSchemaVersion`]，而不是把旧 blob 当新布局解释。
pub const SCHEMA_VERSION: u8 = 1;

/// 编码后的固定字节数（2 magic + 1 version + 1 reserved + 16 vault + 16 entry + 8 hint + 4 crc）。
pub const ENCODED_LEN: usize = 48;

/// 编码格式的 magic，用来尽早拒绝把别的数据当 `FileIdentity` 解码。
const MAGIC: [u8; 2] = *b"VS";

// 编译期保证“长度有界”：编码不超过 CFAPI 上限，且远小于惯例的单条目元数据规模。
const _: () = assert!(ENCODED_LEN <= CFAPI_MAX_FILE_IDENTITY_LEN);
const _: () = assert!(ENCODED_LEN < 64);

/// 同步根标识：16 字节不透明值。
///
/// 原型的取值见 [`crate::cfapi::prototype_layout::PROTOTYPE_VAULT_ID`]；真实 VaultId 由服务端
/// Vault 分配，本模块只负责字节级承载，不解释其语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VaultId([u8; 16]);

impl VaultId {
    /// 由 16 字节原始值构造。
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 取出 16 字节原始值。
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// 条目身份：16 字节不透明值（design I03：路径改变不改变 EntryId）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntryId([u8; 16]);

impl EntryId {
    /// 由 16 字节原始值构造。
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 取出 16 字节原始值。
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// 版本提示：不透明的远端版本标识。
///
/// 只用于诊断与显示（例如判断占位符是否指向已知版本），**不是**业务真相：in-sync 与内容
/// 基线仍由核心数据库判定（design §14.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionHint(u64);

impl VersionHint {
    /// 由原始值构造。
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// 取出原始值。
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 一个占位符的 `FileIdentity`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// 所属同步根。
    pub vault_id: VaultId,
    /// 条目身份。
    pub entry_id: EntryId,
    /// 远端版本提示。
    pub version_hint: VersionHint,
}

impl FileIdentity {
    /// 编码为固定 [`ENCODED_LEN`] 字节，可直接传给
    /// `CF_PLACEHOLDER_CREATE_INFO::FileIdentity` / `FileIdentityLength`。
    pub fn encode(&self) -> [u8; ENCODED_LEN] {
        let mut bytes = [0u8; ENCODED_LEN];
        bytes[0..2].copy_from_slice(&MAGIC);
        bytes[2] = SCHEMA_VERSION;
        // bytes[3] 是保留字节，保持 0。
        bytes[4..20].copy_from_slice(self.vault_id.as_bytes());
        bytes[20..36].copy_from_slice(self.entry_id.as_bytes());
        bytes[36..44].copy_from_slice(&self.version_hint.get().to_le_bytes());
        let checksum = crc32(&bytes[..44]);
        bytes[44..48].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    /// 解码并校验长度、magic、schema_version、保留字节与 CRC-32。
    pub fn decode(bytes: &[u8]) -> Result<Self, IdentityDecodeError> {
        if bytes.len() != ENCODED_LEN {
            return Err(IdentityDecodeError::BadLength { len: bytes.len() });
        }
        let magic = [bytes[0], bytes[1]];
        if magic != MAGIC {
            return Err(IdentityDecodeError::BadMagic { found: magic });
        }
        if bytes[2] != SCHEMA_VERSION {
            return Err(IdentityDecodeError::UnsupportedSchemaVersion { found: bytes[2] });
        }
        if bytes[3] != 0 {
            return Err(IdentityDecodeError::ReservedByteSet { found: bytes[3] });
        }
        let mut stored = [0u8; 4];
        stored.copy_from_slice(&bytes[44..48]);
        let stored = u32::from_le_bytes(stored);
        let computed = crc32(&bytes[..44]);
        if stored != computed {
            return Err(IdentityDecodeError::ChecksumMismatch { stored, computed });
        }
        let mut vault_id = [0u8; 16];
        vault_id.copy_from_slice(&bytes[4..20]);
        let mut entry_id = [0u8; 16];
        entry_id.copy_from_slice(&bytes[20..36]);
        let mut hint = [0u8; 8];
        hint.copy_from_slice(&bytes[36..44]);
        Ok(Self {
            vault_id: VaultId::from_bytes(vault_id),
            entry_id: EntryId::from_bytes(entry_id),
            version_hint: VersionHint::new(u64::from_le_bytes(hint)),
        })
    }
}

/// `FileIdentity` 解码失败的原因。
///
/// 每个变体都保留实际观察到的值，便于在日志里定位是编码代际不匹配还是 blob 损坏；
/// 调用方不得把这些错误当作“没有身份”而静默继续。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityDecodeError {
    /// blob 长度不是 [`ENCODED_LEN`]。
    BadLength {
        /// 实际字节数。
        len: usize,
    },
    /// magic 不匹配。
    BadMagic {
        /// 实际读到的两个字节。
        found: [u8; 2],
    },
    /// schema_version 不是本实现支持的版本。
    UnsupportedSchemaVersion {
        /// 实际读到的版本号。
        found: u8,
    },
    /// 保留字节被置位。
    ReservedByteSet {
        /// 实际读到的值。
        found: u8,
    },
    /// CRC-32 与内容不符（blob 被截断、拼接或改写）。
    ChecksumMismatch {
        /// blob 内保存的校验值。
        stored: u32,
        /// 按内容重新计算的校验值。
        computed: u32,
    },
}

impl std::fmt::Display for IdentityDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadLength { len } => {
                write!(f, "FileIdentity 长度 {len} 不等于 {ENCODED_LEN}")
            }
            Self::BadMagic { found } => {
                write!(f, "FileIdentity magic {found:02X?} 不是 {MAGIC:02X?}")
            }
            Self::UnsupportedSchemaVersion { found } => {
                write!(
                    f,
                    "FileIdentity schema_version {found} 不是 {SCHEMA_VERSION}"
                )
            }
            Self::ReservedByteSet { found } => {
                write!(f, "FileIdentity 保留字节为 {found}，应为 0")
            }
            Self::ChecksumMismatch { stored, computed } => {
                write!(
                    f,
                    "FileIdentity CRC-32 不匹配：blob 内 0x{stored:08X}，实算 0x{computed:08X}"
                )
            }
        }
    }
}

impl std::error::Error for IdentityDecodeError {}

/// CRC-32/IEEE（反射多项式 `0xEDB88320`），用于发现身份 blob 的损坏。
///
/// 这不是安全校验：它只保证“写进去的 44 字节和读出来的 44 字节一致”。信任边界由服务端
/// 分配的身份与数据库基线负责，`FileIdentity` 本身不作为业务真相。
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            // 最低位为 0 时 mask=0；为 1 时 mask=0xFFFFFFFF。
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{
        ENCODED_LEN, EntryId, FileIdentity, IdentityDecodeError, SCHEMA_VERSION, VaultId,
        VersionHint, crc32,
    };

    fn sample() -> FileIdentity {
        FileIdentity {
            vault_id: VaultId::from_bytes([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF,
            ]),
            entry_id: EntryId::from_bytes([
                0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0, 0xE0,
                0xF0, 0x01,
            ]),
            version_hint: VersionHint::new(0x0102_0304_0506_0708),
        }
    }

    #[test]
    fn crc32_matches_ieee_check_vector() {
        // CRC-32/IEEE 的标准检查向量（"123456789" → 0xCBF43926）。
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn encoded_length_is_bounded_and_fixed() {
        // 长度上限全部由模块级 const 断言在编译期保证（ENCODED_LEN ≤ CFAPI 上限且 < 64）；
        // 这里只确认运行时编码结果就是那个固定长度。
        assert_eq!(sample().encode().len(), ENCODED_LEN);
    }

    #[test]
    fn encode_is_stable_golden_bytes() {
        // 金样本：布局一旦改变必须同时改 schema_version 与本断言，避免旧 blob 被误解。
        let encoded = sample().encode();
        assert_eq!(encoded[0..2], *b"VS");
        assert_eq!(encoded[2], SCHEMA_VERSION);
        assert_eq!(encoded[3], 0);
        assert_eq!(
            encoded[4..20],
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF
            ]
        );
        assert_eq!(
            encoded[20..36],
            [
                0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0, 0xD0, 0xE0,
                0xF0, 0x01
            ]
        );
        assert_eq!(encoded[36..44], 0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(encoded[44..48], crc32(&encoded[..44]).to_le_bytes());
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let identity = sample();
        assert_eq!(FileIdentity::decode(&identity.encode()), Ok(identity));
    }

    #[test]
    fn decode_rejects_short_and_long_blobs() {
        let encoded = sample().encode();
        assert_eq!(
            FileIdentity::decode(&encoded[..ENCODED_LEN - 1]),
            Err(IdentityDecodeError::BadLength {
                len: ENCODED_LEN - 1
            })
        );
        assert_eq!(
            FileIdentity::decode(&[]),
            Err(IdentityDecodeError::BadLength { len: 0 })
        );
        let mut longer = encoded.to_vec();
        longer.push(0);
        assert_eq!(
            FileIdentity::decode(&longer),
            Err(IdentityDecodeError::BadLength {
                len: ENCODED_LEN + 1
            })
        );
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut encoded = sample().encode();
        encoded[0] = b'X';
        assert_eq!(
            FileIdentity::decode(&encoded),
            Err(IdentityDecodeError::BadMagic { found: *b"XS" })
        );
    }

    #[test]
    fn decode_rejects_unknown_schema_version() {
        let mut encoded = sample().encode();
        encoded[2] = SCHEMA_VERSION + 1;
        assert_eq!(
            FileIdentity::decode(&encoded),
            Err(IdentityDecodeError::UnsupportedSchemaVersion {
                found: SCHEMA_VERSION + 1
            })
        );
    }

    #[test]
    fn decode_rejects_reserved_byte() {
        let mut encoded = sample().encode();
        encoded[3] = 1;
        assert_eq!(
            FileIdentity::decode(&encoded),
            Err(IdentityDecodeError::ReservedByteSet { found: 1 })
        );
    }

    #[test]
    fn decode_rejects_corrupted_payload() {
        let mut encoded = sample().encode();
        // 改 entry_id 的一个字节但不更新 CRC：必须被校验拦住，而不是解出一个“新”身份。
        encoded[20] ^= 0xFF;
        let stored = u32::from_le_bytes([encoded[44], encoded[45], encoded[46], encoded[47]]);
        let computed = crc32(&encoded[..44]);
        assert_ne!(stored, computed);
        assert_eq!(
            FileIdentity::decode(&encoded),
            Err(IdentityDecodeError::ChecksumMismatch { stored, computed })
        );
    }

    #[test]
    fn decode_error_messages_are_actionable() {
        let text = IdentityDecodeError::UnsupportedSchemaVersion { found: 7 }.to_string();
        assert!(text.contains("schema_version 7"), "{text}");
        let text = IdentityDecodeError::ChecksumMismatch {
            stored: 1,
            computed: 2,
        }
        .to_string();
        assert!(
            text.contains("0x00000001") && text.contains("0x00000002"),
            "{text}"
        );
    }
}
