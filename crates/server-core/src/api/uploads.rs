//! 上传会话与固定块接收的用例规则（design §13.1—13.2、§15.2、I05）。
//!
//! 边界：本模块决定“这个会话能不能开”“这块字节能不能收”，不碰 HTTP、不写数据库、不碰文件
//! 系统——会话与块的存取走 [`UploadSessionStore`]，字节落盘走 [`PartStaging`]（适配器负责）。
//!
//! 固定下来的顺序与判据：
//!
//! 1. **会话绑定一份不可变快照**：总大小、总 SHA-256 与块大小在创建时确定，块几何由
//!    `vaultsyncd_transfer::chunk::ChunkGeometry` 给出（与客户端同一份定义，design §5.4）。
//! 2. **长度先于字节**：块长度必须等于几何给出的期望长度；HTTP 层据此在读取请求体前就能拒绝
//!    越界索引与超大载荷（design §13.1“块包含 index、length、SHA-256”）。
//! 3. **摘要不符一律拒收**：收到的字节按 SHA-256 现场核对声明值，不符返回
//!    [`UploadFailure::DigestMismatch`]，不落盘、不登记（design §15.4 `HASH_MISMATCH`）。
//! 4. **先持久化再记完成**：字节先经 [`PartStaging`] 落盘并 `fsync`，然后才登记块行。登记失败时
//!    最多留下一个孤儿暂存文件，绝不会出现“已登记但内容丢失”。
//! 5. **同索引必须字节一致**：同一索引已经登记过且内容相同 → [`PartWrite::AlreadyPresent`]（幂等，
//!    不重写磁盘）；内容不同 → [`UploadFailure::PartConflict`] 且不覆盖旧块（design §13.1）。
//! 6. **状态由服务器给出**：只有 `open` 会话接收块；`complete`/`finalized`/`aborted` 一律拒绝，
//!    续传依据是服务器记录的已收块，不是客户端位图。

use vaultsyncd_domain::ids::{RootId, SessionId, UploadId};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_transfer::chunk::{ChunkGeometry, GeometryError};
use vaultsyncd_transfer::hash;

use crate::objects::ContentDigest;
use crate::ports::upload_session::{PartStaging, UploadSessionStore};

/// 默认块大小（design §13.1 建议 4 MiB）。
pub const DEFAULT_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
/// 服务端接受的最大块大小：再大就把单次请求的载荷与内存占用放大到无法界定的程度。
pub const MAX_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
/// 单个上传会话允许的最大总大小（客户端一次上传一份快照，超过它应拆成多次操作）。
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// 上传的边界策略（总大小、块大小）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadPolicy {
    /// 客户端没指定块大小时使用的块大小。
    pub default_chunk_size: u64,
    /// 允许的最大块大小。
    pub max_chunk_size: u64,
    /// 允许的最大总大小。
    pub max_total_size: u64,
}

impl UploadPolicy {
    /// 首发默认策略。
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            default_chunk_size: DEFAULT_CHUNK_BYTES,
            max_chunk_size: MAX_CHUNK_BYTES,
            max_total_size: MAX_TOTAL_BYTES,
        }
    }
}

/// 上传会话的状态（与 `uploads.state` 的取值一一对应）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadPhase {
    /// 接收块中。
    Open,
    /// 块已齐，等待 finalize（S046）。
    Complete,
    /// 已发布为不可变对象（终态）。
    Finalized,
    /// 已放弃（终态）。
    Aborted,
}

impl UploadPhase {
    /// 是否还接收块。
    #[must_use]
    pub const fn accepts_parts(self) -> bool {
        matches!(self, Self::Open)
    }

    /// 数据库/契约里的规范文本（同时是 `UploadState` 的 wire 取值）。
    #[must_use]
    pub const fn as_text(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Complete => "complete",
            Self::Finalized => "finalized",
            Self::Aborted => "aborted",
        }
    }
}

/// 一个上传会话的核心事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadSession {
    /// 会话身份。
    pub upload: UploadId,
    /// 所属 Root。
    pub root: RootId,
    /// 签发会话的登录会话（用于“跨会话访问拒绝”）。
    pub session: SessionId,
    /// 快照总大小。
    pub total_size: u64,
    /// 快照总 SHA-256。
    pub hash: ContentDigest,
    /// 块大小。
    pub chunk_size: u64,
    /// 状态。
    pub phase: UploadPhase,
    /// 到期时间。
    pub expires_at: UtcTimestamp,
}

