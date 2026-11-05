//! CFAPI 注册、连接、占位创建、查询、断开与注销（Windows 原型，S010）。
//!
//! 调用序列只用 CFAPI 认可的接口（design §14.2）：
//!
//! ```text
//! CfRegisterSyncRoot → CfConnectSyncRoot（最小回调表）
//!   → CfCreatePlaceholders（先目录，再其下文件）
//!   → CfGetPlaceholderStateFromFileInfo / CfGetPlaceholderInfo（只读查询）
//!   → CfDisconnectSyncRoot → CfUnregisterSyncRoot
//! ```
//!
//! 不手改 reparse point、不截断文件伪造脱水、不在条件不满足时标 in-sync：占位符用
//! `CF_PLACEHOLDER_CREATE_FLAG_NONE` 与 `CF_INSYNC_POLICY_NONE` 创建，查询时如实报告
//! `InSyncState`。内容水合属 S011：`FETCH_DATA` 回调立即用 `STATUS_NOT_SUPPORTED` 结束请求，
//! 绝不返回空数据。
//!
//! 每一步都通过 [`record`] 输出 `stage`、参数摘要与 `HRESULT`；失败返回 [`StageError`]，
//! 由命令入口以非零退出码结束。

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::identity::{ENCODED_LEN, EntryId, FileIdentity};
use super::lifecycle::{HResult, StageError, record};
use super::prototype_layout::{
    PROBE_DIRECTORY, PROBE_FILES, PROTOTYPE_ROOT_ID, entry_id_for, file_identity,
};
use super::raw::*;

/// 注册到系统的提供方名称（显示在 Explorer/系统设置里）。
pub const PROVIDER_NAME: &str = "vaultsyncd-prototype";
/// 提供方版本，随原型实现变化。
pub const PROVIDER_VERSION: &str = "0.1.0";

/// 原型的固定 `ProviderId`；每个同步根提供方需要稳定的 GUID。
const PROTOTYPE_PROVIDER_ID: Guid = Guid {
    data1: 0x7A5C_1F00,
    data2: 0x5EED,
    data3: 0x4A31,
    data4: [0x9B, 0x2E, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
};

/// `CF_CALLBACK_INFO` 中 `FileIdentity` 相对结构体起始的偏移（C 侧 `FileIdentity[1]` 位置）。
///
/// `CfGetPlaceholderInfo` 返回的 `CF_PLACEHOLDER_STANDARD_INFO` 是变长结构：定长前缀按 8
/// 字节对齐，`FileIdentity` 字节紧跟 `FileIdentityLength`（偏移 60）而不是 `size_of` 的 64。
const PLACEHOLDER_IDENTITY_OFFSET: usize = 60;

/// 一次 `FETCH_DATA` 回调计数：create 的读取探测据此判断系统是否真的向本提供方请求内容。
static FETCH_DATA_CALLBACKS: AtomicU64 = AtomicU64::new(0);
/// 一次 `CANCEL_FETCH_DATA` 回调计数。
static CANCEL_FETCH_DATA_CALLBACKS: AtomicU64 = AtomicU64::new(0);

/// `inspect` 的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inspection {
    /// 根已注册，且原型条目全部符合预期。
    Registered,
    /// 根未注册：带上系统返回的“不是同步根/不存在”类错误码，cleanup 之后的预期状态。
    NotRegistered(HResult),
}

/// 一个条目的只读观察结果。
///
/// 元数据来自 `FindFirstFileW` 与 `CfGetPlaceholderStateFromFindData`，占位信息来自
/// `CfGetPlaceholderInfo`；两步都只读元数据，不触发水合，因此可以在提供方未运行时查询
/// （`inspect` 就是这种情形）。
#[derive(Clone, Debug)]
pub struct EntryObservation {
    /// `CfGetPlaceholderStateFromFindData` 的结果位。
    ///
    /// 目录占位符经 `FindFirstFileW` 读到的属性里没有 `FILE_ATTRIBUTE_REPARSE_POINT`，
    /// 该函数因此返回 `NO_STATES`（S010 实测），占位判定以 `placeholder` 为准。
    pub placeholder_state: u32,
    /// `WIN32_FIND_DATAW.dwFileAttributes`
    pub attributes: u32,
    /// `WIN32_FIND_DATAW.dwReserved0`（reparse tag）
    pub reparse_tag: u32,
    /// 逻辑大小（`nFileSizeHigh`/`nFileSizeLow`，目录为 0）。
    pub logical_size: i64,
    /// 是否目录（`FILE_ATTRIBUTE_DIRECTORY`）。
    pub directory: bool,
    /// `CfGetPlaceholderInfo` 成功时的本地占用与身份；非占位符为 `None`。
    pub placeholder: Option<PlaceholderInfo>,
}

impl EntryObservation {
    /// 系统是否把它认作占位符。
    ///
    /// 两个独立来源：`CfGetPlaceholderInfo` 能读到占位元数据（对非占位符返回
    /// `ERROR_NOT_A_CLOUD_FILE`），或者 `CfGetPlaceholderStateFromFindData` 报告了
    /// `CF_PLACEHOLDER_STATE_PLACEHOLDER`。任一成立即视为占位符。
    pub fn is_placeholder(&self) -> bool {
        self.placeholder.is_some() || self.placeholder_state & CF_PLACEHOLDER_STATE_PLACEHOLDER != 0
    }

    /// 内容是否完全不在本地：没有 `PARTIAL`/`PARTIALLY_ON_DISK` 位，且本地数据大小为 0。
    pub fn is_fully_dehydrated(&self) -> bool {
        let state_dehydrated = self.placeholder_state
            & (CF_PLACEHOLDER_STATE_PARTIAL | CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK)
            == 0;
        state_dehydrated && self.on_disk_data_size() == 0
    }

