//! `cfapi.h`（Cloud Files API）与 `fileapi.h`/`handleapi.h` 的最小 FFI 声明（仅 Windows）。
//!
//! 只声明 S010 实际用到的部分，字段顺序与 Windows SDK 10.0.26100 的 `cfapi.h` 逐项对应；
//! 结构体注释里保留 C 侧类型名，便于对照。故意省略的字段（例如 `CF_CALLBACK_INFO` 在
//! `TransferKey` 之后的部分）在代码里从不读取，因此不声明。
//!
//! 链接：`cldapi.lib`（`Cf*`）与 `kernel32.lib`（`CreateFileW`/`FindFirstFileW`/`FindClose`/
//! `CloseHandle`/`GetLastError`）。宿主用 `cargo xwin build` 交叉编译，客体运行（见
//! `docs/operations/windows-lab.md` §12、§16）。

use std::ffi::c_void;

/// UTF-16 代码单元：`LPCWSTR`/`WCHAR`。
pub type WideChar = u16;

/// `HANDLE`：Win32 句柄值。
pub type Handle = *mut c_void;

// ---------------------------------------------------------------------------
// 通用类型与常量
// ---------------------------------------------------------------------------

/// `CF_PLATFORM_INFO`：三个平台版本号。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CfPlatformInfo {
    /// `DWORD BuildNumber`
    pub build_number: u32,
    /// `DWORD RevisionNumber`
    pub revision_number: u32,
    /// `DWORD IntegrationNumber`
    pub integration_number: u32,
}

/// `FILE_BASIC_INFO`（`winbase.h`）：占位符的时间戳与属性。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FileBasicInfo {
    /// `LARGE_INTEGER CreationTime`（FILETIME）
    pub creation_time: i64,
    /// `LARGE_INTEGER LastAccessTime`
    pub last_access_time: i64,
    /// `LARGE_INTEGER LastWriteTime`
    pub last_write_time: i64,
    /// `LARGE_INTEGER ChangeTime`
    pub change_time: i64,
    /// `DWORD FileAttributes`
    pub file_attributes: u32,
}

/// `CF_FS_METADATA`：创建占位符时写入的文件系统元数据。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CfFsMetadata {
    /// `FILE_BASIC_INFO BasicInfo`
    pub basic_info: FileBasicInfo,
    /// `LARGE_INTEGER FileSize`：逻辑大小（不是磁盘占用）。
    pub file_size: i64,
}

/// `CF_HYDRATION_POLICY`：两个 `USHORT` 枚举（`Primary` 与 `Modifier`）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CfHydrationPolicy {
    /// `CF_HYDRATION_POLICY_PRIMARY_USHORT Primary`
    pub primary: u16,
    /// `CF_HYDRATION_POLICY_MODIFIER_USHORT Modifier`
    pub modifier: u16,
}

/// `CF_POPULATION_POLICY`：两个 `USHORT` 枚举。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CfPopulationPolicy {
    /// `CF_POPULATION_POLICY_PRIMARY_USHORT Primary`
    pub primary: u16,
    /// `CF_POPULATION_POLICY_MODIFIER_USHORT Modifier`
    pub modifier: u16,
}

/// `CF_SYNC_POLICIES`：注册同步根时的策略。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CfSyncPolicies {
    /// `ULONG StructSize`
    pub struct_size: u32,
    /// `CF_HYDRATION_POLICY Hydration`
    pub hydration: CfHydrationPolicy,
    /// `CF_POPULATION_POLICY Population`
    pub population: CfPopulationPolicy,
    /// `CF_INSYNC_POLICY InSync`
    pub in_sync: u32,
    /// `CF_HARDLINK_POLICY HardLink`
    pub hard_link: u32,
    /// `CF_PLACEHOLDER_MANAGEMENT_POLICY PlaceholderManagement`
    pub placeholder_management: u32,
}

/// `GUID`。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Guid {
    /// `ULONG Data1`
    pub data1: u32,
    /// `USHORT Data2`
    pub data2: u16,
    /// `USHORT Data3`
    pub data3: u16,
    /// `UCHAR Data4[8]`
    pub data4: [u8; 8],
}

/// `CF_SYNC_REGISTRATION`：提供方标识与根身份。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CfSyncRegistration {
    /// `ULONG StructSize`
    pub struct_size: u32,
    /// `LPCWSTR ProviderName`
    pub provider_name: *const WideChar,
    /// `LPCWSTR ProviderVersion`
    pub provider_version: *const WideChar,
    /// `LPCVOID SyncRootIdentity`
    pub sync_root_identity: *const c_void,
    /// `DWORD SyncRootIdentityLength`
    pub sync_root_identity_length: u32,
    /// `LPCVOID FileIdentity`：根占位符的默认身份（本原型不用，置空）。
    pub file_identity: *const c_void,
    /// `DWORD FileIdentityLength`
    pub file_identity_length: u32,
    /// `GUID ProviderId`
    pub provider_id: Guid,
}

