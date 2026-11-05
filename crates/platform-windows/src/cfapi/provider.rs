//! CFAPI 内容供给回调、取消与读取验证（S011）。
//!
//! 与 S010 的区别：`FETCH_DATA` 不再回执 `STATUS_NOT_SUPPORTED`，而是按回调请求的
//! `Offset`/`Length` 从固定测试内容源（[`super::content`]）分段回填，每段一次
//! `CfExecute(CF_OPERATION_TYPE_TRANSFER_DATA)`。语义约定：
//!
//! - **只回填请求范围**：越界/非法范围（负偏移、非正长度、越过末尾、非原型条目）以
//!   `STATUS_CLOUD_FILE_INVALID_REQUEST` 明确失败，绝不返回 0 字节“成功”（design I05、T06）。
//! - **源不可用**：内容源被标记为不可用时，所有水合请求以
//!   `STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE` 失败；已水合的数据仍在本地，可正常读取
//!   （design §14.2、§20 的“离线打开未下载文件 1 秒内明确失败”）。
//! - **超时**：内容源可配置延迟；延迟达到/超过阈值时按
//!   [`ProviderConfig::timeout_ms`] 截断等待并以 `STATUS_IO_TIMEOUT` 失败，不留悬挂请求。
//! - **取消**：`CANCEL_FETCH_DATA` 只把被取消的 `TransferKey` 记进原子标志；正在回填的循环
//!   在分段之间检查该标志并立即返回，不再调用 `CfExecute`。请求因此尽快结束，句柄可关闭。
//! - **重复请求幂等**：内容由位置确定性生成，同一范围被系统重复请求时返回逐字节相同的
//!   数据，不会混入另一个版本。
//!
//! 生命周期：回调上下文是进程级 [`OnceLock<Arc<ProviderContext>>`]，连同回调表一起是
//! `'static`；回调通过 `PROVIDER.get()` 取到 `Arc` 而不是解引用裸指针，因此
//! **不存在指向已 drop 数据的指针**。连接关闭时先 `CfDisconnectSyncRoot`，再
//! [`ProviderContext::wait_idle`] 确认没有回调仍在执行，然后才结束进程；原型每个进程只建
//! 一个连接，真实适配器需要按连接键维护同样纪律的上下文。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::content;
use super::lifecycle::{HResult, StageError, record};
use super::prototype::{hresult_from_win32, read_wide, wide};
use super::prototype_layout::{PROBE_DIRECTORY, PROBE_FILES};
use super::raw::*;

/// 默认超时阈值：与 design §20 “离线打开未下载文件 1 秒内明确失败”对齐。
pub const DEFAULT_TIMEOUT_MS: u64 = 1_000;
/// 等待回调静默的默认上限（`CfDisconnectSyncRoot` 之后）。
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// 内容源可用性（模拟“断开内容源”）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceAvailability {
    /// 在线：按请求范围回填固定内容。
    Available,
    /// 不可用：所有水合请求以 `STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE` 失败。
    Unavailable,
}

impl SourceAvailability {
    /// 日志/命令行里的名字。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }
}

/// 内容源行为参数：延迟模拟慢源，超时阈值决定何时以明确失败结束，`chunk_delay_ms` 拉长
/// 分段之间的窗口以便观察取消。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderConfig {
    /// 内容源是否可用。
    pub source: SourceAvailability,
    /// 每次 `FETCH_DATA` 开始前的模拟延迟（毫秒）。
    pub latency_ms: u64,
    /// 超时阈值（毫秒）：延迟达到或超过它就按超时失败。
    pub timeout_ms: u64,
    /// 分段回填之间的模拟延迟（毫秒），用于让取消在传输过程中发生。
    pub chunk_delay_ms: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            source: SourceAvailability::Available,
            latency_ms: 0,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            chunk_delay_ms: 0,
        }
    }
}

/// 提供方本进程的累计计数（回调线程与命令线程都可能更新，全部原子）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderStats {
    /// `FETCH_DATA` 回调次数（含系统重试）。
    pub fetch_requests: u64,
    /// 成功 `CfExecute(TRANSFER_DATA)` 的次数。
    pub chunks_sent: u64,
    /// 实际交给系统的内容字节数。
    pub bytes_served: u64,
    /// 因范围/条目非法而被拒绝的请求数。
    pub range_rejections: u64,
    /// `CANCEL_FETCH_DATA` 回调次数。
    pub cancel_callbacks: u64,
    /// 因取消而在分段中途退出的请求数。
    pub aborted_fetches: u64,
    /// 以超时结束的请求数。
    pub timeout_fetches: u64,
    /// 因内容源不可用而失败的请求数。
    pub unavailable_fetches: u64,
    /// `CfExecute` 自身失败次数（非内容错误）。
    pub failed_acks: u64,
    /// 同时在执行的回调峰值。
    pub peak_in_flight: u64,
    /// 当前仍在执行的回调数。
    pub in_flight: u64,
}

impl ProviderStats {
    /// 展开成 `record` 的字段列表，便于日志逐项核对。
    pub fn fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("fetch_requests", self.fetch_requests.to_string()),
            ("chunks_sent", self.chunks_sent.to_string()),
            ("bytes_served", self.bytes_served.to_string()),
            ("range_rejections", self.range_rejections.to_string()),
            ("cancel_callbacks", self.cancel_callbacks.to_string()),
            ("aborted_fetches", self.aborted_fetches.to_string()),
            ("timeout_fetches", self.timeout_fetches.to_string()),
            ("unavailable_fetches", self.unavailable_fetches.to_string()),
            ("failed_acks", self.failed_acks.to_string()),
            ("peak_in_flight", self.peak_in_flight.to_string()),
            ("in_flight", self.in_flight.to_string()),
        ]
    }
}

/// 提供方运行期状态：配置、计数、取消标志。所有回调共享同一个 `Arc`。
#[derive(Debug)]
pub struct ProviderContext {
    /// 上下文安装时刻：所有日志与取消时序都以它为基准（`t_ms`）。
    started_at: Instant,
    config: ProviderConfig,
    fetch_requests: AtomicU64,
    chunks_sent: AtomicU64,
    bytes_served: AtomicU64,
    range_rejections: AtomicU64,
    cancel_callbacks: AtomicU64,
    aborted_fetches: AtomicU64,
    timeout_fetches: AtomicU64,
    unavailable_fetches: AtomicU64,
    failed_acks: AtomicU64,
    in_flight: AtomicU64,
    peak_in_flight: AtomicU64,
    /// 已被取消的 `CF_TRANSFER_KEY`；0 表示没有取消。按 key 匹配，避免把对旧请求的取消
    /// 误用到新请求上。
    aborted_transfer_key: AtomicI64,
    /// 是否已经有 `FETCH_DATA` 回调进入（读取方等待它判断“水合真的开始了”）。
    fetch_started: AtomicBool,
    /// 最近一次 `FETCH_DATA` 请求的范围（平台要求的 Offset/Length）。
    last_fetch: std::sync::Mutex<Option<(i64, i64)>>,
}

