//! 上传执行：固定本地快照 → 申请/复核会话 → 只传服务端仍缺的块 → finalize 校验（design §13.1—13.3）。
//!
//! # 不变量
//!
//! - **一个会话只对应一份内容快照**：整份 SHA-256 由本地流式计算，服务端回报的会话摘要与总大小
//!   必须与它完全一致，否则立即拒绝（[`UploadErrorKind::SessionMismatch`]）。SHA-256 相同即内容相同，
//!   因此即使本地代际换了、内容没变，续传也不会把两份内容拼在一起；反过来，内容变了就绝不会继续
//!   使用旧会话。
//! - **续传只听服务端**：缺哪些块由 `GET …/uploads/{upload}` 的真实已收块决定（design §9.3
//!   「不能只相信客户端位图」）。核心还会核对“缺失区间”与“已收块”是否自相矛盾，避免补错块。
//! - **块摘要必须被服务端原样收下**：`PUT` 带块摘要，回执的索引、长度与摘要都要与本地一致；
//!   不一致就重读本地快照重传该块（[`UploadOptions::part_attempts`]），持续不一致则报
//!   [`UploadErrorKind::PartRejected`]，不 finalize。
//! - **finalize 不是 committed**：返回的只是可提交内容引用（design §15.2）。条目 head 由上层用
//!   `OpId` 按 CAS 提交，再等服务端确认 committed；本模块没有也不该有“提交条目”的入口。
//! - **没有跨版本块增量**：这里只做整份内容的固定块续传。内容寻址去重（`objects`）与跨版本复用
//!   都不由本模块实现（design §13.1「不等同于块级增量同步」）。

use crate::bitmap::PartRange;
use crate::chunk::{ChunkGeometry, GeometryError};
use crate::concurrency::{CancelToken, bounded_join};
use crate::hash::{DIGEST_BYTES, digest_of};
use crate::ports::{
    FinalizedContent, PartAck, PortError, PortFuture, SessionSpec, SnapshotSource,
    UploadProgressInfo, UploadSessionInfo, UploadSessionState, UploadTarget,
};
use crate::stream::{ChunkReader, StreamError, streamed_digest};

/// 首发建议块大小：4 MiB（design §13.1）。服务端可以按自己的上限调整，客户端以回执为准。
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// 单个文件的块并发上限（design §13.3 的可调参数，不是产品硬上限）。
pub const DEFAULT_PART_CONCURRENCY: usize = 4;

/// 每块的尝试次数：首次 + 一次重传。错块必须重传，但无限重试会把“快照在变”伪装成网络问题。
pub const DEFAULT_PART_ATTEMPTS: usize = 2;

/// 上传执行参数。
#[derive(Clone, Debug)]
pub struct UploadOptions {
    /// 希望服务端使用的块大小（服务端可以调整）。
    pub chunk_size: u64,
    /// 同时在飞行的块数上限（至少 1）。
    pub part_concurrency: usize,
    /// 每块的最大尝试次数（至少 1）。
    pub part_attempts: usize,
    /// 取消开关。
    pub cancel: CancelToken,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            part_concurrency: DEFAULT_PART_CONCURRENCY,
            part_attempts: DEFAULT_PART_ATTEMPTS,
            cancel: CancelToken::new(),
        }
    }
}

impl UploadOptions {
    /// 用给定块大小与默认并发/尝试次数构造。
    #[must_use]
    pub fn new(chunk_size: u64) -> Self {
        Self {
            chunk_size,
            ..Self::default()
        }
    }

    /// 设置块并发上限。
    #[must_use]
    pub fn with_part_concurrency(mut self, part_concurrency: usize) -> Self {
        self.part_concurrency = part_concurrency;
        self
    }

    /// 设置每块尝试次数。
    #[must_use]
    pub fn with_part_attempts(mut self, part_attempts: usize) -> Self {
        self.part_attempts = part_attempts;
        self
    }

    /// 挂上取消开关。
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// 参数是否可用：块大小非 0、并发与尝试次数至少 1。
    fn validate(&self) -> Result<(), UploadErrorKind> {
        if self.chunk_size == 0 {
            return Err(UploadErrorKind::InvalidOptions {
                detail: "块大小必须大于 0".to_owned(),
            });
        }
        if self.part_concurrency == 0 || self.part_attempts == 0 {
            return Err(UploadErrorKind::InvalidOptions {
                detail: "块并发与每块尝试次数都至少为 1".to_owned(),
            });
        }
        Ok(())
    }
}

/// 固定下来的一份本地内容快照：本地身份 + 总大小 + 整份 SHA-256。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadSnapshot {
    identity: String,
    total_size: u64,
    digest: [u8; DIGEST_BYTES],
}

impl UploadSnapshot {
    /// 构造；身份用于进度绑定与诊断，必须非空且不含控制字符。
    ///
    /// # Errors
    ///
    /// 身份非法时返回 [`UploadErrorKind::InvalidSnapshot`]。
    pub fn new(
        identity: impl Into<String>,
        total_size: u64,
        digest: [u8; DIGEST_BYTES],
    ) -> Result<Self, UploadErrorKind> {
        let identity = identity.into();
        if identity.is_empty() || identity.chars().any(char::is_control) {
            return Err(UploadErrorKind::InvalidSnapshot {
                detail: "快照身份不能为空且不能包含控制字符".to_owned(),
            });
        }
        Ok(Self {
            identity,
            total_size,
            digest,
        })
    }