    /// 本地实际数据字节数（非占位符按 0 处理）。
    pub fn on_disk_data_size(&self) -> i64 {
        self.placeholder
            .as_ref()
            .map(|info| info.on_disk_data_size)
            .unwrap_or(0)
    }

    /// `CF_IN_SYNC_STATE_*`（非占位符按 `NOT_IN_SYNC`=0 处理）。
    pub fn in_sync_state(&self) -> u32 {
        self.placeholder
            .as_ref()
            .map(|info| info.in_sync_state)
            .unwrap_or(0)
    }

    /// 占位判定的来源，写进日志以免把 `0x00000000` 的 find-data 状态误读成“不是占位符”。
    pub fn placeholder_source(&self) -> &'static str {
        match (
            self.placeholder.is_some(),
            self.placeholder_state & CF_PLACEHOLDER_STATE_PLACEHOLDER != 0,
        ) {
            (true, true) => "placeholder_info,find_data",
            (true, false) => "placeholder_info",
            (false, true) => "find_data",
            (false, false) => "none",
        }
    }

    /// 系统保存的 `FileIdentity` 原始字节（非占位符为空）。
    pub fn identity(&self) -> &[u8] {
        self.placeholder
            .as_ref()
            .map(|info| info.identity.as_slice())
            .unwrap_or(&[])
    }
}

/// `CF_PLACEHOLDER_STANDARD_INFO` 的定长前缀加身份尾部。
///
/// `CfGetPlaceholderInfo(CF_PLACEHOLDER_INFO_STANDARD)` 会写入前缀字段并把 `FileIdentity`
/// 追加在偏移 [`PLACEHOLDER_IDENTITY_OFFSET`] 处；这里多留 [`ENCODED_LEN`] 字节并保持 8 字节
/// 对齐，避免用未对齐指针读取。
#[repr(C)]
struct PlaceholderStandardBuffer {
    info: CfPlaceholderStandardInfo,
    identity_tail: [u8; ENCODED_LEN],
}

/// `CF_PLACEHOLDER_STANDARD_INFO` 里的身份与本地占用情况。
#[derive(Clone, Debug)]
pub struct PlaceholderInfo {
    /// 本地实际数据字节数；脱水占位符为 0。
    pub on_disk_data_size: i64,
    /// 已验证的数据字节数；S010 未验证内容，应为 0。
    pub validated_data_size: i64,
    /// 已修改但未提交的数据字节数；S010 不写入内容，应为 0。
    pub modified_data_size: i64,
    /// `CF_IN_SYNC_STATE_*`
    pub in_sync_state: u32,
    /// 系统保存的 `FileIdentity` 原始字节。
    pub identity: Vec<u8>,
}

/// 同步根提供方信息（`CfGetSyncRootInfoByPath(CF_SYNC_ROOT_INFO_PROVIDER)`）。
#[derive(Clone, Debug)]
pub struct SyncRootProvider {
    /// 提供方名称。
    pub provider_name: String,
    /// 提供方版本。
    pub provider_version: String,
    /// `CF_SYNC_PROVIDER_STATUS`
    pub provider_status: u32,
}

// ---------------------------------------------------------------------------
// 生命周期日志辅助
// ---------------------------------------------------------------------------

/// `HRESULT_FROM_WIN32(code)`：把 Win32 错误码转成 HRESULT 用于日志。
fn hresult_from_win32(code: u32) -> HResult {
    if code == 0 {
        HResult::S_OK
    } else if code <= 0xFFFF {
        HResult::new(((code & 0xFFFF) | 0x8007_0000u32) as i32)
    } else {
        HResult::new(code as i32)
    }
}

/// 把 `std::io::Error` 的 Win32 错误码转成 HRESULT。
fn hresult_from_io(error: &std::io::Error) -> HResult {
    match error.raw_os_error() {
        Some(code) => hresult_from_win32(code as u32),
        None => HResult::new(0x8000_4005u32 as i32), // E_FAIL
    }
}

/// 小写十六进制，用于打印不透明身份。
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// 路径的日志表示（客体路径为 ASCII，`display()` 足够）。
fn show(path: &Path) -> String {
    path.display().to_string()
}