/// `CF_CONNECTION_KEY`（`DECLARE_OPAQUE_KEY`）：不透明的连接键。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CfConnectionKey {
    /// `LONGLONG Internal`
    pub internal: i64,
}

/// `CF_CALLBACK`：CFAPI 回调函数类型。
///
/// C 侧的第二个参数是 `const CF_CALLBACK_PARAMETERS *`；本原型只读取 `FetchData` 分支前缀，
/// 因此把指针类型声明为 [`CfCallbackParametersFetchData`]（ABI 上同为指针，其他回调类型不会
/// 读取该参数的内容）。
pub type CfCallback =
    Option<unsafe extern "system" fn(*const CfCallbackInfo, *const CfCallbackParametersFetchData)>;

/// `CF_CALLBACK_REGISTRATION`：回调表的一项；表以 `CF_CALLBACK_REGISTRATION_END` 结束。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CfCallbackRegistration {
    /// `CF_CALLBACK_TYPE Type`
    pub callback_type: u32,
    /// `CF_CALLBACK Callback`
    pub callback: CfCallback,
}

/// `CF_CALLBACK_INFO` 的读取前缀（`TransferKey` 之后的字段本原型不读）。
#[repr(C)]
pub struct CfCallbackInfo {
    /// `DWORD StructSize`
    pub struct_size: u32,
    /// `CF_CONNECTION_KEY ConnectionKey`
    pub connection_key: CfConnectionKey,
    /// `LPVOID CallbackContext`
    pub callback_context: *mut c_void,
    /// `PCWSTR VolumeGuidName`
    pub volume_guid_name: *const WideChar,
    /// `PCWSTR VolumeDosName`
    pub volume_dos_name: *const WideChar,
    /// `DWORD VolumeSerialNumber`
    pub volume_serial_number: u32,
    /// `LARGE_INTEGER SyncRootFileId`
    pub sync_root_file_id: i64,
    /// `LPCVOID SyncRootIdentity`
    pub sync_root_identity: *const c_void,
    /// `DWORD SyncRootIdentityLength`
    pub sync_root_identity_length: u32,
    /// `LARGE_INTEGER FileId`
    pub file_id: i64,
    /// `LARGE_INTEGER FileSize`
    pub file_size: i64,
    /// `LPCVOID FileIdentity`
    pub file_identity: *const c_void,
    /// `DWORD FileIdentityLength`
    pub file_identity_length: u32,
    /// `PCWSTR NormalizedPath`：只有连接时请求 `CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH` 才填充。
    pub normalized_path: *const WideChar,
    /// `CF_TRANSFER_KEY TransferKey`
    pub transfer_key: i64,
    /// `UCHAR PriorityHint`
    pub priority_hint: u8,
    /// `PCORRELATION_VECTOR CorrelationVector`
    pub correlation_vector: *const c_void,
    /// `CF_PROCESS_INFO* ProcessInfo`
    pub process_info: *const c_void,
    /// `CF_REQUEST_KEY RequestKey`：回执时必须原样带回。
    pub request_key: i64,
}

/// `CF_CALLBACK_PARAMETERS` 的 `FetchData` 分支前缀。
///
/// C 侧是 `ULONG ParamSize` + 联合体（联合体从偏移 8 开始）；`FetchData` 成员依次是
/// `Flags`/`RequiredFileOffset`/`RequiredLength`/`OptionalFileOffset`/`OptionalLength`/
/// `LastDehydrationTime`/`LastDehydrationReason`。这里只声明到比例，后续字段本原型不读。
#[repr(C)]
pub struct CfCallbackParametersFetchData {
    /// `ULONG ParamSize`
    pub param_size: u32,
    /// 联合体对齐填充。
    pub padding: u32,
    /// `CF_CALLBACK_FETCH_DATA_FLAGS Flags`
    pub flags: u32,
    /// 结构体对齐填充。
    pub padding2: u32,
    /// `LARGE_INTEGER RequiredFileOffset`
    pub required_file_offset: i64,
    /// `LARGE_INTEGER RequiredLength`
    pub required_length: i64,
}

