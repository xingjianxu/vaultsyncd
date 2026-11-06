//! 传输执行测试用的内存端口：只在 `cfg(test)` 编译，产品构建里不存在。
//!
//! 这些替身实现的是 `crate::ports` 的真实端口，因此被测的是**产品路径**（同样的 `upload::run` /
//! `download::run`），而不是“测试专用分支”。它们可以注入断线（暂时失败）、错块（回执摘要不同）、
//! 服务端把块大小调整成别的值、空间不足、取消与整份摘要不符等情形。

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use crate::bitmap::{PartBitmap, PartRange};
use crate::chunk::ChunkGeometry;
use crate::concurrency::CancelToken;
use crate::hash::{DIGEST_BYTES, digest_of};
use crate::ports::{
    DownloadKey, DownloadProgressStore, FinalizedContent, LocalErrorKind, PartAck, PortError,
    PortFuture, ReceivedPart, SessionSpec, SnapshotSource, StoredProgress, TempContent,
    UploadProgressInfo, UploadSessionInfo, UploadSessionState, UploadTarget, VersionPin,
    VersionedContent,
};

/// 固定内容的内存快照：记录每次读请求的窗口，便于断言“没有整份读入内存”。
pub(crate) struct MemorySource {
    identity: String,
    bytes: Vec<u8>,
    windows: Mutex<Vec<(u64, u64)>>,
}

impl MemorySource {
    pub(crate) fn new(identity: &str, bytes: Vec<u8>) -> Self {
        Self {
            identity: identity.to_owned(),
            bytes,
            windows: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn windows(&self) -> Vec<(u64, u64)> {
        self.windows.lock().expect("窗口记录").clone()
    }
}

impl SnapshotSource for MemorySource {
    fn identity(&self) -> &str {
        &self.identity
    }

    fn total_size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            self.windows
                .lock()
                .expect("窗口记录")
                .push((offset, length));
            slice(&self.bytes, offset, length)
        })
    }
}

fn slice(bytes: &[u8], offset: u64, length: u64) -> Result<Vec<u8>, PortError> {
    let start = usize::try_from(offset)
        .map_err(|_| PortError::local(LocalErrorKind::Read, "偏移超出本机字长"))?;
    let end = usize::try_from(offset + length)
        .map_err(|_| PortError::local(LocalErrorKind::Read, "长度超出本机字长"))?;
    bytes
        .get(start..end)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| PortError::local(LocalErrorKind::Read, "越界读取"))
}

/// 内存上传会话池：服务端事实（已收块、块几何、状态）都在这里。
#[derive(Default)]
struct TargetState {
    sessions: BTreeMap<String, Session>,
    next_session: u64,
    /// 服务端把块大小调整成这个值（`None` = 就用请求值）。
    begin_chunk_size: Option<u64>,
    /// 会话报告的总大小覆盖（模拟“另一个 generation 的会话”）。
    begin_total_size: Option<u64>,
    /// 会话报告的整份摘要覆盖。
    begin_digest: Option<[u8; DIGEST_BYTES]>,
    /// 新会话的初始状态。
    begin_state: Option<UploadSessionState>,
    /// 该索引的上传第一次返回“暂时不可用”（断线）。
    fail_put_once_at: Option<u64>,
    /// 该索引的回执第一次给错摘要（服务端收下了别的字节）。
    wrong_ack_once_at: Option<u64>,
    /// 该索引的回执永远给错摘要。
    wrong_ack_always_at: Option<u64>,
    puts: Vec<(u64, u64)>,
    status_calls: usize,
    finalize_calls: usize,
    in_flight: usize,
    peak_in_flight: usize,
    /// 第一块传完后取消（模拟上传中途取消）。
    cancel_after_first_put: Option<CancelToken>,
}

struct Session {
    spec: SessionSpec,
    chunk_size: u64,
    parts: BTreeMap<u64, Vec<u8>>,
    state: UploadSessionState,
}

/// 内存服务端：实现 [`UploadTarget`]。
#[derive(Default)]
pub(crate) struct MemoryTarget {
    inner: Mutex<TargetState>,
}

impl MemoryTarget {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_begin_chunk_size(self, chunk_size: u64) -> Self {
        self.inner.lock().expect("目标状态").begin_chunk_size = Some(chunk_size);
        self
    }

    pub(crate) fn with_begin_facts(self, total_size: u64, digest: [u8; DIGEST_BYTES]) -> Self {
        {
            let mut state = self.inner.lock().expect("目标状态");
            state.begin_total_size = Some(total_size);
            state.begin_digest = Some(digest);
        }
        self
    }