/// 逗号分隔的固定文件清单，用于日志参数摘要。
fn probe_file_list() -> String {
    PROBE_FILES
        .iter()
        .map(|(name, size)| format!("{name}:{size}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// 转成以 NUL 结尾的 UTF-16，供 `LPCWSTR` 参数使用。
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// 字面量转成以 NUL 结尾的 UTF-16。
fn wide_str(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain([0]).collect()
}

/// 读取以 NUL 结尾的 UTF-16 文本（`PCWSTR`）。
unsafe fn read_wide(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
    }
}

/// 当前时间的 `FILETIME`（100 ns 自 1601-01-01），用于占位符的真实时间戳。
fn now_filetime() -> i64 {
    /// 1601-01-01 到 1970-01-01 的秒数。
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_epoch.as_secs().saturating_add(EPOCH_DIFF_SECS);
    let ticks = secs
        .saturating_mul(10_000_000)
        .saturating_add(u64::from(since_epoch.subsec_nanos()) / 100);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// 回调表（S010 只有“明确报错”的水合路径）
// ---------------------------------------------------------------------------

/// `FETCH_DATA`：S010 不提供内容。
///
/// 必须用一个明确的失败 NTSTATUS 结束请求：返回空数据会让读取方把半个文件当成功（design
/// I05、T06）。`CfExecute` 是文档要求的完成方式，在回调内同步调用即完成请求。
unsafe extern "system" fn on_fetch_data(
    info: *const CfCallbackInfo,
    params: *const CfCallbackParametersFetchData,
) {
    let count = FETCH_DATA_CALLBACKS.fetch_add(1, Ordering::SeqCst) + 1;
    if info.is_null() {
        eprintln!("callback=fetch_data count={count} error=null_callback_info");
        return;
    }
    let (
        connection_key,
        transfer_key,
        request_key,
        correlation_vector,
        file_size,
        file_identity_len,
        path,
        offset,
        length,
    ) = unsafe {
        let info = &*info;
        let (offset, length) = if params.is_null() {
            (0, info.file_size)
        } else {
            ((*params).required_file_offset, (*params).required_length)
        };
        (
            info.connection_key,
            info.transfer_key,
            info.request_key,
            info.correlation_vector,
            info.file_size,
            info.file_identity_length,
            read_wide(info.normalized_path),
            offset,
            length,
        )
    };
    eprintln!(
        "callback=fetch_data count={count} path={path} file_size={file_size} required_offset={offset} required_length={length} file_identity_length={file_identity_len} request=unsupported hresult=0x{:08X}",
        STATUS_NOT_SUPPORTED as u32
    );
    let op_info = CfOperationInfo {
        struct_size: u32::try_from(std::mem::size_of::<CfOperationInfo>()).unwrap_or(0),
        operation_type: CF_OPERATION_TYPE_TRANSFER_DATA,
        connection_key,
        transfer_key,
        // 回执必须带回回调的 CorrelationVector 与 RequestKey，否则 CfExecute 会以
        // ERROR_CLOUD_FILE_INVALID_REQUEST 拒绝（S010 实测）。
        correlation_vector,
        sync_status: std::ptr::null(),
        request_key,
    };
    let mut params = CfOperationParametersTransferData {
        param_size: CF_OPERATION_PARAMETERS_SIZE,
        padding: 0,
        flags: 0,
        completion_status: STATUS_NOT_SUPPORTED,
        buffer: std::ptr::null(),
        // 回执的 Offset/Length 必须与请求范围一致；CompletionStatus 才是失败原因。
        offset,
        length,
    };
    let hr = unsafe { CfExecute(&op_info, std::ptr::from_mut(&mut params).cast::<c_void>()) };
    eprintln!("callback=fetch_data.ack hresult=0x{:08X}", hr as u32);
}

/// `CANCEL_FETCH_DATA`：取消请求只需计数并记录，当前没有在途传输要中止。
unsafe extern "system" fn on_cancel_fetch_data(
    info: *const CfCallbackInfo,
    _params: *const CfCallbackParametersFetchData,
) {
    let count = CANCEL_FETCH_DATA_CALLBACKS.fetch_add(1, Ordering::SeqCst) + 1;
    let connection_key = if info.is_null() {
        CfConnectionKey::default()
    } else {
        unsafe { (*info).connection_key }
    };
    eprintln!(
        "callback=cancel_fetch_data count={count} connection_key=0x{:X}",
        connection_key.internal
    );
}

/// 最小回调表：`FETCH_DATA` + `CANCEL_FETCH_DATA`，以 `CF_CALLBACK_TYPE_NONE` 结束。
///
/// 表必须是 `'static`：CFAPI 在连接存活期间会持续引用这张表。
static CALLBACK_TABLE: [CfCallbackRegistration; 3] = [
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
// 只读查询
// ---------------------------------------------------------------------------

/// 读取条目的元数据与占位信息；不存在返回 `Ok(None)`。
///
/// 两步都不读取内容、不触发水合：`FindFirstFileW` 取属性/reparse tag/逻辑大小，
/// `CfGetPlaceholderInfo` 取占位元数据（本地占用字节数、in-sync 状态与 `FileIdentity`）。
fn try_observe(path: &Path) -> Result<Option<EntryObservation>, StageError> {
    let path_w = wide(path);
    let mut find = Win32FindData::default();
    let find_handle = unsafe { FindFirstFileW(path_w.as_ptr(), &mut find) };
    if find_handle == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };
        if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        return Err(StageError::new(
            "query.entry",
            hresult_from_win32(code),
            format!("FindFirstFileW({}) 失败（GetLastError={code}）", show(path)),
        ));
    }
    let placeholder_state = unsafe { CfGetPlaceholderStateFromFindData(&find) };
    unsafe { FindClose(find_handle) };

    let handle = unsafe { open_attributes(&path_w) };
    if handle == INVALID_HANDLE_VALUE {
        let code = unsafe { GetLastError() };
        return Err(StageError::new(
            "query.placeholder",
            hresult_from_win32(code),
            format!(
                "打开 {} 读取占位信息失败（GetLastError={code}）",
                show(path)
            ),
        ));
    }
    let placeholder = read_placeholder_info(handle, path);
    unsafe { close_handle(handle) };
    let placeholder = placeholder?;

    Ok(Some(EntryObservation {
        placeholder_state,
        attributes: find.file_attributes,
        reparse_tag: find.reserved0,
        logical_size: find.logical_size(),
        directory: find.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        placeholder,
    }))
}

/// 与 [`try_observe`] 相同，但条目不存在视为错误。
fn observe(path: &Path) -> Result<EntryObservation, StageError> {
    match try_observe(path)? {
        Some(observation) => Ok(observation),
        None => Err(StageError::new(
            "query.entry",
            hresult_from_win32(ERROR_FILE_NOT_FOUND),
            format!("条目不存在：{}", show(path)),
        )),
    }
}

/// 用已打开的句柄读取 `CF_PLACEHOLDER_STANDARD_INFO`。
///
/// 非占位符返回 `Ok(None)`（`ERROR_NOT_A_CLOUD_FILE` = 376）；其他失败原样上报，不当作
/// “没有占位信息”吞掉。
fn read_placeholder_info(
    handle: Handle,
    path: &Path,
) -> Result<Option<PlaceholderInfo>, StageError> {
    let mut buffer = PlaceholderStandardBuffer {
        info: CfPlaceholderStandardInfo::default(),
        identity_tail: [0u8; ENCODED_LEN],
    };
    let mut returned = 0u32;
    let buffer_len = u32::try_from(std::mem::size_of::<PlaceholderStandardBuffer>()).unwrap_or(0);
    let hr = HResult::new(unsafe {
        CfGetPlaceholderInfo(
            handle,
            CF_PLACEHOLDER_INFO_STANDARD,
            std::ptr::from_mut(&mut buffer).cast::<c_void>(),
            buffer_len,
            &mut returned,
        )
    });
    if !hr.succeeded() {
        if hr.code() == HRESULT_FROM_WIN32_ERROR_NOT_A_CLOUD_FILE {
            return Ok(None);
        }
        return Err(StageError::new(
            "query.placeholder",
            hr,
            format!(
                "CfGetPlaceholderInfo(CF_PLACEHOLDER_INFO_STANDARD) 失败：{}",
                show(path)
            ),
        ));
    }
    let identity_len = usize::try_from(buffer.info.file_identity_length).unwrap_or(usize::MAX);
    if identity_len > ENCODED_LEN {
        return Err(StageError::new(
            "query.placeholder",
            hr,
            format!(
                "{} 的 FileIdentityLength={identity_len} 超过本实现的 {ENCODED_LEN}",
                show(path)
            ),
        ));
    }
    let raw = unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(&buffer).cast::<u8>(),
            std::mem::size_of::<PlaceholderStandardBuffer>(),
        )
    };
    let identity =
        raw[PLACEHOLDER_IDENTITY_OFFSET..PLACEHOLDER_IDENTITY_OFFSET + identity_len].to_vec();
    Ok(Some(PlaceholderInfo {
        on_disk_data_size: buffer.info.on_disk_data_size,
        validated_data_size: buffer.info.validated_data_size,
        modified_data_size: buffer.info.modified_data_size,
        in_sync_state: buffer.info.in_sync_state,
        identity,
    }))
}