    /// 本地快照身份。
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// 快照总字节数。
    #[must_use]
    pub const fn total_size(&self) -> u64 {
        self.total_size
    }

    /// 整份内容的 SHA-256。
    #[must_use]
    pub const fn digest(&self) -> [u8; DIGEST_BYTES] {
        self.digest
    }
}

/// 断线后继续同一次上传所需的恢复信息：绑定哪份快照、哪个会话。
///
/// 调用方应当在会话建立后立刻持久化它（例如客户端的 `pending_ops`/operations 行），失败或重启后
/// 原样传回 [`run`]，就能只补缺失块。**不要**在快照内容变化后继续用它：核心会先比对整份摘要，
/// 不一致直接拒绝。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadResume {
    snapshot: UploadSnapshot,
    session: String,
}

impl UploadResume {
    /// 构造；会话身份必须非空且不含控制字符。
    ///
    /// # Errors
    ///
    /// 会话身份非法时返回 [`UploadErrorKind::InvalidOptions`]。
    pub fn new(
        snapshot: UploadSnapshot,
        session: impl Into<String>,
    ) -> Result<Self, UploadErrorKind> {
        let session = session.into();
        if session.is_empty() || session.chars().any(char::is_control) {
            return Err(UploadErrorKind::InvalidOptions {
                detail: "上传会话身份不能为空且不能包含控制字符".to_owned(),
            });
        }
        Ok(Self { snapshot, session })
    }

    /// 绑定的快照。
    #[must_use]
    pub const fn snapshot(&self) -> &UploadSnapshot {
        &self.snapshot
    }

    /// 上传会话身份。
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    fn matches(&self, snapshot: &UploadSnapshot) -> Result<(), UploadErrorKind> {
        if self.snapshot.identity != snapshot.identity {
            return Err(UploadErrorKind::SnapshotChanged {
                detail: format!(
                    "本地快照身份已从 {} 变为 {}",
                    self.snapshot.identity, snapshot.identity
                ),
            });
        }
        if self.snapshot.total_size != snapshot.total_size {
            return Err(UploadErrorKind::SnapshotChanged {
                detail: format!(
                    "快照总大小已从 {} 变为 {}",
                    self.snapshot.total_size, snapshot.total_size
                ),
            });
        }
        if self.snapshot.digest != snapshot.digest {
            return Err(UploadErrorKind::SnapshotChanged {
                detail: format!(
                    "快照内容摘要已从 {} 变为 {}",
                    crate::hash::to_hex(&self.snapshot.digest),
                    crate::hash::to_hex(&snapshot.digest)
                ),
            });
        }
        Ok(())
    }
}

/// finalize 返回的可提交内容引用（**不是**已提交版本）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentReference {
    /// 整份内容的 SHA-256。
    pub digest: [u8; DIGEST_BYTES],
    /// 内容字节数。
    pub size: u64,
    /// 块数。
    pub part_count: u64,
    /// 对象区内的规范相对路径（服务端内部路径，客户端只原样带回提交请求）。
    pub storage_path: String,
    /// 是否为幂等重放（会话本来就已 finalize）。
    pub replayed: bool,
}

/// 一次成功上传的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadOutcome {
    /// 可提交内容引用。提交与 committed 确认由上层完成（design §15.2、I01）。
    pub content: ContentReference,
    /// 上传会话身份。
    pub session: String,
    /// 本次实际发送并确认的块数（不含服务端已收块）。
    pub uploaded_parts: u64,
    /// 服务端已经收下、因此没有再传的块数。
    pub reused_parts: u64,
    /// 本次是否从已有会话续传。
    pub resumed: bool,
}

/// 上传失败的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadErrorKind {
    /// 参数非法（块大小 0、并发/尝试次数 0、身份非法）。
    InvalidOptions {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 快照身份非法。
    InvalidSnapshot {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 读取固定快照失败（I/O 错误或短读）。
    ReadSnapshot {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 续传时本地快照已不是同一份内容：必须重新开始一次上传，不能继续旧会话。
    SnapshotChanged {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 服务端会话与本地固定快照不符（另一个 generation、块几何自相矛盾、缺失区间与已收块矛盾）。
    SessionMismatch {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 会话已被放弃，不能再传块。
    SessionClosed {
        /// 会话状态。
        state: UploadSessionState,
    },
    /// 服务端登记的块与本地发送的不一致：重传后仍不一致，不 finalize。
    PartRejected {
        /// 块索引。
        index: u64,
        /// 本地计算的块摘要。
        expected: [u8; DIGEST_BYTES],
        /// 服务端登记/回报的块摘要。
        actual: [u8; DIGEST_BYTES],
    },
    /// finalize 返回的内容与固定快照不符：不提交。
    FinalizedMismatch {
        /// 面向诊断的原因。
        detail: String,
    },
    /// 调用方取消。
    Cancelled,
    /// 端口失败（网络、认证、服务端拒绝、本地 I/O）；分类保留原样，交给调度器决定。
    Port(PortError),
}

impl UploadErrorKind {
    /// 退避后原样重试整个上传是否可能成功。
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Port(error) if error.is_retryable())
    }

    /// 是否被调用方取消。
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled | Self::Port(PortError::Cancelled))
    }

    /// 是否需要用户重新登录。
    #[must_use]
    pub const fn needs_reauth(&self) -> bool {
        matches!(self, Self::Port(error) if error.needs_reauth())
    }

    /// 端口错误（不是端口错误时为 `None`）。
    #[must_use]
    pub const fn port_error(&self) -> Option<&PortError> {
        match self {
            Self::Port(error) => Some(error),
            _ => None,
        }
    }
}