    pub(crate) fn with_begin_state(self, state: UploadSessionState) -> Self {
        self.inner.lock().expect("目标状态").begin_state = Some(state);
        self
    }

    pub(crate) fn fail_put_once(&self, index: u64) {
        self.inner.lock().expect("目标状态").fail_put_once_at = Some(index);
    }

    pub(crate) fn wrong_ack_once(&self, index: u64) {
        self.inner.lock().expect("目标状态").wrong_ack_once_at = Some(index);
    }

    pub(crate) fn wrong_ack_always(&self, index: u64) {
        self.inner.lock().expect("目标状态").wrong_ack_always_at = Some(index);
    }

    pub(crate) fn puts(&self) -> Vec<(u64, u64)> {
        self.inner.lock().expect("目标状态").puts.clone()
    }

    pub(crate) fn status_calls(&self) -> usize {
        self.inner.lock().expect("目标状态").status_calls
    }

    pub(crate) fn finalize_calls(&self) -> usize {
        self.inner.lock().expect("目标状态").finalize_calls
    }

    pub(crate) fn peak_in_flight(&self) -> usize {
        self.inner.lock().expect("目标状态").peak_in_flight
    }

    pub(crate) fn cancel_after_first_put(&self, token: CancelToken) {
        self.inner.lock().expect("目标状态").cancel_after_first_put = Some(token);
    }
}