impl ProviderContext {
    fn new(config: ProviderConfig) -> Self {
        Self {
            started_at: Instant::now(),
            config,
            fetch_requests: AtomicU64::new(0),
            chunks_sent: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
            range_rejections: AtomicU64::new(0),
            cancel_callbacks: AtomicU64::new(0),
            aborted_fetches: AtomicU64::new(0),
            timeout_fetches: AtomicU64::new(0),
            unavailable_fetches: AtomicU64::new(0),
            failed_acks: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            peak_in_flight: AtomicU64::new(0),
            aborted_transfer_key: AtomicI64::new(0),
            fetch_started: AtomicBool::new(false),
            last_fetch: std::sync::Mutex::new(None),
        }
    }

    /// 自上下文安装以来的毫秒数。
    pub fn elapsed_ms(&self) -> u128 {
        self.started_at.elapsed().as_millis()
    }

    /// 本次进程的配置。
    pub fn config(&self) -> ProviderConfig {
        self.config
    }

    /// 计数快照。
    pub fn stats(&self) -> ProviderStats {
        ProviderStats {
            fetch_requests: self.fetch_requests.load(Ordering::SeqCst),
            chunks_sent: self.chunks_sent.load(Ordering::SeqCst),
            bytes_served: self.bytes_served.load(Ordering::SeqCst),
            range_rejections: self.range_rejections.load(Ordering::SeqCst),
            cancel_callbacks: self.cancel_callbacks.load(Ordering::SeqCst),
            aborted_fetches: self.aborted_fetches.load(Ordering::SeqCst),
            timeout_fetches: self.timeout_fetches.load(Ordering::SeqCst),
            unavailable_fetches: self.unavailable_fetches.load(Ordering::SeqCst),
            failed_acks: self.failed_acks.load(Ordering::SeqCst),
            peak_in_flight: self.peak_in_flight.load(Ordering::SeqCst),
            in_flight: self.in_flight.load(Ordering::SeqCst),
        }
    }

    /// 是否已经有水合请求进入回调。
    pub fn fetch_started(&self) -> bool {
        self.fetch_started.load(Ordering::SeqCst)
    }

    /// 最近一次水合请求的 `(Offset, Length)`：证明平台要求的是哪个范围。
    pub fn last_fetch_range(&self) -> Option<(i64, i64)> {
        *self
            .last_fetch
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn mark_abort(&self, transfer_key: i64) {
        self.aborted_transfer_key
            .store(transfer_key, Ordering::SeqCst);
    }

    fn is_aborted(&self, transfer_key: i64) -> bool {
        self.aborted_transfer_key.load(Ordering::SeqCst) == transfer_key
    }

    /// 等待正在执行的回调全部返回；超时即失败，绝不带着在途回调结束进程。
    pub fn wait_idle(&self, timeout: Duration) -> Result<(), StageError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(StageError::new(
                    "provider.drain",
                    HResult::new(0x8007_05B4u32 as i32), // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
                    format!(
                        "等待回调静默超时：in_flight={}",
                        self.in_flight.load(Ordering::SeqCst)
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn enter(&self) -> CallbackGuard<'_> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let mut peak = self.peak_in_flight.load(Ordering::SeqCst);
        while now > peak {
            match self.peak_in_flight.compare_exchange(
                peak,
                now,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(observed) => peak = observed,
            }
        }
        CallbackGuard { context: self }
    }
}

/// 回调进入/退出的守卫：即使回调 panic 也会把 `in_flight` 减回去。
struct CallbackGuard<'a> {
    context: &'a ProviderContext,
}

impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        self.context.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 进程级提供方上下文：回调只通过它访问状态，**不持有任何裸指针**，因此不存在
/// “回调访问已释放内存”的路径（见模块文档）。
static PROVIDER: OnceLock<Arc<ProviderContext>> = OnceLock::new();

/// 安装本次进程的提供方上下文；同一进程重复安装不同配置会明确失败，避免静默用错配置。
pub fn install_provider(config: ProviderConfig) -> Result<Arc<ProviderContext>, StageError> {
    if let Some(existing) = PROVIDER.get() {
        if existing.config() != config {
            return Err(StageError::new(
                "provider.install",
                HResult::new(0x8007_0057u32 as i32), // HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER)
                format!(
                    "本进程已用另一份内容源配置初始化（existing={:?} requested={config:?}）",
                    existing.config()
                ),
            ));
        }
        return Ok(Arc::clone(existing));
    }
    let context = Arc::new(ProviderContext::new(config));
    Ok(Arc::clone(PROVIDER.get_or_init(|| context)))
}

/// 当前进程的提供方上下文（未安装时为 `None`）。
pub fn provider_context() -> Option<Arc<ProviderContext>> {
    PROVIDER.get().map(Arc::clone)
}

// ---------------------------------------------------------------------------
// 回调
// ---------------------------------------------------------------------------

/// 以明确状态结束一次 `FETCH_DATA`：成功段带缓冲，失败段只带状态。
fn execute_transfer(
    info: &CfCallbackInfo,
    completion_status: i32,
    buffer: *const std::ffi::c_void,
    offset: i64,
    length: i64,
) -> HResult {
    let op_info = CfOperationInfo {
        struct_size: u32::try_from(std::mem::size_of::<CfOperationInfo>()).unwrap_or(0),
        operation_type: CF_OPERATION_TYPE_TRANSFER_DATA,
        connection_key: info.connection_key,
        transfer_key: info.transfer_key,
        // 回执必须带回回调的 CorrelationVector 与 RequestKey（S010 实测，否则
        // CfExecute 返回 ERROR_CLOUD_FILE_INVALID_REQUEST）。
        correlation_vector: info.correlation_vector,
        sync_status: std::ptr::null(),
        request_key: info.request_key,
    };
    let mut params = CfOperationParametersTransferData {
        param_size: CF_OPERATION_PARAMETERS_SIZE,
        padding: 0,
        flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        completion_status,
        buffer,
        offset,
        length,
    };
    HResult::new(unsafe { CfExecute(&op_info, std::ptr::from_mut(&mut params).cast()) })
}

/// `FETCH_DATA`：按请求范围回填固定内容，分段 `CfExecute(TRANSFER_DATA)`。
///
/// # Safety
///
/// 由 CFAPI 以有效指针调用；本函数不接管任何指针的所有权。
unsafe extern "system" fn on_fetch_data(
    info: *const CfCallbackInfo,
    params: *const CfCallbackParametersFetchData,
) {
    if info.is_null() {
        eprintln!("callback=fetch_data error=null_callback_info");
        return;
    }
    let Some(context) = PROVIDER.get() else {
        eprintln!("callback=fetch_data error=provider_context_missing");
        return;
    };
    let _guard = context.enter();
    let info = unsafe { &*info };
    let request = context.fetch_requests.fetch_add(1, Ordering::SeqCst) + 1;
    context.fetch_started.store(true, Ordering::SeqCst);
    let path = unsafe { read_wide(info.normalized_path) };
    let (offset, length) = if params.is_null() {
        (0i64, -1i64)
    } else {
        unsafe { ((*params).required_file_offset, (*params).required_length) }
    };
    if let Ok(mut slot) = context.last_fetch.lock() {
        *slot = Some((offset, length));
    }
    let config = context.config();
    let prefix = format!(
        "callback=fetch_data request={request} t_ms={} transfer_key={} path={path} file_size={} required_offset={offset} required_length={length}",
        context.elapsed_ms(),
        info.transfer_key,
        info.file_size
    );

    if params.is_null() {
        fail(
            context,
            info,
            STATUS_CLOUD_FILE_INVALID_REQUEST,
            offset,
            length,
            &format!("{prefix} error=missing_callback_parameters"),
        );
        return;
    }

    let Some((name, size)) = content::entry_for_path(&path) else {
        context.range_rejections.fetch_add(1, Ordering::SeqCst);
        fail(
            context,
            info,
            STATUS_CLOUD_FILE_INVALID_REQUEST,
            offset,
            length,
            &format!("{prefix} error=unknown_entry"),
        );
        return;
    };
    if info.file_size != i64::try_from(size).unwrap_or(i64::MAX) {
        context.range_rejections.fetch_add(1, Ordering::SeqCst);
        fail(
            context,
            info,
            STATUS_CLOUD_FILE_INVALID_REQUEST,
            offset,
            length,
            &format!("{prefix} entry={name} error=size_mismatch expected_size={size}"),
        );
        return;
    }
    let (offset_u, length_u) = match content::check_range(offset, length, size) {
        Ok(range) => range,
        Err(error) => {
            context.range_rejections.fetch_add(1, Ordering::SeqCst);
            fail(
                context,
                info,
                STATUS_CLOUD_FILE_INVALID_REQUEST,
                offset,
                length,
                &format!("{prefix} entry={name} error=invalid_range detail=\"{error}\""),
            );
            return;
        }
    };

    // 模拟延迟并按阈值判定超时：等待时间一定被截断到阈值，不会留下悬挂请求。
    if config.latency_ms > 0 {
        let bound = config.latency_ms.min(config.timeout_ms);
        std::thread::sleep(Duration::from_millis(bound));
        if config.latency_ms >= config.timeout_ms {
            context.timeout_fetches.fetch_add(1, Ordering::SeqCst);
            fail(
                context,
                info,
                STATUS_IO_TIMEOUT,
                offset,
                length,
                &format!(
                    "{prefix} entry={name} error=source_timeout latency_ms={} timeout_ms={}",
                    config.latency_ms, config.timeout_ms
                ),
            );
            return;
        }
    }

    if config.source == SourceAvailability::Unavailable {
        context.unavailable_fetches.fetch_add(1, Ordering::SeqCst);
        fail(
            context,
            info,
            STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
            offset,
            length,
            &format!("{prefix} entry={name} error=source_unavailable"),
        );
        return;
    }

    let mut sink = TransferSink {
        context,
        info,
        transfer_key: info.transfer_key,
        chunk_delay_ms: config.chunk_delay_ms,
    };
    match content::serve_range(name, offset_u, length_u, &mut sink) {
        Ok(report) => {
            if let Some(failure) = report.failure {
                context.failed_acks.fetch_add(1, Ordering::SeqCst);
                eprintln!(
                    "{prefix} entry={name} error=cf_execute_failed offset={} hresult=0x{:08X}",
                    failure.offset, failure.code as u32
                );
            } else if report.aborted {
                context.aborted_fetches.fetch_add(1, Ordering::SeqCst);
                eprintln!(
                    "{prefix} entry={name} outcome=aborted_after_chunks chunks_sent={}",
                    report.chunks_sent
                );
            } else {
                eprintln!(
                    "{prefix} entry={name} outcome=served chunks={} bytes={} replayed={} t_ms={}",
                    report.chunks_sent,
                    report.bytes_sent,
                    request > 1,
                    context.elapsed_ms()
                );
            }
        }
        Err(error) => {
            context.failed_acks.fetch_add(1, Ordering::SeqCst);
            fail(
                context,
                info,
                STATUS_CLOUD_FILE_INVALID_REQUEST,
                offset,
                length,
                &format!("{prefix} entry={name} error=content_source detail=\"{error}\""),
            );
        }
    }
}

/// 把 `serve_range` 的一段内容交给平台：一次 `CfExecute(TRANSFER_DATA)` 回执。
struct TransferSink<'a> {
    context: &'a ProviderContext,
    info: &'a CfCallbackInfo,
    transfer_key: i64,
    chunk_delay_ms: u64,
}

impl content::ChunkSink for TransferSink<'_> {
    fn aborted(&self) -> bool {
        self.context.is_aborted(self.transfer_key)
    }

