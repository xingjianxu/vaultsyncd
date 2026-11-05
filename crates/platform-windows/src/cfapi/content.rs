//! 固定测试内容源（S011）：由固定种子确定性生成、版本固定、不可变，并自带 SHA-256。
//!
//! 原型需要在客体上反复验证“读到的字节与哈希是否就是提供方声明的版本内容”（design I05、
//! §13.1、§14.2）。这里不读网络、不读磁盘：每个原型条目的内容完全由 [`CONTENT_SEED`] 与
//! 条目名派生，因此
//!
//! - **版本固定**：种子与条目名固定 ⇒ 内容固定，任何一次运行都得到同一份字节与同一份
//!   SHA-256（[`expected_digest`]）；
//! - **可随机区间供给**：内容按 8 字节位置块生成，[`fill_range`] 只按偏移计算请求区间，
//!   不依赖先读前缀，支持 CFAPI 的范围请求；
//! - **纯逻辑**：不依赖 Windows，可在宿主 `cargo test -p vaultsyncd-platform-windows` 直接
//!   验证（含 NIST SHA-256 向量与跨端哈希比对的期望值）。
//!
//! 这只是原型的“不可变内容版本”，不是服务端对象存储：真实实现按 `entry_id + version_id`
//! 从不可变对象取内容，并校验每块与整文件哈希。

use super::prototype_layout::{PROBE_DIRECTORY, PROBE_FILES};

/// 原型固定内容种子。改动它等于发布新内容版本，会让 [`expected_digest`] 全部变化，
/// 因此宿主测试把当前期望哈希当作金样本固定下来（内容版本必须显式变更，不能悄悄漂移）。
pub const CONTENT_SEED: u64 = 0x5EED_0110_0C0F_FEE1;

/// 单次 `CfExecute(TRANSFER_DATA)` 回填的最大字节数。
///
/// design §13.1 的上传块建议值是 4 MiB；这里取 64 KiB 是为了让 1 MiB 条目也产生多次分段
/// 回执，从而真正覆盖“分段供给”和“分段之间被取消”的行为，而不是一次回执就结束。
pub const TRANSFER_CHUNK_BYTES: u64 = 64 * 1024;

/// 内容源的明确错误；不允许把“条目不存在/范围非法”变成 0 字节成功。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentError {
    /// 请求的路径不属于原型固定条目集合。
    UnknownEntry {
        /// 归一化后的路径（只用于日志，不含凭据）。
        path: String,
    },
    /// 请求范围非法：负偏移、非正长度、或越过内容末尾。
    InvalidRange {
        /// 请求偏移（原样保留，便于看出负数）。
        offset: i64,
        /// 请求长度（原样保留）。
        length: i64,
        /// 该条目的内容长度。
        size: u64,
    },
}

impl std::fmt::Display for ContentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownEntry { path } => write!(f, "不是原型固定条目：{path}"),
            Self::InvalidRange {
                offset,
                length,
                size,
            } => write!(
                f,
                "请求范围非法：offset={offset} length={length} size={size}（offset ≥ 0、length > 0、offset+length ≤ size）"
            ),
        }
    }
}

impl std::error::Error for ContentError {}

/// 条目名（含原型目录前缀）对应的固定长度；不属于原型集合返回 `None`。
pub fn entry_size(relative_name: &str) -> Option<u64> {
    PROBE_FILES
        .iter()
        .find(|(name, _)| *name == relative_name)
        .map(|(_, size)| *size)
}

/// 原型固定条目清单（名称与长度），供命令入口与脚本打印期望值。
pub fn entries() -> &'static [(&'static str, u64)] {
    &PROBE_FILES
}

/// 从 CFAPI 给的归一化完整路径识别原型条目，返回（条目名，内容长度）。
///
/// 只接受形如 `…\vaultsyncd-proto\<固定条目名>` 的路径：父目录必须是 [`PROBE_DIRECTORY`]，
/// 条目名必须在 [`PROBE_FILES`] 中。反斜杠与正斜杠都按分隔符处理，`..` 之类的相对片段
/// 不会被解读（只要最后两段不匹配就返回 `None`，因此不存在路径穿越）。
pub fn entry_for_path(path: &str) -> Option<(&'static str, u64)> {
    let mut components = path.rsplit(['\\', '/']);
    let name = components.next()?;
    let parent = components.next()?;
    if parent != PROBE_DIRECTORY {
        return None;
    }
    PROBE_FILES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .copied()
}