impl UploadSession {
    /// 会话的块几何。
    ///
    /// # Errors
    ///
    /// 读到的块大小为 0（坏行）时返回 [`UploadFailure::MalformedSession`]。
    pub fn geometry(&self) -> Result<ChunkGeometry, UploadFailure> {
        ChunkGeometry::new(self.total_size, self.chunk_size)
            .map_err(|GeometryError::ZeroChunkSize| UploadFailure::MalformedSession)
    }
}

/// 已登记的一个块。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPart {
    /// 块索引。
    pub index: u64,
    /// 块长度。
    pub length: u64,
    /// 块 SHA-256。
    pub hash: ContentDigest,
    /// 暂存位置（适配器给的规范相对路径）。
    pub path: String,
}

/// 登记块的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartWrite {
    /// 首次登记。
    Recorded,
    /// 同索引、同字节的幂等重传。
    AlreadyPresent,
}

/// 一次成功接收的块。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartReceipt {
    /// 登记结果。
    pub write: PartWrite,
    /// 块索引。
    pub index: u64,
    /// 块长度。
    pub length: u64,
    /// 块 SHA-256。
    pub hash: ContentDigest,
    /// 登记后的会话状态（块齐之前保持 `open`）。
    pub phase: UploadPhase,
}

/// 创建会话的输入（身份与到期时间由调用方给出，核心不取随机数、不读时钟）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeginRequest {
    /// 快照总大小。
    pub total_size: u64,
    /// 快照总 SHA-256。
    pub hash: ContentDigest,
    /// 客户端希望的块大小；0 表示由服务端选择。
    pub chunk_size: u64,
}

/// 校验后的创建计划。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeginPlan {
    /// 实际使用的块大小。
    pub chunk_size: u64,
    /// 块几何。
    pub geometry: ChunkGeometry,
}

/// 接收失败的分类（HTTP 层据此映射状态码与错误码）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadFailure {
    /// 总大小超过策略上限。
    TotalTooLarge {
        /// 策略上限。
        limit: u64,
        /// 请求的大小。
        actual: u64,
    },
    /// 请求的块大小超过策略上限。
    ChunkTooLarge {
        /// 策略上限。
        limit: u64,
        /// 请求的大小。
        requested: u64,
    },
    /// 块索引超出块几何范围（越界索引不得触发任何 I/O）。
    IndexOutOfRange {
        /// 请求的索引。
        index: u64,
        /// 会话块数。
        part_count: u64,
    },
    /// 块长度与几何给出的期望长度不符（含载荷过大）。
    LengthMismatch {
        /// 块索引。
        index: u64,
        /// 期望长度。
        expected: u64,
        /// 实际长度。
        actual: u64,
    },
    /// 字节的 SHA-256 与声明值不符（design §15.4 `HASH_MISMATCH`）。
    DigestMismatch {
        /// 块索引。
        index: u64,
        /// 声明的摘要。
        expected: ContentDigest,
        /// 实测的摘要。
        actual: ContentDigest,
    },
    /// 同一索引已登记不同内容（design §13.1：不能混合两个本地 generation）。
    PartConflict {
        /// 块索引。
        index: u64,
    },
    /// 会话不再接收块。
    PartsClosed {
        /// 当前状态。
        phase: UploadPhase,
    },
    /// 会话不存在、不属于该 Root 或不属于该登录会话（三者不区分）。
    UnknownUpload,
    /// Root 不在调用方授权范围。
    OutOfScope,
    /// 会话行不满足本层的类型前提（块大小为 0）。
    MalformedSession,
    /// 存储/暂存失败（含满盘），必须暴露原因而不是当作成功。
    Storage {
        /// 失败细节（不回显内容）。
        detail: String,
    },
}