impl UploadTarget for MemoryTarget {
    fn begin<'a>(
        &'a self,
        spec: &'a SessionSpec,
    ) -> PortFuture<'a, Result<UploadSessionInfo, PortError>> {
        Box::pin(async move {
            let mut state = self.inner.lock().expect("目标状态");
            state.next_session += 1;
            let id = format!("{:032x}", state.next_session);
            let chunk_size = state.begin_chunk_size.unwrap_or(spec.chunk_size);
            let total_size = state.begin_total_size.unwrap_or(spec.total_size);
            let digest = state.begin_digest.unwrap_or(spec.digest);
            let session_state = state.begin_state.unwrap_or(UploadSessionState::Open);
            state.sessions.insert(
                id.clone(),
                Session {
                    spec: SessionSpec {
                        total_size,
                        digest,
                        chunk_size,
                    },
                    chunk_size,
                    parts: BTreeMap::new(),
                    state: session_state,
                },
            );
            let info = session_info(&id, chunk_size, total_size, digest, session_state)?;
            Ok(info)
        })
    }

    fn status<'a>(
        &'a self,
        session: &'a str,
    ) -> PortFuture<'a, Result<UploadProgressInfo, PortError>> {
        Box::pin(async move {
            let mut state = self.inner.lock().expect("目标状态");
            state.status_calls += 1;
            let record = state
                .sessions
                .get(session)
                .ok_or_else(|| PortError::rejected("UNSUPPORTED_ENTRY", "会话不存在"))?;
            let info = session_info(
                session,
                record.chunk_size,
                record.spec.total_size,
                record.spec.digest,
                record.state,
            )?;
            let geometry = ChunkGeometry::new(record.spec.total_size, record.chunk_size)
                .map_err(|error| PortError::protocol(error.to_string()))?;
            let mut bitmap = PartBitmap::empty(geometry);
            let mut received = Vec::new();
            for (index, bytes) in &record.parts {
                bitmap
                    .mark(*index)
                    .map_err(|error| PortError::protocol(error.to_string()))?;
                received.push(ReceivedPart {
                    index: *index,
                    length: bytes.len() as u64,
                    digest: digest_of(bytes),
                });
            }
            Ok(UploadProgressInfo {
                session: info,
                received,
                missing: bitmap.missing(),
            })
        })
    }

    fn put_part<'a>(
        &'a self,
        session: &'a str,
        index: u64,
        digest: &'a [u8; DIGEST_BYTES],
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<PartAck, PortError>> {
        Box::pin(async move {
            {
                let mut state = self.inner.lock().expect("目标状态");
                state.in_flight += 1;
                state.peak_in_flight = state.peak_in_flight.max(state.in_flight);
                state.puts.push((index, bytes.len() as u64));
            }
            // 让出一次：让驱动有机会启动别的块，从而观察并发上限。
            tokio::task::yield_now().await;
            let mut state = self.inner.lock().expect("目标状态");
            state.in_flight -= 1;

            if state.fail_put_once_at == Some(index) {
                state.fail_put_once_at = None;
                return Err(PortError::unavailable("连接被重置", None));
            }
            if !state
                .sessions
                .get(session)
                .is_some_and(|record| record.state == UploadSessionState::Open)
            {
                return Err(PortError::rejected("PART_CONFLICT", "会话不接收块"));
            }
            let geometry = {
                let record = state.sessions.get(session).expect("会话刚检查过");
                ChunkGeometry::new(record.spec.total_size, record.chunk_size)
                    .map_err(|error| PortError::protocol(error.to_string()))?
            };
            let expected_length = geometry
                .part_length(index)
                .ok_or_else(|| PortError::rejected("INVALID_REQUEST", "块索引越界"))?;
            if bytes.len() as u64 != expected_length {
                return Err(PortError::rejected("INVALID_REQUEST", "块长度与几何不符"));
            }
            if digest_of(bytes) != *digest {
                return Err(PortError::rejected("HASH_MISMATCH", "块摘要与请求头不符"));
            }
            let wrong_always = state.wrong_ack_always_at == Some(index);
            let wrong_once = state.wrong_ack_once_at == Some(index);
            if wrong_once {
                state.wrong_ack_once_at = None;
            }
            let cancel = std::mem::take(&mut state.cancel_after_first_put);
            let record = state
                .sessions
                .get_mut(session)
                .ok_or_else(|| PortError::rejected("UNSUPPORTED_ENTRY", "会话不存在"))?;

            // 服务端在这一块上永远是“错了的字节”：回执摘要永远与请求不符（重传也一样）。
            if wrong_always {
                record.parts.insert(index, wrong_bytes(bytes));
                return Ok(PartAck {
                    index,
                    length: expected_length,
                    digest: [0xff; DIGEST_BYTES],
                    already_present: false,
                });
            }
            if wrong_once {
                record.parts.insert(index, bytes.to_vec());
                return Ok(PartAck {
                    index,
                    length: expected_length,
                    digest: [0xff; DIGEST_BYTES],
                    already_present: false,
                });
            }
            let already_present = match record.parts.get(&index) {
                Some(existing) if existing == bytes => true,
                Some(_) => {
                    return Err(PortError::rejected(
                        "PART_CONFLICT",
                        "同一索引已收下不同字节",
                    ));
                }
                None => {
                    record.parts.insert(index, bytes.to_vec());
                    false
                }
            };
            if let Some(token) = cancel {
                token.cancel();
            }
            Ok(PartAck {
                index,
                length: expected_length,
                digest: *digest,
                already_present,
            })
        })
    }

    fn finalize<'a>(
        &'a self,
        session: &'a str,
    ) -> PortFuture<'a, Result<FinalizedContent, PortError>> {
        Box::pin(async move {
            let mut state = self.inner.lock().expect("目标状态");
            state.finalize_calls += 1;
            let record = state
                .sessions
                .get_mut(session)
                .ok_or_else(|| PortError::rejected("UNSUPPORTED_ENTRY", "会话不存在"))?;
            let geometry = ChunkGeometry::new(record.spec.total_size, record.chunk_size)
                .map_err(|error| PortError::protocol(error.to_string()))?;
            let replayed = record.state == UploadSessionState::Finalized;
            if record.parts.len() as u64 != geometry.part_count() {
                return Err(PortError::rejected("INVALID_REQUEST", "块未收齐"));
            }
            let mut content =
                Vec::with_capacity(usize::try_from(record.spec.total_size).unwrap_or(0));
            for index in 0..geometry.part_count() {
                let bytes = record
                    .parts
                    .get(&index)
                    .ok_or_else(|| PortError::rejected("INVALID_REQUEST", "块未收齐"))?;
                content.extend_from_slice(bytes);
            }
            if digest_of(&content) != record.spec.digest {
                return Err(PortError::rejected(
                    "HASH_MISMATCH",
                    "拼装内容与声明摘要不符",
                ));
            }
            record.state = UploadSessionState::Finalized;
            Ok(FinalizedContent {
                digest: record.spec.digest,
                size: record.spec.total_size,
                part_count: geometry.part_count(),
                replayed,
                storage_path: format!("objects/{session}"),
            })
        })
    }
}

fn session_info(
    id: &str,
    chunk_size: u64,
    total_size: u64,
    digest: [u8; DIGEST_BYTES],
    state: UploadSessionState,
) -> Result<UploadSessionInfo, PortError> {
    let geometry = ChunkGeometry::new(total_size, chunk_size)
        .map_err(|error| PortError::protocol(error.to_string()))?;
    Ok(UploadSessionInfo {
        id: id.to_owned(),
        chunk_size,
        part_count: geometry.part_count(),
        total_size,
        digest,
        state,
    })
}