/// `CF_OPERATION_INFO`：`CfExecute` 的操作描述。
#[repr(C)]
pub struct CfOperationInfo {
    /// `ULONG StructSize`
    pub struct_size: u32,
    /// `CF_OPERATION_TYPE Type`
    pub operation_type: u32,
    /// `CF_CONNECTION_KEY ConnectionKey`
    pub connection_key: CfConnectionKey,
    /// `CF_TRANSFER_KEY TransferKey`
    pub transfer_key: i64,
    /// `CONST CORRELATION_VECTOR* CorrelationVector`
    pub correlation_vector: *const c_void,
    /// `CONST CF_SYNC_STATUS* SyncStatus`
    pub sync_status: *const c_void,
    /// `CF_REQUEST_KEY RequestKey`
    pub request_key: i64,
}

/// `CF_OPERATION_PARAMETERS` 的 `TransferData` 分支布局（其他分支本原型不用）。
///
/// 与 C 结构体一致：`ULONG ParamSize` 之后接联合体，`TransferData` 分支为
/// `Flags`/`CompletionStatus`/`Buffer`/`Offset`/`Length`。
#[repr(C)]
pub struct CfOperationParametersTransferData {
    /// `ULONG ParamSize`
    pub param_size: u32,
    /// 对齐联合体的填充（C 侧联合体按 8 字节对齐）。
    pub padding: u32,
    /// `CF_OPERATION_TRANSFER_DATA_FLAGS Flags`
    pub flags: u32,
    /// `NTSTATUS CompletionStatus`
    pub completion_status: i32,
    /// `LPCVOID Buffer`
    pub buffer: *const c_void,
    /// `LARGE_INTEGER Offset`
    pub offset: i64,
    /// `LARGE_INTEGER Length`
    pub length: i64,
}

/// `sizeof(CF_OPERATION_PARAMETERS)`。
///
/// `CfExecute` 要求 `CF_OPERATION_PARAMETERS::ParamSize` 等于整个结构的大小（联合体最大成员
/// `RetrieveData` 为 40 字节，加上开头的 `ULONG ParamSize` 共 48），而不是本结构实际用到的
/// `TransferData` 分支大小。传 40 会被拒绝为 `ERROR_CLOUD_FILE_INVALID_REQUEST`（S010 实测）。
pub const CF_OPERATION_PARAMETERS_SIZE: u32 = 48;

const _: () = assert!(
    std::mem::size_of::<CfOperationParametersTransferData>()
        <= CF_OPERATION_PARAMETERS_SIZE as usize
);

/// `CF_PLACEHOLDER_CREATE_INFO`：一次 `CfCreatePlaceholders` 的条目。
#[repr(C)]
pub struct CfPlaceholderCreateInfo {
    /// `LPCWSTR RelativeFileName`
    pub relative_file_name: *const WideChar,
    /// `CF_FS_METADATA FsMetadata`
    pub fs_metadata: CfFsMetadata,
    /// `LPCVOID FileIdentity`
    pub file_identity: *const c_void,
    /// `DWORD FileIdentityLength`
    pub file_identity_length: u32,
    /// `CF_PLACEHOLDER_CREATE_FLAGS Flags`
    pub flags: u32,
    /// `HRESULT Result`：逐条目返回值，必须检查。
    pub result: i32,
    /// `USN CreateUsn`
    pub create_usn: i64,
}

/// `CF_PLACEHOLDER_STANDARD_INFO` 的定长前缀；`FileIdentity[1]` 变长尾部另存。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CfPlaceholderStandardInfo {
    /// `LARGE_INTEGER OnDiskDataSize`：本地实际数据字节数，脱水占位符为 0。
    pub on_disk_data_size: i64,
    /// `LARGE_INTEGER ValidatedDataSize`
    pub validated_data_size: i64,
    /// `LARGE_INTEGER ModifiedDataSize`
    pub modified_data_size: i64,
    /// `LARGE_INTEGER PropertiesSize`
    pub properties_size: i64,
    /// `CF_PIN_STATE PinState`
    pub pin_state: u32,
    /// `CF_IN_SYNC_STATE InSyncState`
    pub in_sync_state: u32,
    /// `LARGE_INTEGER FileId`
    pub file_id: i64,
    /// `LARGE_INTEGER SyncRootFileId`
    pub sync_root_file_id: i64,
    /// `ULONG FileIdentityLength`
    pub file_identity_length: u32,
}

