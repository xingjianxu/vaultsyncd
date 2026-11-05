//! 不透明身份类型（`docs/design.md` §7.1）。
//!
//! 身份是定长字节的 newtype，不是字符串或整数：比较、排序、哈希都按字节，ID 本身不承载语义
//! （不表示路径、层级或时间）。文本形式只用于 wire 与日志：规范形式是 [`ID_HEX_LEN`] 个小写
//! 十六进制字符，解析同时接受大写，因此同一个值只有一种字节表示，不存在“看起来不同”的同一身份。
//!
//! 长度固定为 [`ID_LEN`] = 16 字节，与 `crates/platform-windows` 的 CFAPI `FileIdentity` 编码
//! 一致（该 48 字节 blob 中 RootId、EntryId 各占 16 字节），所以服务端分配的 ID 可以直接写进
//! 平台身份，不需要第二套映射；`crates/domain` 仍是身份的唯一定义处。
//!
//! 本模块不做 I/O：随机性由调用方通过 [`IdSource`] 注入（design §5.4），因此生成过程在测试中
//! 完全可复现。

use core::fmt;
use core::str::FromStr;

/// 不透明身份的字节长度。
pub const ID_LEN: usize = 16;

/// 规范文本形式的字符数（每个字节两个十六进制字符）。
pub const ID_HEX_LEN: usize = ID_LEN * 2;

/// 不透明身份文本的解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdParseError {
    /// 文本为空。
    Empty,
    /// 字节长度不是 [`ID_HEX_LEN`]，附实际长度。
    ///
    /// 长度不符时不再逐字符检查，因此 `0x…`、带连字符的 UUID 文本都落在这里。
    Length(usize),
    /// 出现非十六进制字符，附其在该文本中的字节位置（0 基）。
    NonHexDigit(usize),
}

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("身份文本为空"),
            Self::Length(found) => {
                write!(f, "身份文本长度 {found}，应为 {ID_HEX_LEN} 个十六进制字符")
            }
            Self::NonHexDigit(index) => {
                write!(f, "身份文本第 {index} 个字节不是十六进制字符")
            }
        }
    }
}

impl core::error::Error for IdParseError {}

/// 不透明身份的随机字节来源端口（design §5.4：领域层不做 I/O，随机性由调用方注入）。
///
/// 生产实现取操作系统的随机源；测试实现可以是计数器或固定序列，从而让生成的 ID 可复现。
pub trait IdSource {
    /// 返回一个新身份的全部 [`ID_LEN`] 个随机字节。
    ///
    /// 实现必须给出独立的随机值；领域层不检查返回值是否重复。
    fn next_id_bytes(&mut self) -> [u8; ID_LEN];
}

/// 把十六进制文本解析成原始字节；格式规则见 [`IdParseError`]。
fn parse_hex(text: &str) -> Result<[u8; ID_LEN], IdParseError> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(IdParseError::Empty);
    }
    if bytes.len() != ID_HEX_LEN {
        return Err(IdParseError::Length(bytes.len()));
    }
    let mut raw = [0u8; ID_LEN];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = hex_digit(pair[0]).ok_or(IdParseError::NonHexDigit(index * 2))?;
        let low = hex_digit(pair[1]).ok_or(IdParseError::NonHexDigit(index * 2 + 1))?;
        raw[index] = (high << 4) | low;
    }
    Ok(raw)
}

/// 单个十六进制字符的取值；大写与小写等价。
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 写 32 个小写十六进制字符（不分配）。
fn write_hex(bytes: &[u8; ID_LEN], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

/// 声明一种不透明身份：16 字节载荷、规范小写十六进制文本、与其它身份类型互不兼容。
///
/// 各类型都提供同一组接口（`from_bytes`/`as_bytes`/`from_hex`/`to_hex`/`generate`），并按
/// 字节实现 `Ord`/`Hash`；文档注释由调用方给出，说明该身份在 design 中的含义。
macro_rules! opaque_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; ID_LEN]);

        impl $name {
            /// 由原始字节构造：字节即身份，不做范围或格式判断。
            pub const fn from_bytes(bytes: [u8; ID_LEN]) -> Self {
                Self(bytes)
            }

            /// 取出原始字节（供平台身份、数据库 BLOB 与编码器使用）。
            pub const fn as_bytes(&self) -> &[u8; ID_LEN] {
                &self.0
            }

            /// 由注入的 [`IdSource`] 生成新身份。
            pub fn generate(source: &mut impl IdSource) -> Self {
                Self(source.next_id_bytes())
            }

            /// 解析十六进制文本；大小写都接受，其它格式返回 [`IdParseError`]。
            pub fn from_hex(text: &str) -> Result<Self, IdParseError> {
                Ok(Self(parse_hex(text)?))
            }

            /// 规范文本形式：32 个小写十六进制字符，也是 wire 上的字符串形式。
            pub fn to_hex(self) -> String {
                self.to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::from_hex(text)
            }
        }
    };
}