    fn send(&mut self, offset: u64, bytes: &[u8]) -> Result<(), content::SendFailure> {
        if self.chunk_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.chunk_delay_ms));
        }
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let length = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let ack = execute_transfer(
            self.info,
            STATUS_SUCCESS,
            bytes.as_ptr().cast(),
            offset,
            length,
        );
        if !ack.succeeded() {
            return Err(content::SendFailure {
                offset: u64::try_from(offset).unwrap_or(u64::MAX),
                code: ack.code(),
            });
        }
        self.context.chunks_sent.fetch_add(1, Ordering::SeqCst);
        self.context
            .bytes_served
            .fetch_add(u64::try_from(length).unwrap_or(0), Ordering::SeqCst);
        Ok(())
    }
}

/// 以指定 `NTSTATUS`/`HRESULT` 回执失败，并把原因写进日志。
fn fail(
    context: &ProviderContext,
    info: &CfCallbackInfo,
    status: i32,
    offset: i64,
    length: i64,
    message: &str,
) {
    let ack = execute_transfer(info, status, std::ptr::null(), offset, length);
    if !ack.succeeded() {
        context.failed_acks.fetch_add(1, Ordering::SeqCst);
    }
    eprintln!(
        "{message} ack_status=0x{:08X} ack_hresult=0x{:08X}",
        status as u32,
        ack.code() as u32
    );
}

/// `CANCEL_FETCH_DATA`：只记录被取消的 `TransferKey`，让正在回填的循环在分段边界退出。
///
/// # Safety
///
/// 由 CFAPI 以有效指针调用。
unsafe extern "system" fn on_cancel_fetch_data(
    info: *const CfCallbackInfo,
    _params: *const CfCallbackParametersFetchData,
) {
    if info.is_null() {
        eprintln!("callback=cancel_fetch_data error=null_callback_info");
        return;
    }
    let Some(context) = PROVIDER.get() else {
        eprintln!("callback=cancel_fetch_data error=provider_context_missing");
        return;
    };
    let _guard = context.enter();
    let info = unsafe { &*info };
    let count = context.cancel_callbacks.fetch_add(1, Ordering::SeqCst) + 1;
    context.mark_abort(info.transfer_key);
    eprintln!(
        "callback=cancel_fetch_data count={count} transfer_key={} request_key={} path={}",
        info.transfer_key,
        info.request_key,
        unsafe { read_wide(info.normalized_path) }
    );
}