/// `CF_SYNC_ROOT_PROVIDER_INFO`：注册的提供方名称、版本与状态。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CfSyncRootProviderInfo {
    /// `CF_SYNC_PROVIDER_STATUS ProviderStatus`
    pub provider_status: u32,
    /// `WCHAR ProviderName[CF_MAX_PROVIDER_NAME_LENGTH + 1]`
    pub provider_name: [u16; CF_MAX_PROVIDER_TEXT],
    /// `WCHAR ProviderVersion[CF_MAX_PROVIDER_VERSION_LENGTH + 1]`
    pub provider_version: [u16; CF_MAX_PROVIDER_TEXT],
}

/// `WIN32_FIND_DATAW`（`fileapi.h`）：`FindFirstFileW` 返回的条目信息。
///
/// `dwReserved0`（本文件中的 `reserved0`）就是 reparse tag；`CfGetPlaceholderStateFromFindData`
/// 正是按这份数据判定占位状态的，因此目录与文件都能拿到标签。
#[repr(C)]
pub struct Win32FindData {
    /// `DWORD dwFileAttributes`
    pub file_attributes: u32,
    /// `FILETIME ftCreationTime` 低 32 位
    pub creation_time_low: u32,
    /// `FILETIME ftCreationTime` 高 32 位
    pub creation_time_high: u32,
    /// `FILETIME ftLastAccessTime` 低 32 位
    pub last_access_time_low: u32,
    /// `FILETIME ftLastAccessTime` 高 32 位
    pub last_access_time_high: u32,
    /// `FILETIME ftLastWriteTime` 低 32 位
    pub last_write_time_low: u32,
    /// `FILETIME ftLastWriteTime` 高 32 位
    pub last_write_time_high: u32,
    /// `DWORD nFileSizeHigh`
    pub file_size_high: u32,
    /// `DWORD nFileSizeLow`
    pub file_size_low: u32,
    /// `DWORD dwReserved0`：reparse tag
    pub reserved0: u32,
    /// `DWORD dwReserved1`
    pub reserved1: u32,
    /// `WCHAR cFileName[MAX_PATH]`
    pub file_name: [u16; 260],
    /// `WCHAR cAlternateFileName[14]`
    pub alternate_file_name: [u16; 14],
}

impl Default for Win32FindData {
    fn default() -> Self {
        Self {
            file_attributes: 0,
            creation_time_low: 0,
            creation_time_high: 0,
            last_access_time_low: 0,
            last_access_time_high: 0,
            last_write_time_low: 0,
            last_write_time_high: 0,
            file_size_high: 0,
            file_size_low: 0,
            reserved0: 0,
            reserved1: 0,
            file_name: [0; 260],
            alternate_file_name: [0; 14],
        }
    }
}

impl Win32FindData {
    /// 逻辑大小：`nFileSizeHigh`/`nFileSizeLow` 合成的 64 位值。
    pub const fn logical_size(&self) -> i64 {
        ((self.file_size_high as i64) << 32) | (self.file_size_low as i64)
    }
}

// ---------------------------------------------------------------------------
// 常量（取值与 Windows SDK 一致）
// ---------------------------------------------------------------------------

/// `CF_CALLBACK_TYPE_FETCH_DATA`
pub const CF_CALLBACK_TYPE_FETCH_DATA: u32 = 0;
/// `CF_CALLBACK_TYPE_CANCEL_FETCH_DATA`
pub const CF_CALLBACK_TYPE_CANCEL_FETCH_DATA: u32 = 2;
/// `CF_CALLBACK_TYPE_NONE`：回调表结束标记。
pub const CF_CALLBACK_TYPE_NONE: u32 = 0xFFFF_FFFF;

/// `CF_OPERATION_TYPE_TRANSFER_DATA`
pub const CF_OPERATION_TYPE_TRANSFER_DATA: u32 = 0;
/// `CF_HYDRATE_FLAG_NONE`
pub const CF_HYDRATE_FLAG_NONE: u32 = 0;
/// `CF_DEHYDRATE_FLAG_NONE`
pub const CF_DEHYDRATE_FLAG_NONE: u32 = 0;
/// `CF_OPERATION_TRANSFER_DATA_FLAG_NONE`
pub const CF_OPERATION_TRANSFER_DATA_FLAG_NONE: u32 = 0x0000_0000;