/// 查询同步根注册信息；未注册时返回系统的错误码（不是“猜”出来的）。
fn query_sync_root(path: &Path) -> Result<SyncRootProvider, HResult> {
    let path_w = wide(path);
    let mut buffer = CfSyncRootProviderInfo {
        provider_status: 0,
        provider_name: [0; CF_MAX_PROVIDER_TEXT],
        provider_version: [0; CF_MAX_PROVIDER_TEXT],
    };
    let mut returned = 0u32;
    let buffer_len = u32::try_from(std::mem::size_of::<CfSyncRootProviderInfo>()).unwrap_or(0);
    let hr = HResult::new(unsafe {
        CfGetSyncRootInfoByPath(
            path_w.as_ptr(),
            CF_SYNC_ROOT_INFO_PROVIDER,
            std::ptr::from_mut(&mut buffer).cast::<c_void>(),
            buffer_len,
            &mut returned,
        )
    });
    if !hr.succeeded() {
        return Err(hr);
    }
    let provider_name = unsafe { read_wide(buffer.provider_name.as_ptr()) };
    let provider_version = unsafe { read_wide(buffer.provider_version.as_ptr()) };
    Ok(SyncRootProvider {
        provider_name,
        provider_version,
        provider_status: buffer.provider_status,
    })
}

// ---------------------------------------------------------------------------
// 注册 / 连接 / 断开 / 注销
// ---------------------------------------------------------------------------

/// `CfRegisterSyncRoot`：注册测试同步根。
///
/// 已注册时按 `ERROR_ALREADY_EXISTS` 处理并继续（`create` 可重复运行），但会把该 HRESULT
/// 记进日志，不假装第一次注册成功。
fn register(root: &Path) -> Result<(), StageError> {
    let provider_name = wide_str(PROVIDER_NAME);
    let provider_version = wide_str(PROVIDER_VERSION);
    let root_identity = PROTOTYPE_ROOT_ID.as_bytes().to_vec();
    let registration = CfSyncRegistration {
        struct_size: u32::try_from(std::mem::size_of::<CfSyncRegistration>()).unwrap_or(0),
        provider_name: provider_name.as_ptr(),
        provider_version: provider_version.as_ptr(),
        sync_root_identity: root_identity.as_ptr().cast::<c_void>(),
        sync_root_identity_length: u32::try_from(root_identity.len()).unwrap_or(0),
        file_identity: std::ptr::null(),
        file_identity_length: 0,
        provider_id: PROTOTYPE_PROVIDER_ID,
    };
    let policies = CfSyncPolicies {
        struct_size: u32::try_from(std::mem::size_of::<CfSyncPolicies>()).unwrap_or(0),
        hydration: CfHydrationPolicy {
            primary: CF_HYDRATION_POLICY_FULL,
            modifier: 0,
        },
        population: CfPopulationPolicy {
            primary: CF_POPULATION_POLICY_FULL,
            modifier: 0,
        },
        // 不让系统自动标记 in-sync：是否同步只由核心数据库判定（design §14.2）。
        in_sync: CF_INSYNC_POLICY_NONE,
        hard_link: CF_HARDLINK_POLICY_NONE,
        placeholder_management: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };
    let root_w = wide(root);
    let hr = HResult::new(unsafe {
        CfRegisterSyncRoot(
            root_w.as_ptr(),
            &registration,
            &policies,
            CF_REGISTER_FLAG_NONE,
        )
    });
    record(
        "register",
        &[
            ("path", show(root)),
            ("provider", PROVIDER_NAME.to_string()),
            ("provider_version", PROVIDER_VERSION.to_string()),
            (
                "provider_id",
                "7a5c1f00-5eed-4a31-9b2e-001122334455".to_string(),
            ),
            ("sync_root_identity_length", root_identity.len().to_string()),
            ("hydration_policy", "full".to_string()),
            ("in_sync_policy", "none".to_string()),
        ],
        hr,
    );
    if hr.succeeded() {
        return Ok(());
    }
    if hr.code() == HRESULT_FROM_WIN32_ERROR_ALREADY_EXISTS {
        record(
            "register",
            &[
                ("path", show(root)),
                ("status", "already_registered".to_string()),
            ],
            hr,
        );
        return Ok(());
    }
    Err(StageError::new(
        "register",
        hr,
        format!("CfRegisterSyncRoot 失败：{}", show(root)),
    ))
}