impl std::fmt::Display for UploadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TotalTooLarge { limit, actual } => {
                write!(f, "总大小 {actual} 超过上限 {limit}")
            }
            Self::ChunkTooLarge { limit, requested } => {
                write!(f, "块大小 {requested} 超过上限 {limit}")
            }
            Self::IndexOutOfRange { index, part_count } => {
                write!(f, "块索引 {index} 超出块数 {part_count}")
            }
            Self::LengthMismatch {
                index,
                expected,
                actual,
            } => write!(f, "块 {index} 长度应为 {expected}，实际 {actual}"),
            Self::DigestMismatch {
                index,
                expected,
                actual,
            } => write!(
                f,
                "块 {index} 摘要不符：期望 {}，实测 {}",
                expected.to_hex(),
                actual.to_hex()
            ),
            Self::PartConflict { index } => {
                write!(f, "块 {index} 已用不同内容登记过")
            }
            Self::PartsClosed { phase } => {
                write!(f, "上传会话已是 {}，不再接收块", phase.as_text())
            }
            Self::UnknownUpload => f.write_str("上传会话不存在"),
            Self::OutOfScope => f.write_str("Root 不在调用方授权范围"),
            Self::MalformedSession => f.write_str("上传会话的块大小无效"),
            Self::Storage { detail } => write!(f, "上传存储失败：{detail}"),
        }
    }
}

impl std::error::Error for UploadFailure {}

impl From<GeometryError> for UploadFailure {
    fn from(_: GeometryError) -> Self {
        Self::MalformedSession
    }
}

/// 校验创建请求并给出实际块大小与块几何。
///
/// 服务端把客户端请求的块大小调到 `[1, max_chunk_size]`：超出上限的请求被拒绝（而不是悄悄改成
/// 别的值，让客户端按自己以为的块边界算块），0 表示由服务端选择默认值。
///
/// # Errors
///
/// 总大小超过上限、块大小超过上限时返回对应失败，都不写入任何状态。
pub fn plan_begin(
    policy: UploadPolicy,
    request: &BeginRequest,
) -> Result<BeginPlan, UploadFailure> {
    if request.total_size > policy.max_total_size {
        return Err(UploadFailure::TotalTooLarge {
            limit: policy.max_total_size,
            actual: request.total_size,
        });
    }
    if request.chunk_size > policy.max_chunk_size {
        return Err(UploadFailure::ChunkTooLarge {
            limit: policy.max_chunk_size,
            requested: request.chunk_size,
        });
    }
    let chunk_size = if request.chunk_size == 0 {
        policy.default_chunk_size
    } else {
        request.chunk_size
    };
    let geometry = ChunkGeometry::new(request.total_size, chunk_size)?;
    Ok(BeginPlan {
        chunk_size,
        geometry,
    })
}