impl std::fmt::Display for UploadErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOptions { detail } | Self::InvalidSnapshot { detail } => {
                f.write_str(detail)
            }
            Self::ReadSnapshot { detail } => write!(f, "读取本地快照失败：{detail}"),
            Self::SnapshotChanged { detail } => write!(f, "快照已变化，必须重新上传：{detail}"),
            Self::SessionMismatch { detail } => write!(f, "会话与固定快照不符：{detail}"),
            Self::SessionClosed { state } => {
                write!(f, "上传会话状态为 {}，不能再传块", state.as_str())
            }
            Self::PartRejected {
                index,
                expected,
                actual,
            } => write!(
                f,
                "第 {index} 块的摘要不一致：发送 {}，服务端回报 {}",
                crate::hash::to_hex(expected),
                crate::hash::to_hex(actual)
            ),
            Self::FinalizedMismatch { detail } => write!(f, "finalize 结果与快照不符：{detail}"),
            Self::Cancelled => f.write_str("已取消"),
            Self::Port(error) => write!(f, "端口失败：{error}"),
        }
    }
}

impl std::error::Error for UploadErrorKind {}

/// 一次上传失败：原因 + 可续传的会话信息（会话已建立时）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadFailure {
    kind: UploadErrorKind,
    // 装箱：`Result` 的错误分支不该比成功分支大一个数量级（成功结果是几个标量）。
    resume: Option<Box<UploadResume>>,
}

impl UploadFailure {
    /// 只有原因、没有会话可续（例如参数或快照读失败）。
    #[must_use]
    pub fn new(kind: UploadErrorKind) -> Self {
        Self { kind, resume: None }
    }

    /// 原因 + 可续传信息。
    #[must_use]
    pub fn with_resume(kind: UploadErrorKind, resume: UploadResume) -> Self {
        Self {
            kind,
            resume: Some(Box::new(resume)),
        }
    }

    /// 失败原因。
    #[must_use]
    pub const fn kind(&self) -> &UploadErrorKind {
        &self.kind
    }

    /// 可直接持久化、下次传回 [`run`] 的续传信息。
    #[must_use]
    pub fn resume(&self) -> Option<&UploadResume> {
        self.resume.as_deref()
    }

    /// 取出失败原因。
    #[must_use]
    pub fn into_kind(self) -> UploadErrorKind {
        self.kind
    }
}

impl std::fmt::Display for UploadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for UploadFailure {}

impl From<UploadErrorKind> for UploadFailure {
    fn from(kind: UploadErrorKind) -> Self {
        Self::new(kind)
    }
}