#[derive(Default)]
struct TempState {
    bytes: Vec<u8>,
    capacity_ok: bool,
    writes: Vec<(u64, usize)>,
    reads: Vec<(u64, u64)>,
    discards: usize,
    /// 写完一块后取消（模拟用户在下载中途取消）。
    cancel_after_write: Option<CancelToken>,
    /// 写到这个偏移时报空间不足。
    fail_write_at: Option<u64>,
}

/// 内存临时文件：实现 [`TempContent`]。
pub(crate) struct MemoryTemp {
    path: String,
    inner: Mutex<TempState>,
}

impl MemoryTemp {
    pub(crate) fn new(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            inner: Mutex::new(TempState {
                capacity_ok: true,
                ..TempState::default()
            }),
        }
    }

    pub(crate) fn without_capacity(self) -> Self {
        self.inner.lock().expect("临时状态").capacity_ok = false;
        self
    }

    pub(crate) fn cancel_after_write(&self, token: CancelToken) {
        self.inner.lock().expect("临时状态").cancel_after_write = Some(token);
    }

    pub(crate) fn seed_bytes(&self, bytes: Vec<u8>) {
        self.inner.lock().expect("临时状态").bytes = bytes;
    }

    pub(crate) fn contents(&self) -> Vec<u8> {
        self.inner.lock().expect("临时状态").bytes.clone()
    }

    pub(crate) fn writes(&self) -> Vec<(u64, usize)> {
        self.inner.lock().expect("临时状态").writes.clone()
    }

    pub(crate) fn read_windows(&self) -> Vec<(u64, u64)> {
        self.inner.lock().expect("临时状态").reads.clone()
    }

    pub(crate) fn discards(&self) -> usize {
        self.inner.lock().expect("临时状态").discards
    }
}

impl TempContent for MemoryTemp {
    fn path(&self) -> &str {
        &self.path
    }

    fn ensure_capacity(&self, _needed: u64) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let state = self.inner.lock().expect("临时状态");
            if state.capacity_ok {
                Ok(())
            } else {
                Err(PortError::local(LocalErrorKind::NoSpace, "ENOSPC"))
            }
        })
    }

    fn write_at<'a>(
        &'a self,
        offset: u64,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let cancel = {
                let mut state = self.inner.lock().expect("临时状态");
                if state.fail_write_at == Some(offset) {
                    return Err(PortError::local(LocalErrorKind::NoSpace, "ENOSPC"));
                }
                let start = usize::try_from(offset)
                    .map_err(|_| PortError::local(LocalErrorKind::Write, "偏移超出本机字长"))?;
                let end = start + bytes.len();
                if state.bytes.len() < end {
                    state.bytes.resize(end, 0);
                }
                state.bytes[start..end].copy_from_slice(bytes);
                state.writes.push((offset, bytes.len()));
                state.cancel_after_write.take()
            };
            if let Some(token) = cancel {
                token.cancel();
            }
            Ok(())
        })
    }

    fn read_at(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let mut state = self.inner.lock().expect("临时状态");
            state.reads.push((offset, length));
            slice(&state.bytes, offset, length)
        })
    }

    fn sync(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { Ok(()) })
    }

    fn discard(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let mut state = self.inner.lock().expect("临时状态");
            state.bytes.clear();
            state.discards += 1;
            Ok(())
        })
    }
}

/// 内存下载进度表：实现 [`DownloadProgressStore`]。
#[derive(Default)]
pub(crate) struct MemoryProgress {
    inner: Mutex<HashMap<DownloadKey, StoredProgress>>,
    discards: Mutex<usize>,
}

impl MemoryProgress {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn seed(&self, key: DownloadKey, progress: StoredProgress) {
        self.inner.lock().expect("进度表").insert(key, progress);
    }

    pub(crate) fn get(&self, key: &DownloadKey) -> Option<StoredProgress> {
        self.inner.lock().expect("进度表").get(key).cloned()
    }

    pub(crate) fn discard_count(&self) -> usize {
        *self.discards.lock().expect("丢弃计数")
    }
}

impl DownloadProgressStore for MemoryProgress {
    fn load<'a>(
        &'a self,
        key: &'a DownloadKey,
    ) -> PortFuture<'a, Result<Option<StoredProgress>, PortError>> {
        Box::pin(async move { Ok(self.inner.lock().expect("进度表").get(key).cloned()) })
    }

    fn save<'a>(
        &'a self,
        key: &'a DownloadKey,
        progress: &'a StoredProgress,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            self.inner
                .lock()
                .expect("进度表")
                .insert(key.clone(), progress.clone());
            Ok(())
        })
    }

    fn discard<'a>(&'a self, key: &'a DownloadKey) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            self.inner.lock().expect("进度表").remove(key);
            *self.discards.lock().expect("丢弃计数") += 1;
            Ok(())
        })
    }
}