/// 最小回调表：`FETCH_DATA` + `CANCEL_FETCH_DATA`，以 `CF_CALLBACK_TYPE_NONE` 结束。
///
/// 表是 `'static`：CFAPI 在连接存活期间持续引用它；回调只通过 [`PROVIDER`] 取状态，
/// 因此不需要 `CallbackContext` 指针。
pub(crate) static CALLBACK_TABLE: [CfCallbackRegistration; 3] = [
    CfCallbackRegistration {
        callback_type: CF_CALLBACK_TYPE_FETCH_DATA,
        callback: Some(on_fetch_data),
    },
    CfCallbackRegistration {
        callback_type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
        callback: Some(on_cancel_fetch_data),
    },
    CfCallbackRegistration {
        callback_type: CF_CALLBACK_TYPE_NONE,
        callback: None,
    },
];

// ---------------------------------------------------------------------------
// 连接生命周期
// ---------------------------------------------------------------------------

/// 一个已连接的同步根：连接键 + 上下文。`close` 负责断开、等待回调静默并上报计数。
pub struct ProviderSession {
    root: PathBuf,
    key: CfConnectionKey,
    context: Arc<ProviderContext>,
}

impl ProviderSession {
    /// 安装上下文并连接同步根（调用方负责注册）。
    pub fn open(root: &Path, config: ProviderConfig) -> Result<Self, StageError> {
        let context = install_provider(config)?;
        let key = connect(root)?;
        record(
            "provider.session",
            &[
                ("path", root.display().to_string()),
                ("source", config.source.name().to_string()),
                ("latency_ms", config.latency_ms.to_string()),
                ("timeout_ms", config.timeout_ms.to_string()),
                ("chunk_delay_ms", config.chunk_delay_ms.to_string()),
                ("callbacks", "fetch_data,cancel_fetch_data".to_string()),
            ],
            HResult::S_OK,
        );
        Ok(Self {
            root: root.to_path_buf(),
            key,
            context,
        })
    }

    /// 本次连接使用的上下文。
    pub fn context(&self) -> &Arc<ProviderContext> {
        &self.context
    }

    /// 连接键（`CfDisconnectSyncRoot` 的入参）。
    pub(crate) fn key(&self) -> CfConnectionKey {
        self.key
    }

    /// 断开连接并确认没有回调仍在执行。
    pub fn close(self) -> Result<(), StageError> {
        let hr = disconnect(self.key)?;
        // CfDisconnectSyncRoot 之后不再接受新回调；这里再等一次静默，确保没有回调在我们上报
        // 结果或结束进程时仍在执行（上下文本身是进程级 Arc，不存在悬垂指针）。
        self.context.wait_idle(DEFAULT_DRAIN_TIMEOUT)?;
        record(
            "provider.drain",
            &[
                ("path", self.root.display().to_string()),
                ("in_flight", self.context.stats().in_flight.to_string()),
            ],
            hr,
        );
        Ok(())
    }
}

/// `CfConnectSyncRoot`：安装回调表并取得连接键。
fn connect(root: &Path) -> Result<CfConnectionKey, StageError> {
    let root_w = wide(root);
    let mut key = CfConnectionKey::default();
    let hr = HResult::new(unsafe {
        CfConnectSyncRoot(
            root_w.as_ptr(),
            CALLBACK_TABLE.as_ptr(),
            std::ptr::null_mut(),
            // 请求完整路径，使 FETCH_DATA 日志能直接显示是哪个条目。
            CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH,
            &mut key,
        )
    });
    record(
        "connect",
        &[
            ("path", root.display().to_string()),
            ("callbacks", "fetch_data,cancel_fetch_data".to_string()),
            ("connect_flags", "require_full_file_path".to_string()),
            ("connection_key", format!("0x{:X}", key.internal)),
        ],
        hr,
    );
    if hr.succeeded() {
        Ok(key)
    } else {
        Err(StageError::new(
            "connect",
            hr,
            format!("CfConnectSyncRoot 失败：{}", root.display()),
        ))
    }
}