/// `CF_HYDRATION_POLICY_PARTIAL`（0）：只水合被请求的范围，未请求部分保持脱水。
///
/// S011 用它在同进程内观察“传输中的取消”：`FULL`（2）下平台会在打开时请求整份内容，
/// 打开返回时水合已结束，回调里没有可取消的传输。
pub const CF_HYDRATION_POLICY_PARTIAL: u16 = 0;
/// `CF_HYDRATION_POLICY_FULL`
pub const CF_HYDRATION_POLICY_FULL: u16 = 2;
/// `CF_POPULATION_POLICY_PARTIAL`（0）：不完整目录的访问可能要求提供方补齐名字。
///
/// S011 实测：登记 `FULL`（2）时，**别的进程**按路径打开占位符会先要求目录填充
/// （`FETCH_PLACEHOLDERS` 回调），而本原型不实现该回调，于是打开无限等待；`PARTIAL`
/// 下按路径的文件访问不依赖目录枚举。
pub const CF_POPULATION_POLICY_PARTIAL: u16 = 0;
/// `CF_POPULATION_POLICY_FULL`
pub const CF_POPULATION_POLICY_FULL: u16 = 2;
/// `CF_POPULATION_POLICY_ALWAYS_FULL`（3）：应用维护完整名字空间，不请求 FETCH_PLACEHOLDERS。
pub const CF_POPULATION_POLICY_ALWAYS_FULL: u16 = 3;
/// `CF_INSYNC_POLICY_NONE`：不让系统自动把条目当 in-sync。
pub const CF_INSYNC_POLICY_NONE: u32 = 0;
/// `CF_HARDLINK_POLICY_NONE`
pub const CF_HARDLINK_POLICY_NONE: u32 = 0;
/// `CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT`
pub const CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT: u32 = 0;

/// `CF_REGISTER_FLAG_NONE`
pub const CF_REGISTER_FLAG_NONE: u32 = 0;
/// `CF_CONNECT_FLAG_NONE`
pub const CF_CONNECT_FLAG_NONE: u32 = 0;
/// `CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH`：让 `CF_CALLBACK_INFO::NormalizedPath` 带上完整路径。
pub const CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH: u32 = 0x0000_0004;
/// `CF_CREATE_FLAG_NONE`
pub const CF_CREATE_FLAG_NONE: u32 = 0;
/// `CF_PLACEHOLDER_CREATE_FLAG_NONE`
pub const CF_PLACEHOLDER_CREATE_FLAG_NONE: u32 = 0;

/// `CF_PLACEHOLDER_STATE_PLACEHOLDER`
pub const CF_PLACEHOLDER_STATE_PLACEHOLDER: u32 = 0x0000_0001;
/// `CF_PLACEHOLDER_STATE_SYNC_ROOT`
pub const CF_PLACEHOLDER_STATE_SYNC_ROOT: u32 = 0x0000_0002;
/// `CF_PLACEHOLDER_STATE_IN_SYNC`
pub const CF_PLACEHOLDER_STATE_IN_SYNC: u32 = 0x0000_0008;
/// `CF_PLACEHOLDER_STATE_PARTIAL`
pub const CF_PLACEHOLDER_STATE_PARTIAL: u32 = 0x0000_0010;
/// `CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK`
pub const CF_PLACEHOLDER_STATE_PARTIALLY_ON_DISK: u32 = 0x0000_0020;
/// `CF_PLACEHOLDER_STATE_INVALID`
pub const CF_PLACEHOLDER_STATE_INVALID: u32 = 0xFFFF_FFFF;

/// `CF_PLACEHOLDER_INFO_STANDARD`
pub const CF_PLACEHOLDER_INFO_STANDARD: u32 = 1;
/// `CF_SYNC_ROOT_INFO_PROVIDER`
pub const CF_SYNC_ROOT_INFO_PROVIDER: u32 = 2;

/// `CF_IN_SYNC_STATE_IN_SYNC`
pub const CF_IN_SYNC_STATE_IN_SYNC: u32 = 1;
/// `CF_IN_SYNC_STATE_NOT_IN_SYNC`
pub const CF_IN_SYNC_STATE_NOT_IN_SYNC: u32 = 0;
/// `CF_SET_IN_SYNC_FLAG_NONE`
pub const CF_SET_IN_SYNC_FLAG_NONE: u32 = 0;

/// `FILE_ATTRIBUTE_DIRECTORY`
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// `FILE_ATTRIBUTE_ARCHIVE`
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;

/// `CF_MAX_PROVIDER_NAME_LENGTH`/`CF_MAX_PROVIDER_VERSION_LENGTH` 的上限（含结尾 NUL）。
pub const CF_MAX_PROVIDER_TEXT: usize = 256;