opaque_id! {
    /// 安装实例身份：备份恢复沿用该身份，但更换 [`ServerEpoch`](crate::version::ServerEpoch)。
    ServerId
}

opaque_id! {
    /// 所有者账号身份（design §7.2 `users.id`、§17.1）。
    ///
    /// 每个服务端 Root 都通过 `owner_id` 归属到一个账号；数据访问必须带这个身份作 owner scope，
    /// 不能因为“首发只有一位 owner”就省略授权边界。
    UserId
}

opaque_id! {
    /// 被管理的服务器目录身份，不等于本地路径文本。
    RootId
}

opaque_id! {
    /// 某设备某目录与 Root 的绑定身份，持有独立同步模式与排除规则。
    BindingId
}

opaque_id! {
    /// 服务器命名空间条目身份；路径改变不改变它（design I03），根本身也有合成 EntryId。
    EntryId
}

opaque_id! {
    /// 不可变文件内容版本身份；目录没有 VersionId（design I12：已提交版本的字节不再改变）。
    VersionId
}

opaque_id! {
    /// 设备身份。它标识设备，但不承担身份认证（design §7.1）。
    DeviceId
}

opaque_id! {
    /// 逻辑操作身份：同一 `OpId` 与同一规范化请求只产生一个业务结果（design I06）。
    OpId
}

opaque_id! {
    /// 服务端事务批次身份：一次提交产生的事件批次带同一个 `TxId`（design §7.2 `changes.tx_id`、
    /// §9.3「每批含 RootSeq、TxId」）。
    ///
    /// 它只用于把同一次提交的批次与日志关联起来，不承担幂等键职责——幂等由
    /// (`RootId`, `DeviceId`, `OpId`) 决定。
    TxId
}

opaque_id! {
    /// 登录会话身份（design §7.2 `sessions.id`、`uploads.session_id`）。
    ///
    /// 上传会话绑定到签发它的登录会话：会话失效后不允许接着续传（design §13.2「重新鉴权 →
    /// 校验会话仍有效」）。它不承担令牌职责，令牌摘要只在 `sessions` 表里。
    SessionId
}

opaque_id! {
    /// 上传会话身份（design §7.2 `uploads.id`、§13.1）。
    UploadId
}

opaque_id! {
    /// 工作目录发布意图身份（design §7.2 `fs_intents.intent_id`、§11.2 步骤 5）。
    IntentId
}

opaque_id! {
    /// 下载/提交期间保护已登记内容对象的短期租约身份（design §19.4 的上传/下载租约）。
    ObjectLeaseId
}

opaque_id! {
    /// 备份租约身份（design §7.2 `backup_leases.id`、§19.1）。
    BackupLeaseId
}