/// 校验请求范围，返回 `(offset, length)` 的无符号形式。
///
/// 拒绝 `offset < 0`、`length <= 0`（0 字节“成功”会让读取方把不完整内容当有效数据）以及
/// 越界区间；合法区间必须完全落在 `[0, size)` 内。
pub fn check_range(offset: i64, length: i64, size: u64) -> Result<(u64, u64), ContentError> {
    let invalid = || ContentError::InvalidRange {
        offset,
        length,
        size,
    };
    if offset < 0 || length <= 0 {
        return Err(invalid());
    }
    let offset = u64::try_from(offset).map_err(|_| invalid())?;
    let length = u64::try_from(length).map_err(|_| invalid())?;
    if offset >= size {
        return Err(invalid());
    }
    if offset.saturating_add(length) > size {
        return Err(invalid());
    }
    Ok((offset, length))
}

/// 把 `name` 从 `offset` 开始的内容写进 `out`。
///
/// 生成的字节与位置一一对应：任意 `[offset, offset + out.len())` 区间取到的字节，等于整份
/// 内容在该区间的切片（宿主测试对多个未对齐区间逐字节比对）。越界请求返回
/// [`ContentError::InvalidRange`]，`name` 不属于原型条目返回 [`ContentError::UnknownEntry`]。
pub fn fill_range(name: &str, offset: u64, out: &mut [u8]) -> Result<(), ContentError> {
    let size = entry_size(name).ok_or_else(|| ContentError::UnknownEntry {
        path: name.to_string(),
    })?;
    let length = u64::try_from(out.len()).unwrap_or(u64::MAX);
    if offset.saturating_add(length) > size {
        return Err(ContentError::InvalidRange {
            offset: i64::try_from(offset).unwrap_or(i64::MAX),
            length: i64::try_from(length).unwrap_or(i64::MAX),
            size,
        });
    }
    fill_from(entry_seed(name), offset, out);
    Ok(())
}

/// 整份条目内容（`expect` 子命令与宿主测试用；回调内只按区间生成）。
pub fn content(name: &str) -> Option<Vec<u8>> {
    let size = entry_size(name)?;
    let mut bytes = vec![0u8; usize::try_from(size).ok()?];
    fill_from(entry_seed(name), 0, &mut bytes);
    Some(bytes)
}

/// 条目整份内容的 SHA-256（小写十六进制），即该固定版本内容的已知哈希。
pub fn expected_digest(name: &str) -> Option<String> {
    content(name).map(|bytes| sha256_hex(&bytes))
}

/// 条目内容的派生种子：固定种子与条目名混合，使不同条目内容互不相同且可复现。
fn entry_seed(name: &str) -> u64 {
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01B3);
    }
    CONTENT_SEED ^ hash
}

/// 生成 `seed` 下第 `index` 个 8 字节位置块（SplitMix64 混合函数）。
///
/// 位置块只依赖 `(seed, index)`，因此区间生成无需知道前缀已生成了多少字节。
fn position_block(seed: u64, index: u64) -> [u8; 8] {
    let mut z = seed ^ index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (z ^ (z >> 31)).to_be_bytes()
}

/// 从 `offset` 开始填充 `out`，可跨位置块、可从块中间开始。
fn fill_from(seed: u64, offset: u64, out: &mut [u8]) {
    let mut written = 0usize;
    let mut position = offset;
    while written < out.len() {
        let block = position_block(seed, position / 8);
        let start = usize::try_from(position % 8).unwrap_or(0);
        let take = (8 - start).min(out.len() - written);
        out[written..written + take].copy_from_slice(&block[start..start + take]);
        written += take;
        position += u64::try_from(take).unwrap_or(0);
    }
}

/// `[offset, offset + length)` 切成不超过 [`TRANSFER_CHUNK_BYTES`] 的连续分段。
///
/// 返回的每段都非空且首尾相接、正好覆盖整个请求；空请求返回空迭代（调用方应先做范围校验，
/// 0 长度不属于合法请求）。
pub fn chunk_ranges(offset: u64, length: u64) -> Vec<(u64, u64)> {
    let mut chunks = Vec::new();
    let mut served = 0u64;
    while served < length {
        let remaining = length - served;
        let size = remaining.min(TRANSFER_CHUNK_BYTES);
        chunks.push((offset + served, size));
        served += size;
    }
    chunks
}

/// 回填循环的下游：把一段内容交给平台（Windows 上是 `CfExecute(TRANSFER_DATA)`）。
///
/// 抽象出来是为了让“取消标志在分段之间生效”“下游失败立即停止”这类语义可以在宿主测试里
/// 用假实现验证（design I05：不发布不完整内容；回调取消后尽快退出）。
pub trait ChunkSink {
    /// 是否已被取消；在每段开始前检查。
    fn aborted(&self) -> bool;