/// 执行一次上传（新建或续传）。
///
/// 调用顺序：流式算整份摘要 → 校验/申请会话 → 依服务端已收块补缺失块 → finalize 并核对结果。
/// 失败时 [`UploadFailure::resume`] 给出可持久化的续传信息，下次重跑只补缺块。
///
/// # Errors
///
/// 见 [`UploadErrorKind`]：参数/快照非法、快照已变化、会话与快照不符、块被服务端拒绝、finalize
/// 结果不符、取消或端口失败。
pub async fn run(
    source: &dyn SnapshotSource,
    target: &dyn UploadTarget,
    resume: Option<&UploadResume>,
    options: &UploadOptions,
) -> Result<UploadOutcome, UploadFailure> {
    options.validate()?;
    let resumed = resume.is_some();
    let total_size = source.total_size();
    // 整份摘要流式计算：窗口不超过块大小，内存占用与文件大小无关。
    let digest = streamed_digest(
        &SnapshotReader(source),
        total_size,
        options.chunk_size,
        &options.cancel,
    )
    .await
    .map_err(|error| UploadFailure::new(stream_error(error)))?;
    let snapshot = UploadSnapshot::new(source.identity(), total_size, digest)?;

    // 续传前先确认“还是同一份内容快照”，否则绝不碰旧会话。
    if let Some(resume) = resume {
        resume.matches(&snapshot)?;
    }

    let (session, missing) = match resume {
        Some(resume) => {
            let info = target.status(resume.session()).await.map_err(|error| {
                UploadFailure::with_resume(UploadErrorKind::Port(error), resume.clone())
            })?;
            let resume = UploadResume::new(snapshot.clone(), resume.session())?;
            verify_session(&snapshot, &info.session)
                .map_err(|kind| UploadFailure::with_resume(kind, resume.clone()))?;
            let missing = missing_parts(&info)
                .map_err(|kind| UploadFailure::with_resume(kind, resume.clone()))?;
            (info.session, missing)
        }
        None => {
            let spec = SessionSpec {
                total_size: snapshot.total_size(),
                digest: snapshot.digest(),
                chunk_size: options.chunk_size,
            };
            let session = target
                .begin(&spec)
                .await
                .map_err(|error| UploadFailure::new(UploadErrorKind::Port(error)))?;
            verify_session(&snapshot, &session)?;
            let resume = UploadResume::new(snapshot.clone(), session.id.clone())?;
            let geometry = geometry_of(&session)
                .map_err(|kind| UploadFailure::with_resume(kind, resume.clone()))?;
            let missing = match PartRange::new(0, geometry.part_count()) {
                Some(range) => vec![range],
                None => Vec::new(),
            };
            (session, missing)
        }
    };

    let resume = UploadResume::new(snapshot.clone(), session.id.clone())?;
    let geometry =
        geometry_of(&session).map_err(|kind| UploadFailure::with_resume(kind, resume.clone()))?;

    // 服务端已经收下的块：续传时不再发送，计为“复用”；新会话一个也没有。
    let skipped_parts: u64 = missing.iter().map(|range| range.count).sum::<u64>();
    let skipped_parts = geometry.part_count() - skipped_parts;

    let (uploaded_parts, mut reused_parts) = match session.state {
        UploadSessionState::Aborted => {
            return Err(UploadFailure::with_resume(
                UploadErrorKind::SessionClosed {
                    state: session.state,
                },
                resume,
            ));
        }
        UploadSessionState::Open => {
            upload_missing(source, target, &session, geometry, &missing, options)
                .await
                .map_err(|kind| UploadFailure::with_resume(kind, resume.clone()))?
        }
        UploadSessionState::Complete | UploadSessionState::Finalized => {
            // 块已齐（或已 finalize）：状态里不该再报缺块，否则服务端事实自相矛盾。
            if !missing.is_empty() {
                let detail = format!(
                    "会话状态为 {} 但报告 {} 段缺失块",
                    session.state.as_str(),
                    missing.len()
                );
                return Err(UploadFailure::with_resume(
                    UploadErrorKind::SessionMismatch { detail },
                    resume,
                ));
            }
            (0, 0)
        }
    };
    reused_parts += skipped_parts;

    let finalized = target.finalize(&session.id).await.map_err(|error| {
        UploadFailure::with_resume(UploadErrorKind::Port(error), resume.clone())
    })?;
    verify_finalized(&snapshot, geometry, &finalized)
        .map_err(|kind| UploadFailure::with_resume(kind, resume))?;

    Ok(UploadOutcome {
        content: ContentReference {
            digest: finalized.digest,
            size: finalized.size,
            part_count: finalized.part_count,
            storage_path: finalized.storage_path,
            replayed: finalized.replayed,
        },
        session: session.id,
        uploaded_parts,
        reused_parts,
        resumed,
    })
}

/// 上传缺失块，返回（实际发送确认块数，服务端已收块数）。
async fn upload_missing(
    source: &dyn SnapshotSource,
    target: &dyn UploadTarget,
    session: &UploadSessionInfo,
    geometry: ChunkGeometry,
    missing: &[PartRange],
    options: &UploadOptions,
) -> Result<(u64, u64), UploadErrorKind> {
    let indices: Vec<u64> = missing.iter().flat_map(|range| range.indices()).collect();
    if indices.is_empty() {
        return Ok((0, 0));
    }
    let session_id = session.id.as_str();
    let chunk_size = geometry.chunk_size();
    let attempts = options.part_attempts;
    let cancel = &options.cancel;

    let mut futures = Vec::with_capacity(indices.len());
    for index in indices {
        let length =
            geometry
                .part_length(index)
                .ok_or_else(|| UploadErrorKind::SessionMismatch {
                    detail: format!("第 {index} 块的索引超出会话几何"),
                })?;
        let offset =
            index
                .checked_mul(chunk_size)
                .ok_or_else(|| UploadErrorKind::SessionMismatch {
                    detail: "块偏移乘法溢出".to_owned(),
                })?;
        futures.push(async move {
            if cancel.is_cancelled() {
                return Err(UploadErrorKind::Cancelled);
            }
            let mut last: Option<UploadErrorKind> = None;
            for _ in 0..attempts {
                let bytes = source.read(offset, length).await.map_err(|error| {
                    UploadErrorKind::Port(PortError::Local {
                        kind: crate::ports::LocalErrorKind::Read,
                        detail: format!("读取第 {index} 块失败：{error}"),
                    })
                })?;
                if bytes.len() as u64 != length {
                    return Err(UploadErrorKind::ReadSnapshot {
                        detail: format!(
                            "第 {index} 块应为 {length} 字节，实际读到 {} 字节",
                            bytes.len()
                        ),
                    });
                }
                let digest = digest_of(&bytes);
                let ack: PartAck = target
                    .put_part(session_id, index, &digest, &bytes)
                    .await
                    .map_err(UploadErrorKind::Port)?;
                if ack.index == index && ack.length == length && ack.digest == digest {
                    return Ok((ack.already_present, index));
                }
                // 服务端收下的不是这份字节：重读快照重传（错块必须重传，不能当成已收）。
                last = Some(UploadErrorKind::PartRejected {
                    index,
                    expected: digest,
                    actual: ack.digest,
                });
            }
            Err(last.unwrap_or(UploadErrorKind::Cancelled))
        });
    }

    let outcome = bounded_join(futures, options.part_concurrency).await;
    if let Some((_, kind)) = outcome.failure {
        return Err(kind);
    }
    let mut uploaded = 0_u64;
    let mut reused = 0_u64;
    for (_, (already_present, _)) in outcome.completed {
        if already_present {
            reused += 1;
        } else {
            uploaded += 1;
        }
    }
    Ok((uploaded, reused))
}