/// 接收一个块：校验索引/长度/摘要，先持久化字节，再登记块行。
///
/// 顺序见模块文档；越界索引、长度不符与摘要不符都在写盘之前返回，不留下任何暂存文件。
///
/// # Errors
///
/// 会话不存在/越权 → [`UploadFailure::UnknownUpload`]；会话已不接收块 →
/// [`UploadFailure::PartsClosed`]；索引越界 → [`UploadFailure::IndexOutOfRange`]；长度不符 →
/// [`UploadFailure::LengthMismatch`]；摘要不符 → [`UploadFailure::DigestMismatch`]；同索引不同
/// 字节 → [`UploadFailure::PartConflict`]；落盘/登记失败 → [`UploadFailure::Storage`]。
pub async fn receive_part<S, T>(
    store: &mut S,
    staging: &mut T,
    upload: UploadId,
    index: u64,
    declared: ContentDigest,
    bytes: &[u8],
) -> Result<PartReceipt, UploadFailure>
where
    S: UploadSessionStore,
    T: PartStaging,
{
    let session = store.load(upload).await?;
    if !session.phase.accepts_parts() {
        return Err(UploadFailure::PartsClosed {
            phase: session.phase,
        });
    }
    let geometry = session.geometry()?;
    let expected_length = geometry
        .part_length(index)
        .ok_or(UploadFailure::IndexOutOfRange {
            index,
            part_count: geometry.part_count(),
        })?;
    let actual_length = bytes.len() as u64;
    if actual_length != expected_length {
        return Err(UploadFailure::LengthMismatch {
            index,
            expected: expected_length,
            actual: actual_length,
        });
    }
    let actual_hash = ContentDigest::from_bytes(hash::digest_of(bytes));
    if actual_hash != declared {
        return Err(UploadFailure::DigestMismatch {
            index,
            expected: declared,
            actual: actual_hash,
        });
    }

    // 已登记过同一索引：字节一致就是幂等重传（不重写磁盘），否则拒绝且不覆盖旧块。
    if let Some(existing) = store.existing_part(upload, index).await? {
        if existing.hash == actual_hash && existing.length == actual_length {
            return Ok(PartReceipt {
                write: PartWrite::AlreadyPresent,
                index,
                length: existing.length,
                hash: existing.hash,
                phase: session.phase,
            });
        }
        return Err(UploadFailure::PartConflict { index });
    }

    // 先落盘并 fsync，再登记：登记失败只会留下孤儿暂存文件，不会留下“已登记但无字节”。
    let path = staging.store_part(upload, index, bytes).await?;
    let part = StoredPart {
        index,
        length: actual_length,
        hash: actual_hash,
        path,
    };
    let write = match store.record_part(upload, &part).await {
        Ok(write) => write,
        // 并发重传：另一个请求先登记了同一索引。摘要已验证一致，因此按幂等处理。
        Err(UploadFailure::PartConflict { .. }) => {
            if let Some(existing) = store.existing_part(upload, index).await? {
                if existing.hash == actual_hash && existing.length == actual_length {
                    PartWrite::AlreadyPresent
                } else {
                    return Err(UploadFailure::PartConflict { index });
                }
            } else {
                return Err(UploadFailure::PartConflict { index });
            }
        }
        Err(other) => return Err(other),
    };
    Ok(PartReceipt {
        write,
        index,
        length: actual_length,
        hash: actual_hash,
        phase: session.phase,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn digest(bytes: &[u8]) -> ContentDigest {
        ContentDigest::from_bytes(hash::digest_of(bytes))
    }

    fn session(total: u64, chunk: u64, phase: UploadPhase) -> UploadSession {
        UploadSession {
            upload: UploadId::from_bytes([0x41; 16]),
            root: RootId::from_bytes([0x42; 16]),
            session: SessionId::from_bytes([0x43; 16]),
            total_size: total,
            hash: digest(b"whole"),
            chunk_size: chunk,
            phase,
            expires_at: UtcTimestamp::from_unix_seconds(1_700_000_000).expect("时间"),
        }
    }

    /// 假会话存储：记录调用顺序与登记结果。
    #[derive(Default)]
    struct FakeStore {
        session: Option<UploadSession>,
        parts: BTreeMap<u64, StoredPart>,
        calls: Vec<String>,
        fail_record: bool,
    }

    #[allow(async_fn_in_trait)]
    impl UploadSessionStore for FakeStore {
        async fn load(&mut self, _upload: UploadId) -> Result<UploadSession, UploadFailure> {
            self.calls.push("load".to_owned());
            self.session.clone().ok_or(UploadFailure::UnknownUpload)
        }

        async fn existing_part(
            &mut self,
            _upload: UploadId,
            index: u64,
        ) -> Result<Option<StoredPart>, UploadFailure> {
            self.calls.push(format!("existing:{index}"));
            Ok(self.parts.get(&index).cloned())
        }

        async fn record_part(
            &mut self,
            _upload: UploadId,
            part: &StoredPart,
        ) -> Result<PartWrite, UploadFailure> {
            self.calls.push(format!("record:{}", part.index));
            if self.fail_record {
                return Err(UploadFailure::Storage {
                    detail: "模拟登记失败".to_owned(),
                });
            }
            self.parts.insert(part.index, part.clone());
            Ok(PartWrite::Recorded)
        }
    }

    #[derive(Default)]
    struct FakeStaging {
        writes: Vec<(u64, usize)>,
    }

    #[allow(async_fn_in_trait)]
    impl PartStaging for FakeStaging {
        async fn store_part(
            &mut self,
            _upload: UploadId,
            index: u64,
            bytes: &[u8],
        ) -> Result<String, UploadFailure> {
            self.writes.push((index, bytes.len()));
            Ok(format!("uploads/41/{index}"))
        }
    }

    #[tokio::test]
    async fn begin_plan_rejects_oversize_totals_and_chosen_chunk_sizes() {
        let policy = UploadPolicy::standard();
        let plan = plan_begin(
            policy,
            &BeginRequest {
                total_size: 10,
                hash: digest(b"x"),
                chunk_size: 4,
            },
        )
        .expect("合法请求");
        assert_eq!(plan.chunk_size, 4);
        assert_eq!(plan.geometry.part_count(), 3);

        // 0 表示由服务端选默认块大小。
        let plan = plan_begin(
            policy,
            &BeginRequest {
                total_size: 10,
                hash: digest(b"x"),
                chunk_size: 0,
            },
        )
        .expect("合法请求");
        assert_eq!(plan.chunk_size, policy.default_chunk_size);

        assert_eq!(
            plan_begin(
                policy,
                &BeginRequest {
                    total_size: policy.max_total_size + 1,
                    hash: digest(b"x"),
                    chunk_size: 4,
                }
            ),
            Err(UploadFailure::TotalTooLarge {
                limit: policy.max_total_size,
                actual: policy.max_total_size + 1,
            })
        );
        assert_eq!(
            plan_begin(
                policy,
                &BeginRequest {
                    total_size: 10,
                    hash: digest(b"x"),
                    chunk_size: policy.max_chunk_size + 1,
                }
            ),
            Err(UploadFailure::ChunkTooLarge {
                limit: policy.max_chunk_size,
                requested: policy.max_chunk_size + 1,
            })
        );
    }

    #[tokio::test]
    async fn receive_part_persists_before_recording() {
        let mut store = FakeStore {
            session: Some(session(6, 4, UploadPhase::Open)),
            ..FakeStore::default()
        };
        let mut staging = FakeStaging::default();
        let first = vec![1_u8; 4];
        let receipt = receive_part(
            &mut store,
            &mut staging,
            UploadId::from_bytes([0x41; 16]),
            0,
            digest(&first),
            &first,
        )
        .await
        .expect("接收第一块");
        assert_eq!(receipt.write, PartWrite::Recorded);
        assert_eq!(receipt.length, 4);
        assert_eq!(staging.writes, vec![(0, 4)]);
        assert_eq!(store.parts.len(), 1);
        assert_eq!(
            store.calls,
            vec![
                "load".to_owned(),
                "existing:0".to_owned(),
                "record:0".to_owned()
            ],
            "顺序必须是读会话 → 查已收 → 落盘 → 登记"
        );

        // 第二块是最后一块，长度由几何决定（6 = 4 + 2）。
        let last = vec![2_u8; 2];
        receive_part(
            &mut store,
            &mut staging,
            UploadId::from_bytes([0x41; 16]),
            1,
            digest(&last),
            &last,
        )
        .await
        .expect("接收最后一块");
        assert_eq!(staging.writes, vec![(0, 4), (1, 2)]);
    }

    #[tokio::test]
    async fn duplicate_part_is_idempotent_or_conflicting() {
        let mut store = FakeStore {
            session: Some(session(4, 4, UploadPhase::Open)),
            ..FakeStore::default()
        };
        let mut staging = FakeStaging::default();
        let bytes = vec![7_u8; 4];
        let upload = UploadId::from_bytes([0x41; 16]);
        receive_part(&mut store, &mut staging, upload, 0, digest(&bytes), &bytes)
            .await
            .expect("首次登记");
        let receipt = receive_part(&mut store, &mut staging, upload, 0, digest(&bytes), &bytes)
            .await
            .expect("幂等重传");
        assert_eq!(receipt.write, PartWrite::AlreadyPresent);
        assert_eq!(staging.writes.len(), 1, "幂等重传不得重写磁盘");

        let other = vec![9_u8; 4];
        let conflict = receive_part(&mut store, &mut staging, upload, 0, digest(&other), &other)
            .await
            .expect_err("不同字节必须拒绝");
        assert_eq!(conflict, UploadFailure::PartConflict { index: 0 });
        assert_eq!(store.parts[&0].hash, digest(&bytes), "旧块不得被覆盖");
    }

    #[tokio::test]
    async fn index_length_and_digest_are_rejected_before_any_write() {
        let upload = UploadId::from_bytes([0x41; 16]);
        let mut store = FakeStore {
            session: Some(session(6, 4, UploadPhase::Open)),
            ..FakeStore::default()
        };
        let mut staging = FakeStaging::default();

        let bytes = vec![1_u8; 4];
        let out_of_range =
            receive_part(&mut store, &mut staging, upload, 2, digest(&bytes), &bytes).await;
        assert_eq!(
            out_of_range.expect_err("越界索引"),
            UploadFailure::IndexOutOfRange {
                index: 2,
                part_count: 2
            }
        );

        let short = vec![1_u8; 3];
        let length =
            receive_part(&mut store, &mut staging, upload, 0, digest(&short), &short).await;
        assert_eq!(
            length.expect_err("长度不符"),
            UploadFailure::LengthMismatch {
                index: 0,
                expected: 4,
                actual: 3
            }
        );

        let bytes = vec![1_u8; 4];
        let wrong = receive_part(
            &mut store,
            &mut staging,
            upload,
            0,
            digest("别的字节".as_bytes()),
            &bytes,
        )
        .await;
        match wrong.expect_err("摘要不符") {
            UploadFailure::DigestMismatch {
                index,
                expected,
                actual,
            } => {
                assert_eq!(index, 0);
                assert_eq!(expected, digest("别的字节".as_bytes()));
                assert_eq!(actual, digest(&bytes));
            }
            other => panic!("期望 DigestMismatch，实际 {other:?}"),
        }
        assert!(staging.writes.is_empty(), "校验失败不得写盘");
        assert!(store.parts.is_empty(), "校验失败不得登记");
    }

    #[tokio::test]
    async fn closed_session_rejects_parts() {
        let upload = UploadId::from_bytes([0x41; 16]);
        let bytes = vec![1_u8; 4];
        for phase in [
            UploadPhase::Complete,
            UploadPhase::Finalized,
            UploadPhase::Aborted,
        ] {
            let mut store = FakeStore {
                session: Some(session(4, 4, phase)),
                ..FakeStore::default()
            };
            let mut staging = FakeStaging::default();
            assert_eq!(
                receive_part(&mut store, &mut staging, upload, 0, digest(&bytes), &bytes)
                    .await
                    .expect_err("终态会话"),
                UploadFailure::PartsClosed { phase }
            );
            assert!(staging.writes.is_empty());
        }
    }

    #[tokio::test]
    async fn staging_failure_never_records_the_part() {
        let upload = UploadId::from_bytes([0x41; 16]);
        let mut store = FakeStore {
            session: Some(session(4, 4, UploadPhase::Open)),
            ..FakeStore::default()
        };
        struct FailingStaging;
        #[allow(async_fn_in_trait)]
        impl PartStaging for FailingStaging {
            async fn store_part(
                &mut self,
                _upload: UploadId,
                _index: u64,
                _bytes: &[u8],
            ) -> Result<String, UploadFailure> {
                Err(UploadFailure::Storage {
                    detail: "no_space".to_owned(),
                })
            }
        }
        let mut staging = FailingStaging;
        let bytes = vec![1_u8; 4];
        let failure = receive_part(&mut store, &mut staging, upload, 0, digest(&bytes), &bytes)
            .await
            .expect_err("落盘失败");
        assert!(matches!(failure, UploadFailure::Storage { .. }));
        assert!(store.parts.is_empty(), "落盘失败不得登记块");
    }

    #[tokio::test]
    async fn unknown_upload_and_record_failure_are_visible() {
        let upload = UploadId::from_bytes([0x41; 16]);
        let bytes = vec![1_u8; 4];
        let mut store = FakeStore::default();
        let mut staging = FakeStaging::default();
        assert_eq!(
            receive_part(&mut store, &mut staging, upload, 0, digest(&bytes), &bytes)
                .await
                .expect_err("未知会话"),
            UploadFailure::UnknownUpload
        );

        let mut store = FakeStore {
            session: Some(session(4, 4, UploadPhase::Open)),
            fail_record: true,
            ..FakeStore::default()
        };
        let failure = receive_part(&mut store, &mut staging, upload, 0, digest(&bytes), &bytes)
            .await
            .expect_err("登记失败");
        assert!(matches!(failure, UploadFailure::Storage { .. }));
        assert!(store.parts.is_empty());
        assert_eq!(staging.writes.len(), 1, "登记失败时字节已落盘（孤儿暂存）");
    }
}