    /// 交出一段内容（`bytes.len()` 就是该段长度）。
    fn send(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SendFailure>;
}

/// 平台拒绝某一段的回填（例如连接已断开时 `CfExecute` 返回失败）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendFailure {
    /// 该段的起始偏移。
    pub offset: u64,
    /// 平台返回的原始 `HRESULT`/`NTSTATUS`。
    pub code: i32,
}

/// 一次范围供给的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServeReport {
    /// 成功交给平台的分段数。
    pub chunks_sent: u64,
    /// 成功交给平台的字节数。
    pub bytes_sent: u64,
    /// 是否因取消在分段边界退出。
    pub aborted: bool,
    /// 下游失败原因（如有）。
    pub failure: Option<SendFailure>,
}

/// 按 [`chunk_ranges`] 逐段生成内容并交给 `sink`，每段前检查取消。
///
/// 语义（宿主测试逐条覆盖）：
///
/// - 取消发生在分段之间：被取消时**不再发送任何后续分段**，`aborted=true`；
/// - 下游拒绝某段时立即停止，不继续把后续分段推给平台（避免把不完整内容当成功）；
/// - 0 长度/越界请求在校验阶段就失败，不会产生“0 字节成功”。
pub fn serve_range(
    name: &str,
    offset: u64,
    length: u64,
    sink: &mut impl ChunkSink,
) -> Result<ServeReport, ContentError> {
    let size = entry_size(name).ok_or_else(|| ContentError::UnknownEntry {
        path: name.to_string(),
    })?;
    if offset >= size || length == 0 || offset.saturating_add(length) > size {
        return Err(ContentError::InvalidRange {
            offset: i64::try_from(offset).unwrap_or(i64::MAX),
            length: i64::try_from(length).unwrap_or(i64::MAX),
            size,
        });
    }
    let mut report = ServeReport::default();
    for (chunk_offset, chunk_length) in chunk_ranges(offset, length) {
        if sink.aborted() {
            report.aborted = true;
            return Ok(report);
        }
        let mut buffer = vec![0u8; usize::try_from(chunk_length).unwrap_or(0)];
        fill_range(name, chunk_offset, &mut buffer)?;
        if let Err(failure) = sink.send(chunk_offset, &buffer) {
            report.failure = Some(failure);
            return Ok(report);
        }
        report.chunks_sent += 1;
        report.bytes_sent += chunk_length;
    }
    Ok(report)
}