/// `CfDisconnectSyncRoot`：释放连接键；之后注册仍保留，直到注销。
fn disconnect(key: CfConnectionKey) -> Result<HResult, StageError> {
    let hr = HResult::new(unsafe { CfDisconnectSyncRoot(key) });
    record(
        "disconnect",
        &[("connection_key", format!("0x{:X}", key.internal))],
        hr,
    );
    if hr.succeeded() {
        Ok(hr)
    } else {
        Err(StageError::new(
            "disconnect",
            hr,
            "CfDisconnectSyncRoot 失败".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// 读取验证
// ---------------------------------------------------------------------------

/// 一次读取的观察结果。
#[derive(Clone, Debug)]
pub struct ReadOutcome {
    /// 实际读取到的字节数。
    pub bytes: u64,
    /// 读到的字节的 SHA-256（小写十六进制）。
    pub sha256: String,
    /// 固定内容版本的期望 SHA-256。
    pub expected_sha256: String,
    /// 两份哈希是否一致。
    pub digest_match: bool,
    /// 打开句柄的耗时（毫秒）：`PARTIAL` 策略下打开不应触发整份水合。
    pub open_ms: u128,
    /// 读取本身的耗时（毫秒），水合请求就是在这里发生的。
    pub read_ms: u128,
    /// 从打开到读完/失败的耗时（毫秒）。
    pub elapsed_ms: u128,
    /// 失败时的 Win32 错误码（成功为 `None`）。
    pub error_code: Option<u32>,
    /// 失败时的错误描述（成功为空）。
    pub error: String,
    /// 读取结束后提供方计数。
    pub stats: ProviderStats,
}

/// 读取模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadMode {
    /// 整份条目。
    Full,
    /// 指定区间（`length` 必须 > 0 且落在内容内）。
    Range {
        /// 起始偏移。
        offset: u64,
        /// 字节数。
        length: u64,
    },
}

impl ReadMode {
    fn start_and_length(self, size: u64) -> Result<(u64, u64), StageError> {
        match self {
            Self::Full => Ok((0, size)),
            Self::Range { offset, length } => content::check_range(
                i64::try_from(offset).unwrap_or(i64::MAX),
                i64::try_from(length).unwrap_or(i64::MAX),
                size,
            )
            .map_err(|error| {
                StageError::new(
                    "read.preflight",
                    HResult::new(0x8007_0057u32 as i32),
                    error.to_string(),
                )
            }),
        }
    }
}

/// 原型条目的绝对路径（`release`/`guard` 复用）。
pub(crate) fn entry_path(root: &Path, name: &str) -> PathBuf {
    root.join(PROBE_DIRECTORY).join(name)
}

/// 原型条目的逻辑大小；未知名目报错（`release`/`guard` 复用）。
pub(crate) fn require_entry(name: &str) -> Result<u64, StageError> {
    content::entry_size(name).ok_or_else(|| {
        StageError::new(
            "read.preflight",
            HResult::new(0x8007_0057u32 as i32),
            format!(
                "未知条目 {name}（原型条目：{}）",
                PROBE_FILES
                    .iter()
                    .map(|(entry, _)| *entry)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        )
    })
}

/// 读取条目（整份或区间），并把结果与固定内容版本的期望哈希逐字节比对。
pub fn read_entry(root: &Path, name: &str, mode: ReadMode) -> Result<ReadOutcome, StageError> {
    let size = require_entry(name)?;
    let (offset, length) = mode.start_and_length(size)?;
    let path = entry_path(root, name);
    let started = Instant::now();
    let (attempt, open_ms, read_ms) = read_bytes(&path, offset, length);
    let elapsed_ms = started.elapsed().as_millis();
    let context = provider_context();
    // 读完时回调可能正在收尾；先等回调静默再取计数，避免把“已回填但还没计数”读成 0 段。
    if let Some(context) = context.as_ref() {
        context.wait_idle(DEFAULT_DRAIN_TIMEOUT)?;
    }
    let stats = context
        .as_ref()
        .map_or(ProviderStats::default(), |c| c.stats());

    let mut expected = vec![0u8; usize::try_from(length).unwrap_or(0)];
    content::fill_range(name, offset, &mut expected).map_err(|error| {
        StageError::new(
            "read.preflight",
            HResult::new(0x8007_0057u32 as i32),
            format!("生成期望内容失败：{error}"),
        )
    })?;
    let expected_sha256 = content::sha256_hex(&expected);

    match attempt {
        Ok(bytes) => {
            let sha256 = content::sha256_hex(&bytes);
            Ok(ReadOutcome {
                bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                digest_match: sha256 == expected_sha256 && bytes == expected,
                sha256,
                expected_sha256,
                open_ms,
                read_ms,
                elapsed_ms,
                error_code: None,
                error: String::new(),
                stats,
            })
        }
        Err(error) => Ok(ReadOutcome {
            bytes: 0,
            sha256: String::new(),
            expected_sha256,
            digest_match: false,
            open_ms,
            read_ms,
            elapsed_ms,
            error_code: error.raw_os_error().map(|code| code as u32),
            error: error.to_string(),
            stats,
        }),
    }
}

/// 打开占位符、定位到 `offset` 并读取 `length` 字节；返回（结果，打开耗时，读取耗时）。
fn read_bytes(path: &Path, offset: u64, length: u64) -> (std::io::Result<Vec<u8>>, u128, u128) {
    use std::io::{Read, Seek, SeekFrom};
    let open_started = Instant::now();
    let opened = std::fs::File::open(path);
    let open_ms = open_started.elapsed().as_millis();
    let mut file = match opened {
        Ok(file) => file,
        Err(error) => return (Err(error), open_ms, 0),
    };
    let read_started = Instant::now();
    let mut bytes = vec![0u8; usize::try_from(length).unwrap_or(0)];
    let result = (|| {
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))?;
        }
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    })();
    let read_ms = read_started.elapsed().as_millis();
    (result, open_ms, read_ms)
}

/// 取消观察结果。
#[derive(Clone, Debug)]
pub struct CancelOutcome {
    /// 发出取消前是否观察到 `FETCH_DATA` 已经开始。
    pub fetch_started: bool,
    /// 发出取消前读线程是否已经返回（此时系统没有可取消的 I/O）。
    pub read_finished_before_cancel: bool,
    /// 从开始等待 `FETCH_DATA` 到观察到它的耗时（毫秒）。
    pub fetch_wait_ms: u128,
    /// 发出取消时相对读开始的毫秒数。
    pub cancel_issued_ms: u128,
    /// 发出取消到读线程返回的耗时（毫秒）。
    pub cancel_to_return_ms: u128,
    /// 取消调用的 Win32 结果：0 表示成功，其他值是最后一次 `GetLastError`。
    pub cancel_code: u32,
    /// 读操作是否以错误结束。
    pub read_failed: bool,
    /// 读操作的 Win32 错误码（成功为 `None`）。
    pub error_code: Option<u32>,
    /// 读操作的错误描述。
    pub error: String,
    /// 读到的字节的 SHA-256（读失败时为空）。
    pub sha256: String,
    /// 读到的字节是否等于固定内容版本（读失败时为 false）。
    pub digest_match: bool,
    /// 取消 + 读取结束之后的提供方计数。
    pub stats: ProviderStats,
}

/// 取消时序：用于区分“取消真的发生在传输中”与“读已经结束或系统没有可取消的 I/O”。
struct CancelTiming {
    fetch_started: bool,
    read_finished_before_cancel: bool,
    fetch_wait_ms: u128,
    cancel_issued_ms: u128,
    cancel_to_return_ms: u128,
    /// 取消调用的 Win32 结果：0 表示成功，其他值是最后一次 `GetLastError`。
    cancel_code: u32,
}

/// 读取期间取消读线程的 I/O，验证 `CANCEL_FETCH_DATA` 语义。
///
/// 读取方是独立线程：它在打开/读取占位符时才会向提供方请求内容（S011 实测平台在
/// “打开带读权限的占位符”这一步就发起整份水合，`PARTIAL` 策略下也一样），因此取消方必须
/// 针对**那个线程**发取消。取消发生在 `FETCH_DATA` 已开始之后（`cancel_after_ms` 从观察到
/// 第一个回调开始计时），返回前读线程必须已经退出（用 `wait_idle` 确认没有回调仍在执行）。
pub fn read_with_cancel(
    root: &Path,
    name: &str,
    cancel_after_ms: u64,
) -> Result<CancelOutcome, StageError> {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;

    let size = require_entry(name)?;
    let path = entry_path(root, name);
    let context = provider_context().ok_or_else(|| {
        StageError::new(
            "cancel.preflight",
            HResult::new(0x8007_0057u32 as i32),
            "取消验证需要已安装的提供方上下文".to_string(),
        )
    })?;
    let read_started = Instant::now();
    let reader_thread_id = AtomicU64::new(0);
    let reader_file_handle = AtomicIsize::new(0);
    let reader_done = AtomicBool::new(false);

    let (read_result, buffer, timing) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            reader_thread_id.store(u64::from(unsafe { GetCurrentThreadId() }), Ordering::SeqCst);
            let mut buffer = vec![0u8; usize::try_from(size).unwrap_or(0)];
            let open_started = Instant::now();
            let opened = std::fs::File::open(&path);
            let open_ms = open_started.elapsed().as_millis();
            let result = match opened {
                Ok(mut file) => {
                    reader_file_handle.store(file.as_raw_handle() as isize, Ordering::SeqCst);
                    let read_started = Instant::now();
                    let result = file.read_exact(&mut buffer);
                    eprintln!(
                        "cancel.reader_done t_ms={} open_ms={open_ms} read_elapsed_ms={} result={} sha256={}",
                        context.elapsed_ms(),
                        read_started.elapsed().as_millis(),
                        if result.is_ok() { "ok" } else { "error" },
                        content::sha256_hex(&buffer)
                    );
                    result
                }
                Err(error) => {
                    eprintln!(
                        "cancel.reader_open_failed t_ms={} open_ms={open_ms} error=\"{error}\"",
                        context.elapsed_ms()
                    );
                    Err(error)
                }
            };
            reader_done.store(true, Ordering::SeqCst);
            (result, buffer)
        });

        // 等到水合真的开始再取消，避免“取消了一个还没发出的请求”这种假证据。
        let fetch_wait_started = Instant::now();
        let deadline = fetch_wait_started + Duration::from_millis(5_000);
        while !context.fetch_started()
            && !reader_done.load(Ordering::SeqCst)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let fetch_wait_ms = fetch_wait_started.elapsed().as_millis();
        let fetch_started = context.fetch_started();
        std::thread::sleep(Duration::from_millis(cancel_after_ms));
        let cancel_at = Instant::now();
        let cancel_issued_ms = read_started.elapsed().as_millis();
        let read_finished_before_cancel = reader_done.load(Ordering::SeqCst);

        // 先按线程取消（读线程此时阻塞在“打开占位符触发的同步水合”里），失败再按已打开的
        // 句柄取消该条目上所有挂起的 I/O。两条路径都记录真实 `GetLastError`，不假装已取消。
        let cancel_code = cancel_reader(
            reader_thread_id.load(Ordering::SeqCst),
            reader_file_handle.load(Ordering::SeqCst),
        );
        let result = match reader.join() {
            Ok((result, buffer)) => (result, buffer),
            Err(_) => (
                Err(std::io::Error::other("读线程 panic")),
                vec![0u8; usize::try_from(size).unwrap_or(0)],
            ),
        };
        let cancel_to_return_ms = cancel_at.elapsed().as_millis();
        (
            result.0,
            result.1,
            CancelTiming {
                fetch_started,
                read_finished_before_cancel,
                fetch_wait_ms,
                cancel_issued_ms,
                cancel_to_return_ms,
                cancel_code,
            },
        )
    });

    // 取消后的回调收尾可能晚于读线程返回；等它退出再取计数，否则会误判“没有退出”。
    context.wait_idle(DEFAULT_DRAIN_TIMEOUT)?;
    let stats = context.stats();
    let mut expected = vec![0u8; usize::try_from(size).unwrap_or(0)];
    content::fill_range(name, 0, &mut expected).map_err(|error| {
        StageError::new(
            "cancel.preflight",
            HResult::new(0x8007_0057u32 as i32),
            format!("生成期望内容失败：{error}"),
        )
    })?;
    let read_sha256 = content::sha256_hex(&buffer);
    let read_digest_match = buffer == expected;
    Ok(match read_result {
        Ok(()) => CancelOutcome {
            fetch_started: timing.fetch_started,
            read_finished_before_cancel: timing.read_finished_before_cancel,
            fetch_wait_ms: timing.fetch_wait_ms,
            cancel_issued_ms: timing.cancel_issued_ms,
            cancel_to_return_ms: timing.cancel_to_return_ms,
            cancel_code: timing.cancel_code,
            read_failed: false,
            error_code: None,
            error: String::new(),
            sha256: read_sha256,
            digest_match: read_digest_match,
            stats,
        },
        Err(error) => CancelOutcome {
            fetch_started: timing.fetch_started,
            read_finished_before_cancel: timing.read_finished_before_cancel,
            fetch_wait_ms: timing.fetch_wait_ms,
            cancel_issued_ms: timing.cancel_issued_ms,
            cancel_to_return_ms: timing.cancel_to_return_ms,
            cancel_code: timing.cancel_code,
            read_failed: true,
            error_code: error.raw_os_error().map(|code| code as u32),
            error: error.to_string(),
            sha256: read_sha256,
            digest_match: read_digest_match,
            stats,
        },
    })
}