const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
/// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`
pub const FILE_SHARE_ALL: u32 = 0x0000_0007;
/// `GENERIC_READ`
pub const GENERIC_READ: u32 = 0x8000_0000;
/// `GENERIC_WRITE`
pub const GENERIC_WRITE: u32 = 0x4000_0000;
/// `ERROR_ACCESS_DENIED`（`winerror.h`）：权限或共享/属性冲突都可能返回它。
pub const ERROR_ACCESS_DENIED: u32 = 5;
/// `ERROR_DIR_NOT_EMPTY`（`winerror.h`）：目录里还有条目。
pub const ERROR_DIR_NOT_EMPTY: u32 = 145;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// `THREAD_TERMINATE`：`CancelSynchronousIo` 要求线程句柄具备的访问权限。
pub const THREAD_TERMINATE: u32 = 0x0000_0001;

/// `INVALID_HANDLE_VALUE`
pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

/// `STATUS_NOT_SUPPORTED`：S010 未实现水合时 `FETCH_DATA` 用它结束请求；S011 起不再使用。
pub const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BBu32 as i32;
/// `STATUS_SUCCESS`：`CfExecute(TRANSFER_DATA)` 回填一段数据时的 `CompletionStatus`。
pub const STATUS_SUCCESS: i32 = 0;

// 以下 NTSTATUS 取自 Windows SDK 10.0.26100 `shared/ntstatus.h`（S011 核对，`0xC000CFxx`
// 是 Cloud Files 的专用区间）：
/// `STATUS_CLOUD_FILE_INVALID_REQUEST`（0xC000CF0B）：非原型条目、范围非法、长度与逻辑大小不符。
pub const STATUS_CLOUD_FILE_INVALID_REQUEST: i32 = 0xC000_CF0Bu32 as i32;
/// `STATUS_CLOUD_FILE_INSUFFICIENT_RESOURCES`（0xC000CF10）：单段分配失败。
pub const STATUS_CLOUD_FILE_INSUFFICIENT_RESOURCES: i32 = 0xC000_CF10u32 as i32;
/// `STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE`（0xC000CF11）：内容源不可用（离线）。
pub const STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE: i32 = 0xC000_CF11u32 as i32;
/// `STATUS_IO_TIMEOUT`（0xC00000B5）：内容源延迟超过阈值。
pub const STATUS_IO_TIMEOUT: i32 = 0xC000_00B5u32 as i32;

/// `ERROR_ALREADY_EXISTS` 的 `HRESULT_FROM_WIN32`（同步根已注册/占位已存在时出现）。
pub const HRESULT_FROM_WIN32_ERROR_ALREADY_EXISTS: i32 = 0x8007_00B7u32 as i32;
/// `ERROR_NOT_A_CLOUD_FILE`（376）的 `HRESULT_FROM_WIN32`：`CfGetPlaceholderInfo` 对普通文件返回它。
pub const HRESULT_FROM_WIN32_ERROR_NOT_A_CLOUD_FILE: i32 = 0x8007_0178u32 as i32;
/// `ERROR_FILE_NOT_FOUND`
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
/// `ERROR_PATH_NOT_FOUND`
pub const ERROR_PATH_NOT_FOUND: u32 = 3;

// ---------------------------------------------------------------------------
// 导入函数
// ---------------------------------------------------------------------------