/// `CfConnectSyncRoot`：安装最小回调表并取得连接键。
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
            ("path", show(root)),
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
            format!("CfConnectSyncRoot 失败：{}", show(root)),
        ))
    }
}

/// `CfDisconnectSyncRoot`：释放连接键；之后注册仍保留，直到注销。
fn disconnect(key: CfConnectionKey) -> Result<(), StageError> {
    let hr = HResult::new(unsafe { CfDisconnectSyncRoot(key) });
    record(
        "disconnect",
        &[("connection_key", format!("0x{:X}", key.internal))],
        hr,
    );
    if hr.succeeded() {
        Ok(())
    } else {
        Err(StageError::new(
            "disconnect",
            hr,
            "CfDisconnectSyncRoot 失败".to_string(),
        ))
    }
}

/// 注销同步根，并用随后的查询确认注销生效（不靠猜错误码判断结果）。
fn unregister(root: &Path) -> Result<(), StageError> {
    let root_w = wide(root);
    let hr = HResult::new(unsafe { CfUnregisterSyncRoot(root_w.as_ptr()) });
    record("unregister", &[("path", show(root))], hr);
    match query_sync_root(root) {
        Err(query_hr) => {
            record(
                "sync_root.query",
                &[
                    ("path", show(root)),
                    ("status", "verified_not_registered".to_string()),
                ],
                query_hr,
            );
            Ok(())
        }
        Ok(provider) => Err(StageError::new(
            "unregister",
            hr,
            format!(
                "注销后仍能查到同步根（provider={} version={}）",
                provider.provider_name, provider.provider_version
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// 占位创建
// ---------------------------------------------------------------------------

/// 一次 `CfCreatePlaceholders` 的条目描述。
struct PlaceholderSpec {
    relative_name: String,
    logical_size: u64,
    attributes: u32,
    entry_id: EntryId,
}

/// 目录占位符描述。
fn directory_spec() -> PlaceholderSpec {
    PlaceholderSpec {
        relative_name: PROBE_DIRECTORY.to_string(),
        logical_size: 0,
        attributes: FILE_ATTRIBUTE_DIRECTORY,
        entry_id: entry_id_for(PROBE_DIRECTORY),
    }
}

/// 文件占位符描述（两个不同逻辑大小的固定条目）。
fn file_specs() -> Vec<PlaceholderSpec> {
    PROBE_FILES
        .iter()
        .map(|(name, size)| PlaceholderSpec {
            relative_name: (*name).to_string(),
            logical_size: *size,
            attributes: FILE_ATTRIBUTE_ARCHIVE,
            entry_id: entry_id_for(name),
        })
        .collect()
}

/// 在 `base` 下创建一批占位符，逐条目检查 `Result` 并记录 `HRESULT`。
fn create_placeholders(base: &Path, specs: &[PlaceholderSpec]) -> Result<(), StageError> {
    let base_w = wide(base);
    let names: Vec<Vec<u16>> = specs
        .iter()
        .map(|spec| wide_str(&spec.relative_name))
        .collect();
    let identities: Vec<FileIdentity> = specs
        .iter()
        .map(|spec| file_identity(&spec.relative_name))
        .collect();
    let encoded: Vec<[u8; ENCODED_LEN]> = identities.iter().map(FileIdentity::encode).collect();
    let now = now_filetime();
    let mut infos: Vec<CfPlaceholderCreateInfo> = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| CfPlaceholderCreateInfo {
            relative_file_name: names[index].as_ptr(),
            fs_metadata: CfFsMetadata {
                basic_info: FileBasicInfo {
                    creation_time: now,
                    last_access_time: now,
                    last_write_time: now,
                    change_time: now,
                    file_attributes: spec.attributes,
                },
                file_size: i64::try_from(spec.logical_size).unwrap_or(i64::MAX),
            },
            file_identity: encoded[index].as_ptr().cast::<c_void>(),
            file_identity_length: u32::try_from(ENCODED_LEN).unwrap_or(0),
            flags: CF_PLACEHOLDER_CREATE_FLAG_NONE,
            result: 0,
            create_usn: 0,
        })
        .collect();
    let mut processed = 0u32;
    let count = u32::try_from(infos.len()).unwrap_or(0);
    let hr = HResult::new(unsafe {
        CfCreatePlaceholders(
            base_w.as_ptr(),
            infos.as_mut_ptr(),
            count,
            CF_CREATE_FLAG_NONE,
            &mut processed,
        )
    });
    record(
        "create.placeholders",
        &[
            ("base", show(base)),
            ("count", count.to_string()),
            ("entries_processed", processed.to_string()),
            ("create_flags", "none".to_string()),
        ],
        hr,
    );

    let mut first_failure: Option<StageError> = None;
    for (index, spec) in specs.iter().enumerate() {
        let entry_hr = HResult::new(infos[index].result);
        record(
            "create.placeholder",
            &[
                ("path", format!("{}/{}", show(base), spec.relative_name)),
                (
                    "kind",
                    if spec.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        "directory"
                    } else {
                        "file"
                    }
                    .to_string(),
                ),
                ("logical_size", spec.logical_size.to_string()),
                ("attributes", format!("0x{:08X}", spec.attributes)),
                ("identity_length", encoded[index].len().to_string()),
                ("identity_entry_id", hex(spec.entry_id.as_bytes())),
                ("create_usn", infos[index].create_usn.to_string()),
            ],
            entry_hr,
        );
        if !entry_hr.succeeded() && first_failure.is_none() {
            first_failure = Some(StageError::new(
                "create.placeholder",
                entry_hr,
                format!("CfCreatePlaceholders 拒绝条目 {}", spec.relative_name),
            ));
        }
    }
    if !hr.succeeded() {
        return Err(StageError::new(
            "create.placeholders",
            hr,
            format!("CfCreatePlaceholders 失败（entries_processed={processed}）"),
        ));
    }
    if let Some(error) = first_failure {
        return Err(error);
    }
    Ok(())
}

/// 确保目录占位符存在：不存在则创建；已存在但不是占位符则拒绝覆盖。
fn ensure_directory_placeholder(root: &Path) -> Result<(), StageError> {
    let path = root.join(PROBE_DIRECTORY);
    match try_observe(&path)? {
        Some(observation) if observation.is_placeholder() => {
            record(
                "create.placeholder",
                &[
                    ("path", show(&path)),
                    ("kind", "directory".to_string()),
                    ("status", "already_placeholder".to_string()),
                ],
                HResult::S_OK,
            );
            Ok(())
        }
        Some(observation) => Err(StageError::new(
            "create.placeholder",
            HResult::new(HRESULT_FROM_WIN32_ERROR_ALREADY_EXISTS),
            format!(
                "{} 已存在但不是占位符（placeholder_state=0x{:08X}），拒绝覆盖",
                show(&path),
                observation.placeholder_state
            ),
        )),
        None => create_placeholders(root, &[directory_spec()]),
    }
}

/// 确保目录下的文件占位符存在，返回新建的条目数。
fn ensure_file_placeholders(root: &Path) -> Result<usize, StageError> {
    let directory = root.join(PROBE_DIRECTORY);
    let mut missing = Vec::new();
    for spec in file_specs() {
        let path = directory.join(&spec.relative_name);
        match try_observe(&path)? {
            Some(observation) if observation.is_placeholder() => {
                record(
                    "create.placeholder",
                    &[
                        ("path", show(&path)),
                        ("kind", "file".to_string()),
                        ("status", "already_placeholder".to_string()),
                        ("logical_size", observation.logical_size.to_string()),
                    ],
                    HResult::S_OK,
                );
            }
            Some(observation) => {
                return Err(StageError::new(
                    "create.placeholder",
                    HResult::new(HRESULT_FROM_WIN32_ERROR_ALREADY_EXISTS),
                    format!(
                        "{} 已存在但不是占位符（placeholder_state=0x{:08X}），拒绝覆盖",
                        show(&path),
                        observation.placeholder_state
                    ),
                ));
            }
            None => missing.push(spec),
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    create_placeholders(&directory, &missing)?;
    Ok(missing.len())
}

// ---------------------------------------------------------------------------
// 命令实现
// ---------------------------------------------------------------------------

/// `create`：注册、连接、创建占位、尝试读取（必须被明确拒绝），最后断开。
pub fn create(root: &Path) -> Result<(), StageError> {
    if !root.is_dir() {
        return Err(StageError::new(
            "create.preflight",
            hresult_from_win32(ERROR_PATH_NOT_FOUND),
            format!("同步根目录不存在：{}", show(root)),
        ));
    }
    record(
        "create.begin",
        &[
            ("path", show(root)),
            ("probe_directory", PROBE_DIRECTORY.to_string()),
            ("probe_files", probe_file_list()),
        ],
        HResult::S_OK,
    );
    register(root)?;
    let key = connect(root)?;
    ensure_directory_placeholder(root)?;
    let created = ensure_file_placeholders(root)?;
    probe_hydration_refusal(root)?;
    record(
        "create.summary",
        &[
            ("path", show(root)),
            ("placeholders_created", created.to_string()),
            ("probe_files", probe_file_list()),
        ],
        HResult::S_OK,
    );
    disconnect(key)
}

/// 读取一个脱水占位符：S011 之前必须失败并带明确错误码，不能返回 0 字节内容。
fn probe_hydration_refusal(root: &Path) -> Result<(), StageError> {
    let (name, size) = PROBE_FILES[0];
    let path = root.join(PROBE_DIRECTORY).join(name);
    let before = FETCH_DATA_CALLBACKS.load(Ordering::SeqCst);
    match std::fs::read(&path) {
        Ok(bytes) => Err(StageError::new(
            "hydration.probe",
            HResult::S_OK,
            format!(
                "读取 {} 意外成功（{} 字节）：S010 未实现水合，不得当成功",
                show(&path),
                bytes.len()
            ),
        )),
        Err(error) => {
            let after = FETCH_DATA_CALLBACKS.load(Ordering::SeqCst);
            let code = error.raw_os_error().unwrap_or(0);
            record(
                "hydration.probe",
                &[
                    ("path", show(&path)),
                    ("logical_size", size.to_string()),
                    ("outcome", "read_failed_as_expected".to_string()),
                    ("error", error.to_string()),
                    ("win32_error", code.to_string()),
                    (
                        "fetch_data_callbacks",
                        after.saturating_sub(before).to_string(),
                    ),
                    ("bytes_returned", "none".to_string()),
                ],
                HResult::new(code),
            );
            Ok(())
        }
    }
}

/// `inspect`：只读查询注册信息与每个原型条目的占位/大小/身份。
pub fn inspect(root: &Path) -> Result<Inspection, StageError> {
    if !root.is_dir() {
        return Err(StageError::new(
            "inspect.preflight",
            hresult_from_win32(ERROR_PATH_NOT_FOUND),
            format!("同步根目录不存在：{}", show(root)),
        ));
    }
    match query_sync_root(root) {
        Ok(provider) => {
            record(
                "sync_root.query",
                &[
                    ("path", show(root)),
                    ("status", "registered".to_string()),
                    ("provider", provider.provider_name),
                    ("provider_version", provider.provider_version),
                    (
                        "provider_status",
                        format!("0x{:08X}", provider.provider_status),
                    ),
                    ("expected_root_id", hex(PROTOTYPE_ROOT_ID.as_bytes())),
                ],
                HResult::S_OK,
            );
        }
        Err(hr) => {
            record(
                "sync_root.query",
                &[
                    ("path", show(root)),
                    ("status", "not_registered".to_string()),
                    ("probe_files", probe_file_list()),
                ],
                hr,
            );
            return Ok(Inspection::NotRegistered(hr));
        }
    }

    inspect_directory(root)?;
    for (name, size) in PROBE_FILES {
        inspect_file(root, name, size)?;
    }
    record(
        "inspect.summary",
        &[
            ("path", show(root)),
            ("entries", (PROBE_FILES.len() + 1).to_string()),
            ("probe_files", probe_file_list()),
        ],
        HResult::S_OK,
    );
    Ok(Inspection::Registered)
}

/// 校验目录占位符。
fn inspect_directory(root: &Path) -> Result<(), StageError> {
    let path = root.join(PROBE_DIRECTORY);
    let relative = PROBE_DIRECTORY.to_string();
    let observation = observe(&path)?;
    let expected = file_identity(&relative);
    let mut fields = vec![
        ("path", relative.clone()),
        ("kind", "directory".to_string()),
        ("placeholder", observation.is_placeholder().to_string()),
        (
            "placeholder_source",
            observation.placeholder_source().to_string(),
        ),
        (
            "placeholder_state",
            format!("0x{:08X}", observation.placeholder_state),
        ),
        ("attributes", format!("0x{:08X}", observation.attributes)),
        ("reparse_tag", format!("0x{:08X}", observation.reparse_tag)),
        ("directory", observation.directory.to_string()),
        ("logical_size", observation.logical_size.to_string()),
        (
            "on_disk_data_size",
            observation.on_disk_data_size().to_string(),
        ),
        ("in_sync_state", observation.in_sync_state().to_string()),
        ("identity_length", observation.identity().len().to_string()),
    ];
    fields.extend(identity_fields(observation.identity(), &expected)?);
    record("inspect.entry", &fields, HResult::S_OK);

    if !observation.directory {
        return Err(StageError::new(
            "inspect.entry",
            HResult::S_OK,
            format!("{relative} 不是目录"),
        ));
    }
    if observation.on_disk_data_size() != 0 {
        return Err(StageError::new(
            "inspect.entry",
            HResult::S_OK,
            format!(
                "{relative} 的 on_disk_data_size={}，目录占位不应有本地数据",
                observation.on_disk_data_size()
            ),
        ));
    }
    require_placeholder(&relative, &observation)
}

/// 校验一个文件占位符：占位状态、逻辑大小、脱水状态与身份。
fn inspect_file(root: &Path, name: &str, expected_size: u64) -> Result<(), StageError> {
    let path = root.join(PROBE_DIRECTORY).join(name);
    let relative = format!("{PROBE_DIRECTORY}/{name}");
    let observation = observe(&path)?;
    // 身份按“相对其父目录的条目名”派生，与 create 时写入的 blob 一致；日志里的 path 才是
    // 相对同步根的完整位置。
    let expected = file_identity(name);
    let dehydrated = observation.is_fully_dehydrated();
    let mut fields = vec![
        ("path", relative.clone()),
        ("kind", "file".to_string()),
        ("placeholder", observation.is_placeholder().to_string()),
        (
            "placeholder_source",
            observation.placeholder_source().to_string(),
        ),
        (
            "placeholder_state",
            format!("0x{:08X}", observation.placeholder_state),
        ),
        ("attributes", format!("0x{:08X}", observation.attributes)),
        ("reparse_tag", format!("0x{:08X}", observation.reparse_tag)),
        ("logical_size", observation.logical_size.to_string()),
        (
            "on_disk_data_size",
            observation.on_disk_data_size().to_string(),
        ),
        ("dehydrated", dehydrated.to_string()),
        ("in_sync_state", observation.in_sync_state().to_string()),
        ("identity_length", observation.identity().len().to_string()),
    ];
    fields.extend(identity_fields(observation.identity(), &expected)?);
    record("inspect.entry", &fields, HResult::S_OK);

    require_placeholder(&relative, &observation)?;
    let expected_size_i64 = i64::try_from(expected_size).unwrap_or(i64::MAX);
    if observation.logical_size != expected_size_i64 {
        return Err(StageError::new(
            "inspect.entry",
            HResult::S_OK,
            format!(
                "{relative} 逻辑大小 {} 不等于设定值 {expected_size_i64}",
                observation.logical_size
            ),
        ));
    }
    if observation.in_sync_state() != 0 {
        return Err(StageError::new(
            "inspect.entry",
            HResult::S_OK,
            format!(
                "{relative} 报告 in_sync_state={}，但 S010 没有内容可校验，不应标记 in-sync",
                observation.in_sync_state()
            ),
        ));
    }
    if !dehydrated {
        return Err(StageError::new(
            "inspect.entry",
            HResult::S_OK,
            format!(
                "{relative} 不是脱水占位（placeholder_state=0x{:08X} on_disk_data_size={}）",
                observation.placeholder_state,
                observation.on_disk_data_size()
            ),
        ));
    }
    Ok(())
}

/// 占位符位必须置位，否则不是本工具创建的占位。
fn require_placeholder(relative: &str, observation: &EntryObservation) -> Result<(), StageError> {
    if observation.is_placeholder() {
        Ok(())
    } else {
        Err(StageError::new(
            "inspect.entry",
            HResult::S_OK,
            format!(
                "{relative} 不是占位符（placeholder_state=0x{:08X}）",
                observation.placeholder_state
            ),
        ))
    }
}

/// 解码系统保存的身份并生成日志字段；与原型预期不符则失败。
fn identity_fields(
    stored: &[u8],
    expected: &FileIdentity,
) -> Result<Vec<(&'static str, String)>, StageError> {
    let decoded = FileIdentity::decode(stored).map_err(|error| {
        StageError::new(
            "inspect.identity",
            HResult::S_OK,
            format!("FileIdentity 解码失败：{error}"),
        )
    })?;
    let fields = vec![
        ("identity_root_id", hex(decoded.root_id.as_bytes())),
        ("identity_entry_id", hex(decoded.entry_id.as_bytes())),
        (
            "identity_version_hint",
            decoded.version_hint.get().to_string(),
        ),
    ];
    if decoded != *expected {
        return Err(StageError::new(
            "inspect.identity",
            HResult::S_OK,
            format!(
                "FileIdentity 与预期不符：root={} entry={} hint={}",
                hex(decoded.root_id.as_bytes()),
                hex(decoded.entry_id.as_bytes()),
                decoded.version_hint.get()
            ),
        ));
    }
    Ok(fields)
}

/// `cleanup`：只删除本工具创建的占位符，保留用户内容，然后断开并注销。
pub fn cleanup(root: &Path) -> Result<(), StageError> {
    if !root.is_dir() {
        return Err(StageError::new(
            "cleanup.preflight",
            hresult_from_win32(ERROR_PATH_NOT_FOUND),
            format!("同步根目录不存在：{}", show(root)),
        ));
    }
    record(
        "cleanup.begin",
        &[
            ("path", show(root)),
            ("probe_directory", PROBE_DIRECTORY.to_string()),
            ("probe_files", probe_file_list()),
        ],
        HResult::S_OK,
    );
    // 连接一次，使删除动作发生在“提供方在线”的正常语义下；未注册时继续尝试清理残留。
    let connection = connect(root).ok();
    for (name, _) in PROBE_FILES {
        remove_probe_placeholder(root, &format!("{PROBE_DIRECTORY}/{name}"))?;
    }
    remove_probe_placeholder(root, PROBE_DIRECTORY)?;
    if let Some(key) = connection {
        disconnect(key)?;
    }
    unregister(root)?;
    report_preserved(root)
}

/// 只删除确认是占位符的探针条目；不是占位符就保留并在日志里说明。
fn remove_probe_placeholder(root: &Path, relative: &str) -> Result<(), StageError> {
    let path = root.join(relative);
    match try_observe(&path)? {
        None => {
            record(
                "cleanup.remove",
                &[
                    ("path", relative.to_string()),
                    ("status", "absent".to_string()),
                ],
                HResult::S_OK,
            );
            Ok(())
        }
        Some(observation) if observation.is_placeholder() => {
            let removed = if observation.directory {
                std::fs::remove_dir(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match removed {
                Ok(()) => {
                    record(
                        "cleanup.remove",
                        &[
                            ("path", relative.to_string()),
                            ("status", "removed".to_string()),
                            (
                                "placeholder_state",
                                format!("0x{:08X}", observation.placeholder_state),
                            ),
                            ("logical_size", observation.logical_size.to_string()),
                        ],
                        HResult::S_OK,
                    );
                    Ok(())
                }
                Err(error) => Err(StageError::new(
                    "cleanup.remove",
                    hresult_from_io(&error),
                    format!("删除占位 {} 失败：{error}", show(&path)),
                )),
            }
        }
        Some(observation) => {
            record(
                "cleanup.remove",
                &[
                    ("path", relative.to_string()),
                    ("status", "kept_not_placeholder".to_string()),
                    (
                        "placeholder_state",
                        format!("0x{:08X}", observation.placeholder_state),
                    ),
                ],
                HResult::S_OK,
            );
            Ok(())
        }
    }
}

/// 逐条列出同步根里剩下的内容：占位符只可能来自残留，其余是用户数据必须保留。
fn report_preserved(root: &Path) -> Result<(), StageError> {
    let entries = std::fs::read_dir(root).map_err(|error| {
        StageError::new(
            "cleanup.report",
            hresult_from_io(&error),
            format!("读取 {} 失败：{error}", show(root)),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            StageError::new(
                "cleanup.report",
                hresult_from_io(&error),
                format!("遍历 {} 失败：{error}", show(root)),
            )
        })?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map(show)
            .unwrap_or_else(|_| show(&path));
        match try_observe(&path)? {
            Some(observation) => record(
                "cleanup.preserved",
                &[
                    ("path", relative),
                    ("placeholder", observation.is_placeholder().to_string()),
                    ("directory", observation.directory.to_string()),
                    ("logical_size", observation.logical_size.to_string()),
                ],
                HResult::S_OK,
            ),
            None => record(
                "cleanup.preserved",
                &[("path", relative), ("status", "vanished".to_string())],
                HResult::S_OK,
            ),
        }
    }
    Ok(())
}

/// 原型固定条目的绝对路径（供测试脚本或诊断使用）。
pub fn probe_paths(root: &Path) -> Vec<PathBuf> {
    let directory = root.join(PROBE_DIRECTORY);
    let mut paths = vec![directory.clone()];
    for (name, _) in PROBE_FILES {
        paths.push(directory.join(name));
    }
    paths
}