/// 取消读线程上挂起的同步 I/O；优先按线程句柄（覆盖“打开触发的同步水合”等待），
/// 退回按文件句柄取消该条目上所有挂起的 I/O。返回值 0 表示某条路径成功，否则为最后的
/// `GetLastError`。
fn cancel_reader(thread_id: u64, file_handle: isize) -> u32 {
    let thread_id = u32::try_from(thread_id).unwrap_or(0);
    let thread_handle = unsafe { OpenThread(THREAD_TERMINATE, 0, thread_id) };
    let mut last_error = if thread_handle.is_null() {
        let code = unsafe { GetLastError() };
        eprintln!("cancel: OpenThread(thread_id={thread_id}) 失败（GetLastError={code}）");
        code
    } else {
        let canceled = unsafe { CancelSynchronousIo(thread_handle) };
        unsafe { close_handle(thread_handle) };
        if canceled != 0 {
            return 0;
        }
        let code = unsafe { GetLastError() };
        eprintln!("cancel: CancelSynchronousIo 返回 0（GetLastError={code}）");
        code
    };
    if file_handle != 0 {
        let handle = file_handle as Handle;
        let canceled = unsafe { CancelIoEx(handle, std::ptr::null()) };
        if canceled != 0 {
            return 0;
        }
        last_error = unsafe { GetLastError() };
        eprintln!("cancel: CancelIoEx 返回 0（GetLastError={last_error}）");
    }
    last_error
}

/// 已水合数据的第三方证据：`CfGetPlaceholderInfo` 报告的本地位数（不触发水合）。
pub fn on_disk_data_size(root: &Path, name: &str) -> Result<i64, StageError> {
    let path = entry_path(root, name);
    super::prototype::on_disk_data_size(&path)
}

// ---------------------------------------------------------------------------
// 命令入口
// ---------------------------------------------------------------------------

/// `read` 子命令的期望结果：在线读取必须成功且哈希一致，离线/超时场景必须明确失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadExpectation {
    /// 期望读取成功：整份/区间字节与固定内容版本的 SHA-256 一致。
    Success,
    /// 期望明确失败：必须带非零错误码，且不能返回任何字节。
    Failure,
}

impl ReadExpectation {
    /// 日志里的名字。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
        }
    }
}

/// 条目必须是原型占位符：不存在的条目、普通文件都不能被当成“水合失败”的证据。
fn require_placeholder(root: &Path, name: &str) -> Result<(), StageError> {
    let path = entry_path(root, name);
    match super::prototype::try_observe(&path)? {
        Some(observation) if observation.is_placeholder() => Ok(()),
        Some(_) => Err(StageError::new(
            "read.preflight",
            HResult::new(HRESULT_FROM_WIN32_ERROR_NOT_A_CLOUD_FILE),
            format!("{} 不是云占位符，拒绝用它做水合验证", path.display()),
        )),
        None => Err(StageError::new(
            "read.preflight",
            HResult::new(0x8007_0002u32 as i32), // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
            format!("{} 不存在，先运行 create", path.display()),
        )),
    }
}