#[link(name = "cldapi")]
unsafe extern "system" {
    /// `CfGetPlatformInfo(_Out_ CF_PLATFORM_INFO *)`
    pub fn CfGetPlatformInfo(platform_version: *mut CfPlatformInfo) -> i32;
    /// `CfRegisterSyncRoot(LPCWSTR, const CF_SYNC_REGISTRATION *, const CF_SYNC_POLICIES *, CF_REGISTER_FLAGS)`
    pub fn CfRegisterSyncRoot(
        sync_root_path: *const WideChar,
        registration: *const CfSyncRegistration,
        policies: *const CfSyncPolicies,
        register_flags: u32,
    ) -> i32;
    /// `CfUnregisterSyncRoot(LPCWSTR)`
    pub fn CfUnregisterSyncRoot(sync_root_path: *const WideChar) -> i32;
    /// `CfConnectSyncRoot(LPCWSTR, const CF_CALLBACK_REGISTRATION *, LPCVOID, CF_CONNECT_FLAGS, CF_CONNECTION_KEY *)`
    pub fn CfConnectSyncRoot(
        sync_root_path: *const WideChar,
        callback_table: *const CfCallbackRegistration,
        callback_context: *mut c_void,
        connect_flags: u32,
        connection_key: *mut CfConnectionKey,
    ) -> i32;
    /// `CfDisconnectSyncRoot(CF_CONNECTION_KEY)`
    pub fn CfDisconnectSyncRoot(connection_key: CfConnectionKey) -> i32;
    /// `CfCreatePlaceholders(LPCWSTR, CF_PLACEHOLDER_CREATE_INFO *, DWORD, CF_CREATE_FLAGS, PDWORD)`
    pub fn CfCreatePlaceholders(
        base_directory_path: *const WideChar,
        placeholder_array: *mut CfPlaceholderCreateInfo,
        placeholder_count: u32,
        create_flags: u32,
        entries_processed: *mut u32,
    ) -> i32;
    /// `CfGetPlaceholderStateFromFindData(CONST WIN32_FIND_DATA *)`
    pub fn CfGetPlaceholderStateFromFindData(find_data: *const Win32FindData) -> u32;
    /// `CfGetPlaceholderInfo(HANDLE, CF_PLACEHOLDER_INFO_CLASS, PVOID, DWORD, PDWORD)`
    pub fn CfGetPlaceholderInfo(
        file_handle: Handle,
        info_class: u32,
        info_buffer: *mut c_void,
        info_buffer_length: u32,
        returned_length: *mut u32,
    ) -> i32;
    /// `CfGetSyncRootInfoByPath(LPCWSTR, CF_SYNC_ROOT_INFO_CLASS, PVOID, DWORD, PDWORD)`
    pub fn CfGetSyncRootInfoByPath(
        file_path: *const WideChar,
        info_class: u32,
        info_buffer: *mut c_void,
        info_buffer_length: u32,
        returned_length: *mut u32,
    ) -> i32;
    /// `CfHydratePlaceholder(HANDLE, LARGE_INTEGER, LARGE_INTEGER, CF_HYDRATE_FLAGS, CF_OPERATION_PARAMETERS *)`
    pub fn CfHydratePlaceholder(
        file_handle: Handle,
        starting_offset: i64,
        length: i64,
        hydrate_flags: u32,
        hydrate_params: *mut c_void,
    ) -> i32;
    /// `CfDehydratePlaceholder(HANDLE, LARGE_INTEGER, LARGE_INTEGER, CF_DEHYDRATE_FLAGS, CF_OPERATION_PARAMETERS *)`
    ///
    /// 第 5 个参数在文档里是“保留，必须为 NULL”（与 `CfHydratePlaceholder` 相同，S011 已按
    /// NULL 实测可用）。
    pub fn CfDehydratePlaceholder(
        file_handle: Handle,
        starting_offset: i64,
        length: i64,
        dehydrate_flags: u32,
        dehydrate_params: *mut c_void,
    ) -> i32;
    /// `CfSetInSyncState(HANDLE, CF_IN_SYNC_STATE, CF_SET_IN_SYNC_FLAGS, USN *)`
    ///
    /// `USN` 是 `LONGLONG`；提供方据此声明“本地内容等于已确认版本”，这也是
    /// `CfDehydratePlaceholder` 的前置条件（S012 实测，见 `agent/tasks/S012.md`）。
    pub fn CfSetInSyncState(
        file_handle: Handle,
        in_sync_state: u32,
        set_in_sync_flags: u32,
        in_sync_usn: *mut i64,
    ) -> i32;
    /// `CfExecute(const CF_OPERATION_INFO *, CF_OPERATION_PARAMETERS *)`
    pub fn CfExecute(op_info: *const CfOperationInfo, op_params: *mut c_void) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    /// `CreateFileW(...)`：本原型只用于“只读属性、不跟随 reparse point”地打开条目。
    pub fn CreateFileW(
        file_name: *const WideChar,
        desired_access: u32,
        share_mode: u32,
        security_attributes: *const c_void,
        creation_disposition: u32,
        flags_and_attributes: u32,
        template_file: Handle,
    ) -> Handle;
    /// `CloseHandle(HANDLE)`
    pub fn CloseHandle(object: Handle) -> i32;
    /// `FindFirstFileW(LPCWSTR, LPWIN32_FIND_DATAW)`：查询单个条目的属性、大小与 reparse tag。
    pub fn FindFirstFileW(file_name: *const WideChar, find_file_data: *mut Win32FindData)
    -> Handle;
    /// `FindClose(HANDLE)`
    pub fn FindClose(find_file: Handle) -> i32;
    /// `GetLastError()`：紧跟失败的 Win32 调用读取，用于区分“不存在”与“拒绝访问”。
    pub fn GetLastError() -> u32;
    /// `CancelIoEx(HANDLE hFile, LPOVERLAPPED lpOverlapped)`：取消该文件句柄上所有线程发出的
    /// 挂起 I/O（`lpOverlapped` 为 NULL 表示全部）。原型用它触发水合请求的取消。
    pub fn CancelIoEx(file: Handle, overlapped: *const c_void) -> i32;
    /// `CancelSynchronousIo(HANDLE hThread)`：取消指定线程上挂起的同步 I/O。
    ///
    /// 原型用它验证“读取被取消 ⇒ `CANCEL_FETCH_DATA` 语义 ⇒ 传输中途退出”。
    pub fn CancelSynchronousIo(thread_handle: Handle) -> i32;
    /// `OpenThread(DWORD dwDesiredAccess, BOOL bInheritHandle, DWORD dwThreadId)`：
    /// `CancelSynchronousIo` 需要一个线程句柄，而 `std::thread::Thread` 不提供线程 id/句柄。
    pub fn OpenThread(desired_access: u32, inherit_handle: i32, thread_id: u32) -> Handle;
    /// `GetCurrentThreadId()`：读线程用它把自己的系统线程 id 公布给取消方。
    pub fn GetCurrentThreadId() -> u32;
}