opaque_id! {
    /// 一致性快照清单身份（design §9.3 的 `snapshot_id`）：同一份清单的所有分页引用同一个值。
    SnapshotId
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: [u8; ID_LEN] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    const SAMPLE_HEX: &str = "00112233445566778899aabbccddeeff";

    const ZERO_ENTRY: EntryId = EntryId::from_bytes([0; ID_LEN]);

    /// 对每种身份类型执行同一组检查：规范文本、往返、Debug 只带类型名与文本。
    macro_rules! check_identity_type {
        ($ty:ty) => {{
            let id = <$ty>::from_bytes(SAMPLE);
            assert_eq!(id.as_bytes(), &SAMPLE);
            assert_eq!(id.to_hex(), SAMPLE_HEX);
            assert_eq!(id.to_string(), SAMPLE_HEX);
            assert_eq!(
                format!("{id:?}"),
                concat!(stringify!($ty), "(00112233445566778899aabbccddeeff)")
            );
            assert_eq!(<$ty>::from_hex(SAMPLE_HEX).unwrap(), id);
            assert_eq!(SAMPLE_HEX.parse::<$ty>().unwrap(), id);
        }};
    }

    #[test]
    fn every_identity_type_round_trips_through_canonical_hex() {
        check_identity_type!(ServerId);
        check_identity_type!(UserId);
        check_identity_type!(RootId);
        check_identity_type!(BindingId);
        check_identity_type!(EntryId);
        check_identity_type!(VersionId);
        check_identity_type!(DeviceId);
        check_identity_type!(OpId);
        check_identity_type!(TxId);
        check_identity_type!(SessionId);
        check_identity_type!(UploadId);
        check_identity_type!(IntentId);
        check_identity_type!(ObjectLeaseId);
        check_identity_type!(BackupLeaseId);
        check_identity_type!(SnapshotId);
    }

    #[test]
    fn identity_hex_parsing_accepts_uppercase() {
        assert_eq!(
            EntryId::from_hex(&SAMPLE_HEX.to_uppercase()).unwrap(),
            EntryId::from_bytes(SAMPLE)
        );
        assert_eq!(
            RootId::from_hex("0011AA22bb33CC44dd55EE66ff77AA88")
                .unwrap()
                .to_hex(),
            "0011aa22bb33cc44dd55ee66ff77aa88"
        );
    }

    #[test]
    fn identity_parsing_rejects_malformed_text() {
        let cases = [
            ("", IdParseError::Empty),
            ("00112233445566778899aabbccddeef", IdParseError::Length(31)),
            (
                "00112233445566778899aabbccddeeff00",
                IdParseError::Length(34),
            ),
            (
                "00112233-4455-6677-8899-aabbccddeeff",
                IdParseError::Length(36),
            ),
            (
                "0x112233445566778899aabbccddeeff",
                IdParseError::NonHexDigit(1),
            ),
            (
                "00112233445566778899aabbccddeefg",
                IdParseError::NonHexDigit(31),
            ),
            (
                " 0112233445566778899aabbccddeeff",
                IdParseError::NonHexDigit(0),
            ),
            ("éééééééééééééééé", IdParseError::NonHexDigit(0)),
        ];
        for (text, expected) in cases {
            assert_eq!(EntryId::from_hex(text), Err(expected), "text={text:?}");
            assert_eq!(text.parse::<EntryId>(), Err(expected), "text={text:?}");
        }
    }

    #[test]
    fn parse_error_display_states_the_problem() {
        assert!(IdParseError::Empty.to_string().contains("空"));
        assert!(IdParseError::Length(31).to_string().contains("31"));
        assert!(IdParseError::NonHexDigit(7).to_string().contains('7'));
    }

    #[test]
    fn identity_bytes_work_in_const_context() {
        assert_eq!(ZERO_ENTRY.to_hex(), "0".repeat(ID_HEX_LEN));
        assert_eq!(EntryId::from_hex(&ZERO_ENTRY.to_hex()).unwrap(), ZERO_ENTRY);
    }

    /// 可复现的 ID 来源：前 8 字节是大端计数器，其余为 0。
    struct CountingSource(u64);

    impl IdSource for CountingSource {
        fn next_id_bytes(&mut self) -> [u8; ID_LEN] {
            let mut bytes = [0u8; ID_LEN];
            bytes[..8].copy_from_slice(&self.0.to_be_bytes());
            self.0 += 1;
            bytes
        }
    }

    #[test]
    fn id_source_injection_makes_generation_deterministic() {
        let mut first = CountingSource(1);
        let mut second = CountingSource(1);
        let generated = [EntryId::generate(&mut first), EntryId::generate(&mut first)];
        let repeated = [
            EntryId::generate(&mut second),
            EntryId::generate(&mut second),
        ];
        assert_eq!(generated, repeated);
        assert_ne!(generated[0], generated[1]);
        assert_eq!(generated[0].to_hex(), "00000000000000010000000000000000");
        assert_eq!(generated[1].to_hex(), "00000000000000020000000000000000");

        // 相同字节在不同身份类型里仍是不同类型：`RootId` 与 `EntryId` 不能互相赋值。
        let mut third = CountingSource(1);
        assert_eq!(RootId::generate(&mut third).to_hex(), generated[0].to_hex());
    }
}