/// `read` 子命令：连接同步根 → 读取整份/区间 → 与固定内容版本比对 → 断开并等待静默。
///
/// 返回 `Ok(())` 表示“实际观察符合声明期望”；不符时返回带阶段与错误码的
/// [`StageError`]，由命令入口以非零退出码结束（不允许把意外成功/意外失败当成功）。
pub fn run_read(
    root: &Path,
    name: &str,
    mode: ReadMode,
    config: ProviderConfig,
    expect: ReadExpectation,
) -> Result<(), StageError> {
    let size = require_entry(name)?;
    let (offset, length) = mode.start_and_length(size)?;
    require_placeholder(root, name)?;
    let session = ProviderSession::open(root, config)?;
    let outcome = read_entry(root, name, mode)?;
    let on_disk = on_disk_data_size(root, name).unwrap_or(-1);
    let hr = outcome.error_code.map_or(HResult::S_OK, hresult_from_win32);
    record(
        "read.result",
        &[
            ("path", format!("{PROBE_DIRECTORY}/{name}")),
            ("offset", offset.to_string()),
            ("length", length.to_string()),
            ("bytes", outcome.bytes.to_string()),
            ("sha256", empty_as_dash(&outcome.sha256)),
            ("expected_sha256", outcome.expected_sha256.clone()),
            ("digest_match", outcome.digest_match.to_string()),
            ("on_disk_data_size", on_disk.to_string()),
            ("source", config.source.name().to_string()),
            ("latency_ms", config.latency_ms.to_string()),
            ("timeout_ms", config.timeout_ms.to_string()),
            ("open_ms", outcome.open_ms.to_string()),
            ("read_ms", outcome.read_ms.to_string()),
            ("elapsed_ms", outcome.elapsed_ms.to_string()),
            ("outcome", outcome_kind(&outcome)),
            ("error", empty_as_dash(&outcome.error)),
            ("expect", expect.name().to_string()),
        ],
        hr,
    );
    record("provider.stats", &outcome.stats.fields(), HResult::S_OK);
    let context = Arc::clone(session.context());
    session.close()?;
    let after = context.stats();
    if after.in_flight != 0 {
        return Err(StageError::new(
            "read.drain",
            HResult::new(0x8007_05B4u32 as i32),
            format!("断开后仍有在途回调：in_flight={}", after.in_flight),
        ));
    }

    match expect {
        ReadExpectation::Success => {
            if let Some(code) = outcome.error_code {
                return Err(StageError::new(
                    "read.result",
                    HResult::new(crate::cfapi::prototype::hresult_from_win32(code).code()),
                    format!("读取失败（win32_error={code}）：{}", outcome.error),
                ));
            }
            if !outcome.digest_match {
                return Err(StageError::new(
                    "read.result",
                    HResult::S_OK,
                    format!(
                        "内容与固定版本不一致：bytes={} sha256={} expected={}",
                        outcome.bytes, outcome.sha256, outcome.expected_sha256
                    ),
                ));
            }
            if outcome.bytes != length {
                return Err(StageError::new(
                    "read.result",
                    HResult::S_OK,
                    format!("读到的字节数 {} 与请求长度 {length} 不符", outcome.bytes),
                ));
            }
            Ok(())
        }
        ReadExpectation::Failure => match outcome.error_code {
            None => Err(StageError::new(
                "read.result",
                HResult::S_OK,
                format!(
                    "读取意外成功（bytes={} sha256={}），离线/超时场景不得返回数据",
                    outcome.bytes, outcome.sha256
                ),
            )),
            Some(0) => Err(StageError::new(
                "read.result",
                HResult::S_OK,
                "读取失败但错误码为 0，无法作为明确失败证据".to_string(),
            )),
            Some(_) if outcome.bytes != 0 => Err(StageError::new(
                "read.result",
                HResult::S_OK,
                format!("读取失败却返回了 {} 字节数据", outcome.bytes),
            )),
            Some(_) => Ok(()),
        },
    }
}

/// `cancel` 子命令：读取大条目并在传输中途取消，验证回调中止、读取有界返回、句柄可关闭。
pub fn run_cancel(
    root: &Path,
    name: &str,
    config: ProviderConfig,
    cancel_after_ms: u64,
    require_cancel_callback: bool,
) -> Result<(), StageError> {
    require_entry(name)?;
    require_placeholder(root, name)?;
    let session = ProviderSession::open(root, config)?;
    let outcome = read_with_cancel(root, name, cancel_after_ms)?;
    let hr = outcome.error_code.map_or(HResult::S_OK, hresult_from_win32);
    record(
        "cancel.result",
        &[
            ("path", format!("{PROBE_DIRECTORY}/{name}")),
            ("cancel_after_ms", cancel_after_ms.to_string()),
            ("chunk_delay_ms", config.chunk_delay_ms.to_string()),
            ("fetch_started", outcome.fetch_started.to_string()),
            (
                "read_finished_before_cancel",
                outcome.read_finished_before_cancel.to_string(),
            ),
            ("fetch_wait_ms", outcome.fetch_wait_ms.to_string()),
            ("cancel_issued_ms", outcome.cancel_issued_ms.to_string()),
            ("cancel_code", outcome.cancel_code.to_string()),
            ("read_failed", outcome.read_failed.to_string()),
            (
                "read_sha256",
                String::from(if outcome.sha256.is_empty() {
                    "-"
                } else {
                    &outcome.sha256
                }),
            ),
            ("read_digest_match", outcome.digest_match.to_string()),
            ("read_error", empty_as_dash(&outcome.error)),
            (
                "cancel_to_return_ms",
                outcome.cancel_to_return_ms.to_string(),
            ),
            (
                "cancel_callbacks",
                outcome.stats.cancel_callbacks.to_string(),
            ),
            ("aborted_fetches", outcome.stats.aborted_fetches.to_string()),
            ("chunks_sent", outcome.stats.chunks_sent.to_string()),
            ("bytes_served", outcome.stats.bytes_served.to_string()),
            ("in_flight", outcome.stats.in_flight.to_string()),
        ],
        hr,
    );
    record("provider.stats", &outcome.stats.fields(), HResult::S_OK);
    let context = Arc::clone(session.context());
    // 取消后句柄必须能正常关闭：read_with_cancel 已把 File 交还这一层，close 前再确认静默。
    session.close()?;
    let after = context.stats();
    let mut failures: Vec<String> = Vec::new();
    if !outcome.fetch_started {
        failures.push("取消前未观察到 FETCH_DATA 开始".to_string());
    }
    if require_cancel_callback && outcome.stats.cancel_callbacks == 0 {
        failures.push("没有收到 CANCEL_FETCH_DATA 回调".to_string());
    }
    if require_cancel_callback && outcome.stats.aborted_fetches == 0 {
        failures.push("回填循环没有观察到取消（aborted_fetches=0）".to_string());
    }
    if outcome.cancel_to_return_ms > 5_000 {
        failures.push(format!(
            "取消到读线程返回耗时 {} ms，超过 5000 ms 上限",
            outcome.cancel_to_return_ms
        ));
    }
    if after.in_flight != 0 {
        failures.push(format!("断开后仍有在途回调：in_flight={}", after.in_flight));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(StageError::new(
            "cancel.result",
            HResult::S_OK,
            failures.join("；"),
        ))
    }
}

fn empty_as_dash(text: &str) -> String {
    if text.is_empty() {
        "-".to_string()
    } else {
        text.to_string()
    }
}

fn outcome_kind(outcome: &ReadOutcome) -> String {
    if outcome.error_code.is_some() {
        "failed".to_string()
    } else if outcome.digest_match {
        "matched".to_string()
    } else {
        "mismatched".to_string()
    }
}

// ---------------------------------------------------------------------------
// `hydrate`：范围水合与传输中断实验
// ---------------------------------------------------------------------------