/// 以“只读属性”方式打开条目：只请求 `FILE_READ_ATTRIBUTES`，因此不会触发水合回调；
/// `FILE_FLAG_BACKUP_SEMANTICS` 允许打开目录。返回的句柄必须交给 [`close_handle`]。
///
/// 占位状态与 reparse tag 不走这个句柄：S010 实测目录占位符经句柄读
/// `GetFileInformationByHandleEx(FileAttributeTagInfo)` 得到 `ReparseTag=0`，而
/// `FindFirstFileW` 的 `dwReserved0` 给出 `0x9000001A`，因此状态判定统一用
/// `CfGetPlaceholderStateFromFindData`，句柄只用于 `CfGetPlaceholderInfo`。
///
/// # Safety
///
/// `path` 必须是以 NUL 结尾的有效 UTF-16 缓冲区，且在调用期间保持有效。
pub unsafe fn open_attributes(path: &[WideChar]) -> Handle {
    unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    }
}

/// 以指定访问权限与共享模式打开现有文件，用于**使用保护探测**；返回的句柄必须交给
/// [`close_handle`]。
///
/// 故意不加 `FILE_FLAG_OPEN_REPARSE_POINT`：探测要在与普通应用相同的路径语义下做共享检查
/// （绕过重解析点会看不到真实的文件对象共享状态）。
///
/// # Safety
///
/// `path` 必须是以 NUL 结尾的有效 UTF-16 缓冲区，且在调用期间保持有效。
pub unsafe fn open_with_share(path: &[WideChar], desired_access: u32, share_mode: u32) -> Handle {
    unsafe {
        CreateFileW(
            path.as_ptr(),
            desired_access,
            share_mode,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    }
}

/// 关闭 [`open_attributes`] 返回的句柄；`INVALID_HANDLE_VALUE` 视为无需关闭。
///
/// # Safety
///
/// `handle` 必须是尚未关闭的 Win32 句柄（或 `INVALID_HANDLE_VALUE`/空指针）。
pub unsafe fn close_handle(handle: Handle) {
    if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
        unsafe {
            CloseHandle(handle);
        }
    }
}

// 布局断言：数值由 Windows SDK 10.0.26100 的 cfapi.h 用 clang-cl `_Static_assert` 实测得出
// （见 agent/tasks/S010.md）。结构体一旦偏离 C 布局，FFI 立刻在这里编译失败，而不是在客体上
// 出现难解释的 HRESULT。
const _: () = {
    assert!(std::mem::size_of::<CfPlatformInfo>() == 12);
    assert!(std::mem::size_of::<CfSyncRegistration>() == 72);
    assert!(std::mem::size_of::<CfSyncPolicies>() == 24);
    assert!(std::mem::size_of::<Guid>() == 16);
    assert!(std::mem::size_of::<CfCallbackRegistration>() == 16);
    assert!(std::mem::size_of::<CfOperationInfo>() == 48);
    assert!(std::mem::size_of::<CfPlaceholderCreateInfo>() == 88);
    assert!(std::mem::size_of::<CfPlaceholderStandardInfo>() == 64);
    assert!(std::mem::size_of::<CfSyncRootProviderInfo>() == 1028);
    assert!(std::mem::size_of::<CfCallbackInfo>() == 152);
    assert!(std::mem::size_of::<CfFsMetadata>() == 48);
    assert!(std::mem::size_of::<FileBasicInfo>() == 40);
    assert!(std::mem::size_of::<Win32FindData>() == 592);
    assert!(std::mem::size_of::<CfCallbackParametersFetchData>() <= 8 + 64);
};