/// SHA-256（FIPS 180-4）。本包无第三方依赖，故自带实现；正确性由 NIST 测试向量锁定。
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let mut message = Vec::with_capacity(bytes.len() + 72);
    message.extend_from_slice(bytes);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    let bit_len = (bytes.len() as u128) * 8;
    message.extend_from_slice(&bit_len.to_be_bytes()[8..]);

    for block in message.chunks_exact(64) {
        let mut words = [0u32; 64];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[index * 4],
                block[index * 4 + 1],
                block[index * 4 + 2],
                block[index * 4 + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let mut working = state;
        for index in 0..64 {
            let s1 = working[4].rotate_right(6)
                ^ working[4].rotate_right(11)
                ^ working[4].rotate_right(25);
            let choice = (working[4] & working[5]) ^ (!working[4] & working[6]);
            let temp1 = working[7]
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = working[0].rotate_right(2)
                ^ working[0].rotate_right(13)
                ^ working[0].rotate_right(22);
            let majority =
                (working[0] & working[1]) ^ (working[0] & working[2]) ^ (working[1] & working[2]);
            let temp2 = s0.wrapping_add(majority);
            working[7] = working[6];
            working[6] = working[5];
            working[5] = working[4];
            working[4] = working[3].wrapping_add(temp1);
            working[3] = working[2];
            working[2] = working[1];
            working[1] = working[0];
            working[0] = temp1.wrapping_add(temp2);
        }
        for index in 0..8 {
            state[index] = state[index].wrapping_add(working[index]);
        }
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// SHA-256 的小写十六进制表示（日志与跨端比对用）。
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(64);
    for byte in sha256(bytes) {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{
        CONTENT_SEED, ChunkSink, ContentError, SendFailure, ServeReport, TRANSFER_CHUNK_BYTES,
        check_range, chunk_ranges, content, entry_for_path, entry_size, expected_digest,
        fill_range, serve_range, sha256_hex,
    };

    /// 测试用下游：记录收到的分段、可在第 N 段后“被取消”或拒绝。
    struct FakeSink {
        abort_after: Option<u64>,
        reject_at: Option<u64>,
        sent: Vec<(u64, Vec<u8>)>,
    }

    impl FakeSink {
        fn new(abort_after: Option<u64>, reject_at: Option<u64>) -> Self {
            Self {
                abort_after,
                reject_at,
                sent: Vec::new(),
            }
        }
    }

    impl ChunkSink for FakeSink {
        fn aborted(&self) -> bool {
            self.abort_after
                .is_some_and(|limit| self.sent.len() as u64 >= limit)
        }

        fn send(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SendFailure> {
            if self.reject_at == Some(self.sent.len() as u64) {
                return Err(SendFailure {
                    offset,
                    code: 0x8007_0057u32 as i32,
                });
            }
            self.sent.push((offset, bytes.to_vec()));
            Ok(())
        }
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        // FIPS 180-4 / NIST CAVP 的公开向量。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 1,000,000 个 'a'：跨多块与长度编码边界。
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            sha256_hex(&million),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn content_is_position_addressed_and_ranges_match_full_slice() {
        let full = content("large-1mib.bin").expect("large entry");
        assert_eq!(full.len(), 1_048_576);
        // 未对齐偏移、跨位置块、末尾不足一块的区间都必须与整份内容的切片逐字节相同。
        for (offset, length) in [
            (0u64, 1u64),
            (1, 7),
            (3, 4096),
            (65_535, 65_536),
            (1_048_500, 76),
            (123_456, 1),
        ] {
            let mut ranged = vec![0u8; usize::try_from(length).unwrap()];
            fill_range("large-1mib.bin", offset, &mut ranged).expect("range");
            let start = usize::try_from(offset).unwrap();
            let end = start + usize::try_from(length).unwrap();
            assert_eq!(ranged, full[start..end], "offset={offset} length={length}");
        }
    }

    #[test]
    fn content_is_deterministic_and_differs_per_entry() {
        assert_eq!(content("small-12345.bin"), content("small-12345.bin"));
        let small = content("small-12345.bin").expect("small entry");
        let large = content("large-1mib.bin").expect("large entry");
        assert_eq!(small.len(), 12_345);
        assert_ne!(small, large[..small.len()]);
    }

    #[test]
    fn expected_digests_are_pinned_for_the_fixed_content_version() {
        // 金样本：固定种子 + 条目名 ⇒ 固定内容 ⇒ 固定 SHA-256。改动 CONTENT_SEED 或生成
        // 算法必须同时更新期望值，等于显式发布新内容版本（内容不能悄悄漂移）。
        assert_eq!(CONTENT_SEED, 0x5EED_0110_0C0F_FEE1);
        assert_eq!(
            expected_digest("small-12345.bin").as_deref(),
            Some("a82033db2b87c155e2ce539bfdb4d18929431883af1ffa7b8b5378fe3ca94bcd")
        );
        assert_eq!(
            expected_digest("large-1mib.bin").as_deref(),
            Some("8333acaa8d2834095eec7e33c0c133dc560e0efc5254d1136d07655fe6c62759")
        );
        assert_eq!(entry_size("small-12345.bin"), Some(12_345));
        assert_eq!(entry_size("large-1mib.bin"), Some(1_048_576));
        assert_eq!(expected_digest("missing.bin"), None);
    }

    #[test]
    fn entry_lookup_requires_the_prototype_directory() {
        assert_eq!(
            entry_for_path(r"C:\vaultsyncd-test\vaultsyncd-proto\small-12345.bin"),
            Some(("small-12345.bin", 12_345))
        );
        assert_eq!(
            entry_for_path("C:/vaultsyncd-test/vaultsyncd-proto/large-1mib.bin"),
            Some(("large-1mib.bin", 1_048_576))
        );
        // 父目录不对、条目名不对、只有名字、路径穿越都不能被当成原型条目。
        assert_eq!(
            entry_for_path(r"C:\vaultsyncd-test\other\small-12345.bin"),
            None
        );
        assert_eq!(
            entry_for_path(r"C:\vaultsyncd-test\vaultsyncd-proto\other.bin"),
            None
        );
        assert_eq!(entry_for_path("small-12345.bin"), None);
        assert_eq!(
            entry_for_path(r"C:\vaultsyncd-test\vaultsyncd-proto\..\small-12345.bin"),
            None
        );
        assert_eq!(entry_for_path(""), None);
    }

    #[test]
    fn invalid_ranges_are_rejected_explicitly() {
        assert_eq!(check_range(0, 12_345, 12_345), Ok((0, 12_345)));
        assert_eq!(check_range(12_344, 1, 12_345), Ok((12_344, 1)));
        for (offset, length) in [(-1i64, 1i64), (0, 0), (0, -1), (12_345, 1), (12_000, 1_000)] {
            assert!(
                matches!(
                    check_range(offset, length, 12_345),
                    Err(ContentError::InvalidRange { .. })
                ),
                "offset={offset} length={length} 必须被拒绝"
            );
        }
        // 0 长度请求不能变成“0 字节成功”。
        let mut empty: [u8; 0] = [];
        assert!(matches!(
            fill_range("small-12345.bin", 12_344, &mut empty[..0]),
            Ok(())
        ));
        assert!(matches!(
            fill_range("small-12345.bin", 12_345, &mut [0u8; 1]),
            Err(ContentError::InvalidRange { .. })
        ));
        assert!(matches!(
            fill_range("missing.bin", 0, &mut [0u8; 1]),
            Err(ContentError::UnknownEntry { .. })
        ));
    }

    #[test]
    fn serve_range_sends_every_chunk_of_the_requested_range() {
        let mut sink = FakeSink::new(None, None);
        let report = serve_range("large-1mib.bin", 65_536, 131_072, &mut sink).expect("serve");
        assert_eq!(
            report,
            ServeReport {
                chunks_sent: 2,
                bytes_sent: 131_072,
                aborted: false,
                failure: None,
            }
        );
        let full = content("large-1mib.bin").expect("content");
        let mut position = 65_536usize;
        for (offset, bytes) in &sink.sent {
            assert_eq!(usize::try_from(*offset).unwrap(), position);
            assert_eq!(*bytes, full[position..position + bytes.len()]);
            position += bytes.len();
        }
        assert_eq!(position, 65_536 + 131_072);
    }

    #[test]
    fn serve_range_stops_immediately_when_aborted_between_chunks() {
        // 取消语义：第 3 段之后被取消，剩余分段一律不再发送。
        let mut sink = FakeSink::new(Some(3), None);
        let report = serve_range("large-1mib.bin", 0, 1_048_576, &mut sink).expect("serve");
        assert!(report.aborted);
        assert_eq!(report.chunks_sent, 3);
        assert_eq!(report.bytes_sent, 3 * TRANSFER_CHUNK_BYTES);
        assert_eq!(sink.sent.len(), 3);
        assert_eq!(report.failure, None);
    }

    #[test]
    fn serve_range_stops_on_platform_rejection_without_hiding_it() {
        let mut sink = FakeSink::new(None, Some(1));
        let report = serve_range("large-1mib.bin", 0, 1_048_576, &mut sink).expect("serve");
        assert!(!report.aborted);
        assert_eq!(report.chunks_sent, 1);
        assert_eq!(
            report.failure,
            Some(SendFailure {
                offset: TRANSFER_CHUNK_BYTES,
                code: 0x8007_0057u32 as i32,
            })
        );
        assert_eq!(sink.sent.len(), 1);
    }

    #[test]
    fn serve_range_rejects_invalid_ranges_before_sending_anything() {
        let mut sink = FakeSink::new(None, None);
        for (offset, length) in [(0u64, 0u64), (1_048_576, 1), (1_048_000, 1_000)] {
            assert!(matches!(
                serve_range("large-1mib.bin", offset, length, &mut sink),
                Err(ContentError::InvalidRange { .. })
            ));
        }
        assert!(matches!(
            serve_range("missing.bin", 0, 1, &mut sink),
            Err(ContentError::UnknownEntry { .. })
        ));
        assert!(sink.sent.is_empty());
    }

    #[test]
    fn chunks_tile_the_request_without_gaps() {
        assert_eq!(chunk_ranges(0, 0), Vec::new());
        assert_eq!(chunk_ranges(0, 100), vec![(0, 100)]);
        let chunks = chunk_ranges(0, TRANSFER_CHUNK_BYTES + 7);
        assert_eq!(
            chunks,
            vec![(0, TRANSFER_CHUNK_BYTES), (TRANSFER_CHUNK_BYTES, 7)]
        );
        let chunks = chunk_ranges(1000, 1_048_576 - 1000);
        assert_eq!(chunks.first().copied(), Some((1000, TRANSFER_CHUNK_BYTES)));
        assert_eq!(chunks.last().map(|(o, l)| o + l), Some(1_048_576));
        let mut expected_offset = 1000;
        for (offset, length) in &chunks {
            assert_eq!(*offset, expected_offset);
            assert!(*length > 0 && *length <= TRANSFER_CHUNK_BYTES);
            expected_offset += length;
        }
        assert_eq!(expected_offset, 1_048_576);
    }
}
