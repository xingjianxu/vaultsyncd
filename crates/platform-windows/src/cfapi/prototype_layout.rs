//! S010 原型的固定同步根与条目集合（纯逻辑，可在宿主测试）。
//!
//! 原型要在客体上可重复复现，因此身份与条目集合固定在代码里，而不是随机生成：
//!
//! - 同步根身份 [`PROTOTYPE_VAULT_ID`]：16 字节常量，模拟服务端 VaultId。
//! - 条目目录 [`PROBE_DIRECTORY`]：目录占位符，先于其下文件创建。
//! - 条目文件 [`PROBE_FILES`]：12,345 B 与 1 MiB 两个文件占位符，用来证明逻辑大小正确、
//!   而不是普通零字节文件。
//! - 每个条目的 EntryId 由相对名派生（[`entry_id_for`]），create / inspect / cleanup
//!   三个命令因此在没有数据库的情况下也能互相核对身份。
//!
//! 真实 EntryId 由服务端分配并随绑定下发；本模块的派生只是原型的确定性替代，不能带到
//! 业务路径（design I03 的稳定身份由服务端与客户端数据库共同保证）。

use super::identity::{EntryId, FileIdentity, VaultId, VersionHint};

/// 原型同步根身份（模拟服务端分配的 VaultId）。
pub const PROTOTYPE_VAULT_ID: VaultId = VaultId::from_bytes([
    0x56, 0x53, 0x43, 0x46, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
]);

/// 原型探针目录名（相对同步根），内容全部由本工具创建。
pub const PROBE_DIRECTORY: &str = "vaultsyncd-proto";

/// 原型固定文件集合：名称与逻辑大小（字节）。名称用 ASCII，避免客体控制台代码页干扰。
///
/// S011 增加 16 MiB 条目：1 MiB 及以下会被平台按“整份”请求（S011 实测 `open` 即请求
/// 1,048,576 B），只有更大的条目才会出现真正的范围请求，从而能验证按 Offset/Length 供给。
pub const PROBE_FILES: [(&str, u64); 3] = [
    ("small-12345.bin", 12_345),
    ("large-1mib.bin", 1_048_576),
    ("huge-16mib.bin", 16_777_216),
];

/// 原型条目的固定版本提示。真实版本提示随远端版本推进，这里固定以便复现比较。
pub const PROTOTYPE_VERSION_HINT: VersionHint = VersionHint::new(1);

/// 由条目相对名派生稳定的 EntryId（原型专用）。
///
/// 用两个不同基值的 FNV-1a 64 位哈希拼出 16 字节：确定性、无依赖、改动名称即产生不同身份。
/// 它**不是**密码学哈希，也不能用来判断身份真伪——原型只需可复现的稳定值。
pub fn entry_id_for(relative_name: &str) -> EntryId {
    let low = fnv1a64(0xCBF2_9CE4_8422_2325, relative_name.as_bytes());
    let high = fnv1a64(0x9E37_79B9_7F4A_7C15, relative_name.as_bytes());
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&low.to_le_bytes());
    bytes[8..].copy_from_slice(&high.to_le_bytes());
    EntryId::from_bytes(bytes)
}

/// 原型条目在 `FileIdentity` 中的完整身份。
pub fn file_identity(relative_name: &str) -> FileIdentity {
    FileIdentity {
        vault_id: PROTOTYPE_VAULT_ID,
        entry_id: entry_id_for(relative_name),
        version_hint: PROTOTYPE_VERSION_HINT,
    }
}

/// FNV-1a 64 位哈希（允许自定义基值，以便同一输入派生两段不同字节）。
fn fnv1a64(basis: u64, bytes: &[u8]) -> u64 {
    let mut hash = basis;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01B3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        PROBE_DIRECTORY, PROBE_FILES, PROTOTYPE_VAULT_ID, PROTOTYPE_VERSION_HINT, entry_id_for,
        file_identity,
    };

    #[test]
    fn probe_layout_has_a_directory_and_two_distinct_sized_files() {
        assert_eq!(PROBE_DIRECTORY, "vaultsyncd-proto");
        assert_eq!(PROBE_FILES.len(), 3);
        assert_eq!(PROBE_FILES[0], ("small-12345.bin", 12_345));
        assert_eq!(PROBE_FILES[1], ("large-1mib.bin", 1_048_576));
        assert_eq!(PROBE_FILES[1].1, 1024 * 1024);
        assert_eq!(PROBE_FILES[2], ("huge-16mib.bin", 16_777_216));
        assert_eq!(PROBE_FILES[2].1, 16 * 1024 * 1024);
        // 两个文件大小必须不同，否则无法证明 Length 是逐条目设置的。
        assert_ne!(PROBE_FILES[0].1, PROBE_FILES[1].1);
    }

    #[test]
    fn entry_ids_are_stable_and_distinct() {
        let first = entry_id_for("small-12345.bin");
        assert_eq!(first, entry_id_for("small-12345.bin"));
        assert_ne!(first, entry_id_for("large-1mib.bin"));
        assert_ne!(first, entry_id_for("huge-16mib.bin"));
        assert_ne!(first, entry_id_for(PROBE_DIRECTORY));
    }

    #[test]
    fn file_identity_carries_vault_entry_and_hint() {
        let identity = file_identity("small-12345.bin");
        assert_eq!(identity.vault_id, PROTOTYPE_VAULT_ID);
        assert_eq!(identity.entry_id, entry_id_for("small-12345.bin"));
        assert_eq!(identity.version_hint, PROTOTYPE_VERSION_HINT);

        // 宿主侧即可验证：原型身份能编码并原样解码（客体再验证系统保存的 blob）。
        let encoded = identity.encode();
        assert_eq!(super::FileIdentity::decode(&encoded), Ok(identity));
    }
}