/// `hydrate` 的观察结果。
#[derive(Clone, Debug)]
pub struct HydrateOutcome {
    /// 请求的起始偏移。
    pub requested_offset: i64,
    /// 请求的长度。
    pub requested_length: i64,
    /// 平台通过 `FETCH_DATA` 要求的范围（未发生水合请求时为 `None`）。
    pub platform_range: Option<(i64, i64)>,
    /// `CfHydratePlaceholder` 的返回值（被中断时保留平台的失败码）。
    pub hydrate_hresult: i32,
    /// 中断方式：`none`（等水合完成）、`close_handle`、`disconnect`。
    pub interruption: &'static str,
    /// 发出中断到水合线程返回的耗时（毫秒）。
    pub interrupt_to_return_ms: u128,
    /// 从发起到返回的总耗时（毫秒）。
    pub total_ms: u128,
    /// 水合后本地位数（`CfGetPlaceholderInfo`）。
    pub on_disk_data_size: i64,
    /// 提供方计数。
    pub stats: ProviderStats,
}

/// `hydrate` 子命令：用 `CfHydratePlaceholder` 让平台按**指定范围**请求内容。
///
/// 与“直接读文件”相比（S011 实测差异）：读取一个带读权限的占位符会在 `CreateFileW`
/// 内部触发整份水合，调用方拿不到句柄，也就无从中断；而 `CfHydratePlaceholder` 用只请求
/// `FILE_READ_ATTRIBUTES` 的句柄按范围请求，回调因此能收到真正的范围请求
/// （实测请求 4,000,000+1,048,576 时平台按页对齐要求 `offset=3,997,696 length=1,052,672`）。
///
/// `cancel_after_ms` 在传输中途关闭句柄、`disconnect_after_ms` 在传输中途
/// `CfDisconnectSyncRoot`：两者都必须在有限时间内返回，并且不留下在途回调。
pub fn run_hydrate(
    root: &Path,
    name: &str,
    mode: ReadMode,
    config: ProviderConfig,
    cancel_after_ms: Option<u64>,
    disconnect_after_ms: Option<u64>,
) -> Result<HydrateOutcome, StageError> {
    let size = require_entry(name)?;
    let (offset, length) = mode.start_and_length(size)?;
    require_placeholder(root, name)?;
    let path = entry_path(root, name);
    let path_w = wide(&path);
    let session = ProviderSession::open(root, config)?;
    let context = Arc::clone(session.context());
    let connection_key = session.key();

    let handle = unsafe { open_attributes(&path_w) };
    if handle == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };
        return Err(StageError::new(
            "hydrate.preflight",
            hresult_from_win32(code),
            format!("打开 {} 失败（GetLastError={code}）", path.display()),
        ));
    }
    let offset_i = i64::try_from(offset).unwrap_or(i64::MAX);
    let length_i = i64::try_from(length).unwrap_or(i64::MAX);
    let started = Instant::now();
    let interruption = if cancel_after_ms.is_some() {
        "close_handle"
    } else if disconnect_after_ms.is_some() {
        "disconnect"
    } else {
        "none"
    };

    // `HANDLE` 是裸指针，不能跨线程直接捕获；按位传递后在闭包内还原。
    let handle_bits = handle as isize;
    let (hydrate_hresult, interrupt_to_return_ms) = std::thread::scope(|scope| {
        let worker = scope.spawn(move || unsafe {
            CfHydratePlaceholder(
                handle_bits as Handle,
                offset_i,
                length_i,
                CF_HYDRATE_FLAG_NONE,
                std::ptr::null_mut(),
            )
        });
        let mut interrupt_at = Instant::now();
        let mut interrupted = false;
        let wait_for_fetch = || {
            let deadline = Instant::now() + Duration::from_millis(5_000);
            while !context.fetch_started() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        if let Some(cancel_after_ms) = cancel_after_ms {
            wait_for_fetch();
            std::thread::sleep(Duration::from_millis(cancel_after_ms));
            // 关闭句柄：读取方提前关掉文件是平台取消水合请求的文档化触发条件。
            unsafe { close_handle(handle_bits as Handle) };
            interrupted = true;
            interrupt_at = Instant::now();
        } else if let Some(disconnect_after_ms) = disconnect_after_ms {
            wait_for_fetch();
            std::thread::sleep(Duration::from_millis(disconnect_after_ms));
            // 传输中断开同步根：平台从此不再向本提供方要内容，在途分段会被拒绝。
            let hr = unsafe { CfDisconnectSyncRoot(connection_key) };
            eprintln!(
                "hydrate: 传输中 CfDisconnectSyncRoot hresult=0x{:08X} t_ms={}",
                hr as u32,
                context.elapsed_ms()
            );
            interrupted = true;
            interrupt_at = Instant::now();
        }
        let hresult = worker.join().unwrap_or(0);
        let elapsed = if interrupted {
            interrupt_at.elapsed().as_millis()
        } else {
            0
        };
        (hresult, elapsed)
    });
    if interruption != "close_handle" {
        unsafe { close_handle(handle) };
    }
    let total_ms = started.elapsed().as_millis();

    context.wait_idle(DEFAULT_DRAIN_TIMEOUT)?;
    let stats = context.stats();
    let platform_range = context.last_fetch_range();
    let on_disk = on_disk_data_size(root, name).unwrap_or(-1);
    record(
        "hydrate.result",
        &[
            ("path", format!("{PROBE_DIRECTORY}/{name}")),
            ("requested_offset", offset_i.to_string()),
            ("requested_length", length_i.to_string()),
            (
                "platform_offset",
                platform_range.map_or_else(|| "-".to_string(), |(value, _)| value.to_string()),
            ),
            (
                "platform_length",
                platform_range.map_or_else(|| "-".to_string(), |(_, value)| value.to_string()),
            ),
            (
                "hydrate_hresult",
                format!("0x{:08X}", hydrate_hresult as u32),
            ),
            ("interruption", interruption.to_string()),
            ("interrupt_to_return_ms", interrupt_to_return_ms.to_string()),
            ("total_ms", total_ms.to_string()),
            ("on_disk_data_size", on_disk.to_string()),
            ("fetch_requests", stats.fetch_requests.to_string()),
            ("chunks_sent", stats.chunks_sent.to_string()),
            ("bytes_served", stats.bytes_served.to_string()),
            ("cancel_callbacks", stats.cancel_callbacks.to_string()),
            ("aborted_fetches", stats.aborted_fetches.to_string()),
            ("failed_acks", stats.failed_acks.to_string()),
            ("in_flight", stats.in_flight.to_string()),
        ],
        HResult::new(hydrate_hresult),
    );
    record("provider.stats", &stats.fields(), HResult::S_OK);
    // 传输中断开过的连接不再重复断开。
    if interruption == "disconnect" {
        drop(session);
    } else {
        session.close()?;
    }
    if stats.in_flight != 0 {
        return Err(StageError::new(
            "hydrate.drain",
            HResult::new(0x8007_05B4u32 as i32),
            format!("断开后仍有在途回调：in_flight={}", stats.in_flight),
        ));
    }

    Ok(HydrateOutcome {
        requested_offset: offset_i,
        requested_length: length_i,
        platform_range,
        hydrate_hresult,
        interruption,
        interrupt_to_return_ms,
        total_ms,
        on_disk_data_size: on_disk,
        stats,
    })
}