/// 会话事实必须与固定快照一致：总大小、整份摘要、块数与块几何。
fn verify_session(
    snapshot: &UploadSnapshot,
    session: &UploadSessionInfo,
) -> Result<(), UploadErrorKind> {
    if session.total_size != snapshot.total_size() {
        return Err(UploadErrorKind::SessionMismatch {
            detail: format!(
                "会话总大小为 {}，快照为 {}",
                session.total_size,
                snapshot.total_size()
            ),
        });
    }
    if session.digest != snapshot.digest() {
        return Err(UploadErrorKind::SessionMismatch {
            detail: format!(
                "会话内容是 {}，快照是 {}（不是同一份内容，不能混用）",
                crate::hash::to_hex(&session.digest),
                crate::hash::to_hex(&snapshot.digest())
            ),
        });
    }
    let geometry = geometry_of(session)?;
    if geometry.part_count() != session.part_count {
        return Err(UploadErrorKind::SessionMismatch {
            detail: format!(
                "会话报告 {} 块，按几何应为 {} 块",
                session.part_count,
                geometry.part_count()
            ),
        });
    }
    Ok(())
}

/// 从会话事实取块几何；块大小为 0 或块数溢出都算会话不可用。
fn geometry_of(session: &UploadSessionInfo) -> Result<ChunkGeometry, UploadErrorKind> {
    ChunkGeometry::new(session.total_size, session.chunk_size).map_err(|error: GeometryError| {
        UploadErrorKind::SessionMismatch {
            detail: format!("会话块几何非法：{error}"),
        }
    })
}

/// 由服务端已收块推出缺失区间，并核对“已收块”与“缺失区间”自洽。
///
/// 这条检查防止按错误的服务端摘要补块：缺失区间必须恰好等于已收块之外的部分。
fn missing_parts(info: &UploadProgressInfo) -> Result<Vec<PartRange>, UploadErrorKind> {
    let geometry = geometry_of(&info.session)?;
    let mut bitmap = crate::bitmap::PartBitmap::empty(geometry);
    for part in &info.received {
        let expected =
            geometry
                .part_length(part.index)
                .ok_or_else(|| UploadErrorKind::SessionMismatch {
                    detail: format!("服务端报告了越界块 {}", part.index),
                })?;
        if part.length != expected {
            return Err(UploadErrorKind::SessionMismatch {
                detail: format!(
                    "服务端第 {} 块长度为 {}，几何要求 {expected}",
                    part.index, part.length
                ),
            });
        }
        let fresh = bitmap
            .mark(part.index)
            .map_err(|error| UploadErrorKind::SessionMismatch {
                detail: format!("服务端已收块无法写入位图：{error}"),
            })?;
        if !fresh {
            return Err(UploadErrorKind::SessionMismatch {
                detail: format!("服务端重复报告第 {} 块", part.index),
            });
        }
    }
    let expected = bitmap.missing();
    if expected != info.missing {
        return Err(UploadErrorKind::SessionMismatch {
            detail: "服务端缺失区间与已收块不一致".to_owned(),
        });
    }
    Ok(expected)
}

/// finalize 的结果必须与固定快照逐项相符。
fn verify_finalized(
    snapshot: &UploadSnapshot,
    geometry: ChunkGeometry,
    finalized: &FinalizedContent,
) -> Result<(), UploadErrorKind> {
    if finalized.digest != snapshot.digest() {
        return Err(UploadErrorKind::FinalizedMismatch {
            detail: format!(
                "服务端内容摘要为 {}，快照为 {}",
                crate::hash::to_hex(&finalized.digest),
                crate::hash::to_hex(&snapshot.digest())
            ),
        });
    }
    if finalized.size != snapshot.total_size() {
        return Err(UploadErrorKind::FinalizedMismatch {
            detail: format!(
                "服务端内容长度为 {}，快照为 {}",
                finalized.size,
                snapshot.total_size()
            ),
        });
    }
    if finalized.part_count != geometry.part_count() {
        return Err(UploadErrorKind::FinalizedMismatch {
            detail: format!(
                "服务端报告 {} 块，几何为 {} 块",
                finalized.part_count,
                geometry.part_count()
            ),
        });
    }
    Ok(())
}