#[derive(Default)]
struct ContentState {
    reads: Vec<(u64, u64)>,
    pins: Vec<(String, String)>,
    in_flight: usize,
    peak_in_flight: usize,
    fail_once_at: Option<u64>,
    corrupt_once_at: Option<u64>,
    corrupt_always_at: Option<u64>,
}

/// 固定版本内容的内存服务端：实现 [`VersionedContent`]。
pub(crate) struct MemoryContent {
    bytes: Vec<u8>,
    inner: Mutex<ContentState>,
}

impl MemoryContent {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            inner: Mutex::new(ContentState::default()),
        }
    }

    pub(crate) fn fail_once_at(&self, offset: u64) {
        self.inner.lock().expect("内容状态").fail_once_at = Some(offset);
    }

    pub(crate) fn corrupt_once_at(&self, offset: u64) {
        self.inner.lock().expect("内容状态").corrupt_once_at = Some(offset);
    }

    pub(crate) fn corrupt_always_at(&self, offset: u64) {
        self.inner.lock().expect("内容状态").corrupt_always_at = Some(offset);
    }

    pub(crate) fn reads(&self) -> Vec<(u64, u64)> {
        self.inner.lock().expect("内容状态").reads.clone()
    }

    pub(crate) fn requested_versions(&self) -> Vec<(String, String)> {
        self.inner.lock().expect("内容状态").pins.clone()
    }

    pub(crate) fn read_offsets(&self) -> Vec<u64> {
        self.reads().into_iter().map(|(offset, _)| offset).collect()
    }

    pub(crate) fn peak_in_flight(&self) -> usize {
        self.inner.lock().expect("内容状态").peak_in_flight
    }
}

impl VersionedContent for MemoryContent {
    fn read_range<'a>(
        &'a self,
        pin: &'a VersionPin,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            {
                let mut state = self.inner.lock().expect("内容状态");
                state.in_flight += 1;
                state.peak_in_flight = state.peak_in_flight.max(state.in_flight);
                state.reads.push((offset, length));
                state.pins.push((pin.entry.clone(), pin.version.clone()));
            }
            tokio::task::yield_now().await;
            let mut state = self.inner.lock().expect("内容状态");
            state.in_flight -= 1;
            if state.fail_once_at == Some(offset) {
                state.fail_once_at = None;
                return Err(PortError::unavailable("连接被重置", None));
            }
            let mut bytes = slice(&self.bytes, offset, length)?;
            if state.corrupt_always_at == Some(offset) {
                flip(&mut bytes);
            } else if state.corrupt_once_at == Some(offset) {
                state.corrupt_once_at = None;
                flip(&mut bytes);
            }
            Ok(bytes)
        })
    }
}

/// 与收到的字节不同但长度相同的“坏块”。
fn wrong_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut copy = bytes.to_vec();
    flip(&mut copy);
    copy
}

fn flip(bytes: &mut [u8]) {
    if let Some(first) = bytes.first_mut() {
        *first ^= 0xff;
    }
}

/// 只回答“会话状态”的假端口：用来验证“缺失区间与已收块矛盾”会被拒绝。
pub(crate) struct StatusOnly {
    pub(crate) info: UploadProgressInfo,
}

impl UploadTarget for StatusOnly {
    fn begin<'a>(
        &'a self,
        _spec: &'a SessionSpec,
    ) -> PortFuture<'a, Result<UploadSessionInfo, PortError>> {
        Box::pin(async move { Err(PortError::protocol("不该申请新会话")) })
    }

    fn status<'a>(
        &'a self,
        _session: &'a str,
    ) -> PortFuture<'a, Result<UploadProgressInfo, PortError>> {
        Box::pin(async move { Ok(self.info.clone()) })
    }

    fn put_part<'a>(
        &'a self,
        _session: &'a str,
        _index: u64,
        _digest: &'a [u8; DIGEST_BYTES],
        _bytes: &'a [u8],
    ) -> PortFuture<'a, Result<PartAck, PortError>> {
        Box::pin(async move { Err(PortError::protocol("不该上传块")) })
    }

    fn finalize<'a>(
        &'a self,
        _session: &'a str,
    ) -> PortFuture<'a, Result<FinalizedContent, PortError>> {
        Box::pin(async move { Err(PortError::protocol("不该 finalize")) })
    }
}

/// 缺失区间的便捷构造（测试里拼装服务端事实用）。
pub(crate) fn range(first_index: u64, count: u64) -> PartRange {
    PartRange::new(first_index, count).expect("非空区间")
}