fn stream_error(error: StreamError) -> UploadErrorKind {
    match error {
        StreamError::Port(error) => UploadErrorKind::Port(error),
        StreamError::ShortRead {
            offset,
            expected,
            actual,
        } => UploadErrorKind::ReadSnapshot {
            detail: format!("在偏移 {offset} 请求 {expected} 字节，实际读到 {actual} 字节"),
        },
        StreamError::Cancelled => UploadErrorKind::Cancelled,
    }
}

/// 把快照端口适配成流式摘要的读取入口。
struct SnapshotReader<'a>(&'a dyn SnapshotSource);

impl ChunkReader for SnapshotReader<'_> {
    fn read_chunk<'a>(
        &'a self,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        self.0.read(offset, length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concurrency::CancelToken;
    use crate::fakes::{MemorySource, MemoryTarget, StatusOnly, range};
    use crate::hash::digest_of;
    use crate::ports::{ReceivedPart, UploadProgressInfo};

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    fn options(chunk_size: u64) -> UploadOptions {
        UploadOptions::new(chunk_size).with_part_concurrency(2)
    }

    #[tokio::test]
    async fn uploads_every_part_then_returns_a_content_reference() {
        let bytes = content(10);
        let source = MemorySource::new("gen-1", bytes.clone());
        let target = MemoryTarget::new();
        let outcome = run(&source, &target, None, &options(4))
            .await
            .expect("上传成功");

        assert_eq!(outcome.content.digest, digest_of(&bytes));
        assert_eq!(outcome.content.size, 10);
        assert_eq!(outcome.content.part_count, 3);
        assert_eq!(outcome.uploaded_parts, 3);
        assert_eq!(outcome.reused_parts, 0);
        assert!(!outcome.resumed);
        assert!(!outcome.content.replayed);
        assert!(!outcome.session.is_empty());
        assert_eq!(target.finalize_calls(), 1);
        // 块边界由几何决定：4 + 4 + 2。
        let mut lengths: Vec<u64> = target
            .puts()
            .into_iter()
            .map(|(_, length)| length)
            .collect();
        lengths.sort_unstable();
        assert_eq!(lengths, vec![2, 4, 4]);
    }

    #[tokio::test]
    async fn the_snapshot_digest_is_computed_in_bounded_windows() {
        let bytes = content(20);
        let source = MemorySource::new("gen-1", bytes.clone());
        let target = MemoryTarget::new();
        run(&source, &target, None, &options(4))
            .await
            .expect("上传成功");

        let windows: Vec<u64> = source
            .windows()
            .iter()
            .map(|(_, length)| *length)
            .collect::<Vec<_>>();
        // 摘要阶段按块大小流式读取，绝不会要求整份内容一次性读入。
        assert!(windows.iter().all(|length| *length <= 4), "{windows:?}");
        assert!(windows.len() >= 5, "至少要按块读完一轮：{windows:?}");
    }

    #[tokio::test]
    async fn a_disconnect_only_resends_the_missing_parts() {
        let bytes = content(20);
        let source = MemorySource::new("gen-1", bytes.clone());
        let target = MemoryTarget::new();
        target.fail_put_once(2);

        let failure = run(&source, &target, None, &options(4))
            .await
            .expect_err("第一次上传在块 2 断线");
        assert!(failure.kind().is_retryable(), "{failure}");
        assert_eq!(target.finalize_calls(), 0, "断线后不 finalize");
        let resume = failure.resume().expect("会话已建立，可续传").clone();
        let first_puts = target.puts();
        let missing_before: Vec<u64> = target
            .status(resume.session())
            .await
            .expect("会话状态")
            .missing
            .iter()
            .flat_map(|range| range.indices())
            .collect();
        assert!(!missing_before.is_empty(), "断线时必然还缺块");

        let outcome = run(&source, &target, Some(&resume), &options(4))
            .await
            .expect("续传成功");
        assert!(outcome.resumed);
        assert!(target.status_calls() >= 2, "续传先查服务端已收块");
        assert_eq!(
            outcome.uploaded_parts + outcome.reused_parts,
            5,
            "五个块最终都在服务端"
        );
        let status = target.status(&outcome.session).await.expect("会话状态");
        assert!(status.missing.is_empty(), "续传后不再缺块");
        assert_eq!(status.received.len(), 5);

        // 续传只发送“服务端仍缺”的块；断线时被丢弃的请求会重发，但不会有成功块被重发。
        let puts = target.puts();
        let resent = &puts[first_puts.len()..];
        for (index, _) in resent {
            assert!(
                missing_before.contains(index),
                "索引 {index} 不在续传时的缺失清单里：{missing_before:?}"
            );
        }
        let mut resent_indices: Vec<u64> = resent.iter().map(|(index, _)| *index).collect();
        resent_indices.sort_unstable();
        resent_indices.dedup();
        assert_eq!(resent_indices, missing_before, "缺失清单驱动的续传");
        for index in 0..5_u64 {
            let count = puts.iter().filter(|(put, _)| *put == index).count();
            assert!(count <= 2, "索引 {index} 上传了 {count} 次");
        }
        assert_eq!(
            puts.iter().filter(|(index, _)| *index == 2).count(),
            2,
            "断线的块必须重传"
        );
        assert_eq!(target.finalize_calls(), 1);
    }

    #[tokio::test]
    async fn a_part_the_server_stored_as_other_bytes_is_sent_again() {
        let bytes = content(8);
        let source = MemorySource::new("gen-1", bytes.clone());
        let target = MemoryTarget::new();
        target.wrong_ack_once(1);

        let outcome = run(&source, &target, None, &options(4))
            .await
            .expect("重传后成功");
        assert_eq!(outcome.content.digest, digest_of(&bytes));
        let puts = target.puts();
        assert_eq!(
            puts.iter().filter(|(index, _)| *index == 1).count(),
            2,
            "回执不符的块必须重传：{puts:?}"
        );
        assert_eq!(target.finalize_calls(), 1);
    }

    #[tokio::test]
    async fn a_part_that_stays_wrong_never_finalizes() {
        let source = MemorySource::new("gen-1", content(8));
        let target = MemoryTarget::new();
        target.wrong_ack_always(1);

        let failure = run(&source, &target, None, &options(4))
            .await
            .expect_err("持续不符必须失败");
        match failure.kind() {
            UploadErrorKind::PartRejected { index, .. } => assert_eq!(*index, 1),
            other => panic!("期望 PartRejected，实际 {other}"),
        }
        assert!(failure.resume().is_some(), "会话仍在，可续传");
        assert_eq!(target.finalize_calls(), 0, "错块不得进入 finalize");
    }

    #[tokio::test]
    async fn resuming_with_a_changed_snapshot_is_refused_before_any_request() {
        let original = MemorySource::new("gen-1", content(8));
        let resumed_snapshot =
            UploadSnapshot::new(original.identity(), 8, digest_of(&content(8))).expect("快照");
        let resume = UploadResume::new(resumed_snapshot, "00000000000000000000000000000001")
            .expect("续传信息");

        let changed = MemorySource::new("gen-2", content(9));
        let target = MemoryTarget::new();
        let failure = run(&changed, &target, Some(&resume), &options(4))
            .await
            .expect_err("快照已变");
        assert!(
            matches!(failure.kind(), UploadErrorKind::SnapshotChanged { .. }),
            "实际：{failure}"
        );
        assert_eq!(target.status_calls(), 0, "快照变了就不碰旧会话");
        assert!(target.puts().is_empty());
    }

    #[tokio::test]
    async fn a_session_bound_to_another_generation_is_refused() {
        let bytes = content(8);
        let source = MemorySource::new("gen-1", bytes.clone());
        let target = MemoryTarget::new().with_begin_facts(8, [9_u8; DIGEST_BYTES]);

        let failure = run(&source, &target, None, &options(4))
            .await
            .expect_err("会话内容与快照不符");
        assert!(
            matches!(failure.kind(), UploadErrorKind::SessionMismatch { .. }),
            "实际：{failure}"
        );
        assert!(target.puts().is_empty(), "混版本绝不上传块");
        assert_eq!(target.finalize_calls(), 0);
    }

    #[tokio::test]
    async fn the_server_chunk_size_defines_the_blocks() {
        let bytes = content(20);
        let source = MemorySource::new("gen-1", bytes.clone());
        let target = MemoryTarget::new().with_begin_chunk_size(8);

        let outcome = run(&source, &target, None, &options(4))
            .await
            .expect("上传成功");
        assert_eq!(outcome.content.part_count, 3, "服务端把块大小调成 8");
        let mut lengths: Vec<u64> = target
            .puts()
            .into_iter()
            .map(|(_, length)| length)
            .collect();
        lengths.sort_unstable();
        assert_eq!(lengths, vec![4, 8, 8]);
    }

    #[tokio::test]
    async fn uploads_never_exceed_the_configured_part_concurrency() {
        let source = MemorySource::new("gen-1", content(40));
        let target = MemoryTarget::new();
        run(
            &source,
            &target,
            None,
            &UploadOptions::new(4).with_part_concurrency(2),
        )
        .await
        .expect("上传成功");
        assert_eq!(target.peak_in_flight(), 2);
    }

    #[tokio::test]
    async fn cancelling_keeps_the_session_and_the_received_parts() {
        let bytes = content(20);
        let source = MemorySource::new("gen-1", bytes);
        let target = MemoryTarget::new();
        let cancel = CancelToken::new();
        target.cancel_after_first_put(cancel.clone());

        let failure = run(
            &source,
            &target,
            None,
            &UploadOptions::new(4)
                .with_part_concurrency(1)
                .with_cancel(cancel.clone()),
        )
        .await
        .expect_err("取消不是成功");
        assert!(failure.kind().is_cancelled(), "{failure}");
        assert_eq!(target.finalize_calls(), 0);
        let resume = failure.resume().expect("取消后仍可续传").clone();
        assert_eq!(target.puts().len(), 1, "取消后不再启动新块");

        let outcome = run(&source, &target, Some(&resume), &options(4))
            .await
            .expect("续传成功");
        assert!(outcome.reused_parts >= 1, "已收块不重传");
        assert_eq!(outcome.uploaded_parts + outcome.reused_parts, 5);
    }

    #[tokio::test]
    async fn a_finalized_session_replays_without_resending_parts() {
        let bytes = content(10);
        let source = MemorySource::new("gen-1", bytes);
        let target = MemoryTarget::new();
        let first = run(&source, &target, None, &options(4))
            .await
            .expect("首次上传");
        let resume = UploadResume::new(
            UploadSnapshot::new(source.identity(), 10, first.content.digest).expect("快照"),
            first.session.clone(),
        )
        .expect("续传信息");
        let puts_before = target.puts().len();

        let second = run(&source, &target, Some(&resume), &options(4))
            .await
            .expect("重放");
        assert!(second.content.replayed);
        assert_eq!(second.uploaded_parts, 0);
        assert_eq!(target.puts().len(), puts_before, "已 finalize 的会话不重传");
        assert_eq!(target.finalize_calls(), 2);
    }

    #[tokio::test]
    async fn empty_content_uploads_without_any_part() {
        let source = MemorySource::new("gen-empty", Vec::new());
        let target = MemoryTarget::new();
        let outcome = run(&source, &target, None, &options(4))
            .await
            .expect("空内容合法");

        assert_eq!(outcome.content.size, 0);
        assert_eq!(outcome.content.part_count, 0);
        assert_eq!(outcome.content.digest, digest_of(b""));
        assert!(target.puts().is_empty());
        assert_eq!(target.finalize_calls(), 1);
    }

    #[tokio::test]
    async fn a_status_that_contradicts_the_received_parts_is_refused() {
        let bytes = content(8);
        let source = MemorySource::new("gen-1", bytes.clone());
        let snapshot = UploadSnapshot::new("gen-1", 8, digest_of(&bytes)).expect("快照");
        let resume = UploadResume::new(snapshot, "00000000000000000000000000000002").expect("续传");
        let info = UploadProgressInfo {
            session: UploadSessionInfo {
                id: "00000000000000000000000000000002".to_owned(),
                chunk_size: 4,
                part_count: 2,
                total_size: 8,
                digest: digest_of(&bytes),
                state: UploadSessionState::Open,
            },
            received: vec![ReceivedPart {
                index: 0,
                length: 4,
                digest: digest_of(&bytes[..4]),
            }],
            // 自相矛盾：块 0 已收，却又被报成缺失。
            missing: vec![range(0, 1)],
        };
        let target = StatusOnly { info };
        let failure = run(&source, &target, Some(&resume), &options(4))
            .await
            .expect_err("服务端事实自相矛盾");
        assert!(
            matches!(failure.kind(), UploadErrorKind::SessionMismatch { .. }),
            "实际：{failure}"
        );
    }

    #[tokio::test]
    async fn a_received_part_with_the_wrong_length_is_refused() {
        let bytes = content(8);
        let source = MemorySource::new("gen-1", bytes.clone());
        let snapshot = UploadSnapshot::new("gen-1", 8, digest_of(&bytes)).expect("快照");
        let resume = UploadResume::new(snapshot, "00000000000000000000000000000003").expect("续传");
        let target = StatusOnly {
            info: UploadProgressInfo {
                session: UploadSessionInfo {
                    id: "00000000000000000000000000000003".to_owned(),
                    chunk_size: 4,
                    part_count: 2,
                    total_size: 8,
                    digest: digest_of(&bytes),
                    state: UploadSessionState::Open,
                },
                received: vec![ReceivedPart {
                    index: 0,
                    length: 3,
                    digest: digest_of(&bytes[..3]),
                }],
                missing: vec![range(0, 1), range(1, 1)],
            },
        };
        let failure = run(&source, &target, Some(&resume), &options(4))
            .await
            .expect_err("块长度与服务端几何不符");
        assert!(
            matches!(failure.kind(), UploadErrorKind::SessionMismatch { .. }),
            "实际：{failure}"
        );
    }

    #[tokio::test]
    async fn an_aborted_session_is_reported_as_closed() {
        let bytes = content(8);
        let source = MemorySource::new("gen-1", bytes.clone());
        let target = MemoryTarget::new().with_begin_state(UploadSessionState::Aborted);
        let failure = run(&source, &target, None, &options(4))
            .await
            .expect_err("已放弃的会话不能再传");
        assert!(
            matches!(
                failure.kind(),
                UploadErrorKind::SessionClosed {
                    state: UploadSessionState::Aborted
                }
            ),
            "实际：{failure}"
        );
        assert!(target.puts().is_empty());
    }

    #[tokio::test]
    async fn invalid_options_are_rejected_before_any_request() {
        let source = MemorySource::new("gen-1", content(4));
        let target = MemoryTarget::new();
        for options in [
            UploadOptions::new(0),
            UploadOptions::new(4).with_part_concurrency(0),
            UploadOptions::new(4).with_part_attempts(0),
        ] {
            let failure = run(&source, &target, None, &options)
                .await
                .expect_err("非法参数");
            assert!(
                matches!(failure.kind(), UploadErrorKind::InvalidOptions { .. }),
                "实际：{failure}"
            );
        }
        assert_eq!(target.status_calls(), 0);
        assert!(target.puts().is_empty());
    }
}
