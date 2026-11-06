//! 本文件由 `cargo xtask generate` 从 `api/local-ipc.schema.json` 生成，禁止手改。
//!
//! 契约：vaultsyncd 本机 IPC（api/local-ipc.schema.json）；生成器：vaultsyncd-xtask 0.2.0
//! Windows agent 与本机 GUI/CLI 之间的最小命令契约（docs/design.md §15.5、§5.6）。
//! 本文件是本机 IPC 的唯一编辑源；`cargo xtask generate` 据此生成
//! crates/local-ipc 的 Rust DTO 与 packages/local-ipc-types 的 TypeScript 类型。
//! 本契约与服务器管理 API 分开：它不经网络传输，也不复用 api/openapi.yaml。
//! 消息按帧编码：FRAME_HEADER_BYTES 字节的大端长度头（不含头自身）加该长度的
//! UTF-8 JSON 负载，单帧负载上限为 MAX_FRAME_BYTES；两个常量由 x-vaultsyncd 生成。
//! 请求方按 Request 编码，agent 按 Response 应答；request_id 原样回显，
//! 调用方据此把并发应答配对，agent 不解析它的内容。
//! 未知命令、未知闭集取值与畸形 JSON 一律拒绝，不当作默认成功：
//! 解码失败发生在产生应答之前，由传输层结束该连接。
//!
//! 生成代码只做 wire 形态：闭集枚举的未知取值在反序列化时报错，可选字段缺省时不出现在
//! JSON 里；帧的读写与命令处理在 `crate::framing`/`crate::service`，传输层（命名管道、
//! socket）不在本 crate。

/// agent 支持的 IPC 协议 major 版本（major 不同即不兼容）。
pub const PROTOCOL_MAJOR: u32 = 1;
/// agent 支持的 IPC 协议 minor 版本（高于此值的调用方被拒绝）。
pub const PROTOCOL_MINOR: u32 = 4;
/// 帧长度头的字节数：大端无符号整数，表示后续 JSON 负载的字节数。
pub const FRAME_HEADER_BYTES: usize = 4;
/// 单帧负载上限（字节）：超过即在读帧阶段拒绝，不为坏帧扩容。
pub const MAX_FRAME_BYTES: usize = 1048576;

/// 持久 Binding 配置与控制事实，包括解绑记录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct BindingInfo {
    /// Binding 身份。
    pub binding_id: String,
    /// 保留的缓存。
    pub cache_path: String,
    /// 保留数据停止管理。
    pub detached: bool,
    /// 用户选择的目录。
    pub local_path: String,
    /// 持久目标模式。
    pub mode: String,
    /// 已暂停。
    pub paused: bool,
    /// Root 身份。
    pub root_id: String,
}

/// 运行中 agent 从核心持久状态读取的单 Binding 多维快照。计数为十进制文本，不含本机路径或凭据；synchronized 只能由核心完整判定，不能仅按 pending 判定。每个 Binding 的字段来自同一读事务，不承诺多 Binding 原子性；1.2 的 subscribe_status 按实例版本长轮询推送全量，get_status 可无条件恢复丢失提示。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct BindingStatus {
    /// Binding 稳定身份。
    pub binding_id: String,
    /// Binding 已解除。
    pub detached: bool,
    /// 未确认本地内容数量（十进制）。
    pub dirty: String,
    /// 接收与应用游标间隔（十进制）。
    pub event_lag: String,
    /// 失败队列数量（十进制）。
    pub failed: String,
    /// 开放本地意图数量（十进制）。
    pub intents: String,
    /// 完整驻留数量（十进制）。
    pub materialized: String,
    /// 仅元数据占位数量（十进制）。
    pub metadata_only: String,
    /// 模式补齐尚未完成。
    pub mode_pending: bool,
    /// 断网任务数量（十进制）。
    pub offline: String,
    /// 部分驻留数量（十进制）。
    pub partial: String,
    /// Binding 已暂停。
    pub paused: bool,
    /// 尚未完成队列数量（十进制），不等于 committed。
    pub pending: String,
    /// 未完成恢复数量（十进制）。
    pub recovery: String,
    /// 核心根据全部维度判定的完成事实；暂停单独展示。
    pub synchronized: bool,
    /// 未完成传输数量（十进制）。
    pub transfers: String,
    /// 未核对内容或需重新协调数量（十进制）。
    pub unknown: String,
}

/// 命令负载。parameters 按命令逐项校验必填项与允许项；多余参数、非规范身份、相对路径、越界文本一律失败。1.0/1.1 仍支持无参数 get_status。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// 要执行的命令名；未知取值在解码阶段被拒绝。
    pub name: CommandName,
    /// 命令专属参数；执行器复核，不能仅信任界面。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<CommandParameters>,
}

/// 本机管理命令闭集；新增命令自协议 1.2 起提供，旧版本只支持 get_status。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum CommandName {
    /// 取值 `get_status`。
    #[serde(rename = "get_status")]
    GetStatus,
    /// 取值 `subscribe_status`。
    #[serde(rename = "subscribe_status")]
    SubscribeStatus,
    /// 取值 `list_bindings`。
    #[serde(rename = "list_bindings")]
    ListBindings,
    /// 取值 `create_binding`。
    #[serde(rename = "create_binding")]
    CreateBinding,
    /// 取值 `pause_binding`。
    #[serde(rename = "pause_binding")]
    PauseBinding,
    /// 取值 `resume_binding`。
    #[serde(rename = "resume_binding")]
    ResumeBinding,
    /// 取值 `download_entry`。
    #[serde(rename = "download_entry")]
    DownloadEntry,
    /// 取值 `release_entry`。
    #[serde(rename = "release_entry")]
    ReleaseEntry,
    /// 取值 `list_problems`。
    #[serde(rename = "list_problems")]
    ListProblems,
    /// 取值 `retry_problem`。
    #[serde(rename = "retry_problem")]
    RetryProblem,
    /// 取值 `detach_binding`。
    #[serde(rename = "detach_binding")]
    DetachBinding,
    /// 取值 `export_recovery`。
    #[serde(rename = "export_recovery")]
    ExportRecovery,
    /// 取值 `get_connection`。
    #[serde(rename = "get_connection")]
    GetConnection,
    /// 取值 `set_connection`。
    #[serde(rename = "set_connection")]
    SetConnection,
    /// 取值 `login`。
    #[serde(rename = "login")]
    Login,
    /// 取值 `logout`。
    #[serde(rename = "logout")]
    Logout,
    /// 取值 `list_roots`。
    #[serde(rename = "list_roots")]
    ListRoots,
    /// 取值 `set_mode`。
    #[serde(rename = "set_mode")]
    SetMode,
    /// 取值 `list_history`。
    #[serde(rename = "list_history")]
    ListHistory,
    /// 取值 `list_trash`。
    #[serde(rename = "list_trash")]
    ListTrash,
    /// 取值 `restore_entry`。
    #[serde(rename = "restore_entry")]
    RestoreEntry,
    /// 取值 `list_files`。
    #[serde(rename = "list_files")]
    ListFiles,
    /// 取值 `claim_notification`。
    #[serde(rename = "claim_notification")]
    ClaimNotification,
}

/// 可选字段的组合由处理器按命令校验；所有身份规范小写十六进制，所有路径由 Windows 安全句柄复核。登录凭据仅请求中瞬时使用，不返回、不记录日志。
#[derive(Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CommandParameters {
    /// 订阅最后看到的 instance:counter；缺省或不匹配立即返回全量。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_version: Option<String>,
    /// 恢复前当前修订 CAS。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    /// Binding 身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
    /// 用户显式选择的附加 CA，不禁用系统证书验证。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<String>,
    /// 专属且与所有同步根/状态目录分离的缓存。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,
    /// 历史或回收站分页的 after 身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// 新的恢复导出目录；不可覆盖。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    /// 条目身份，不接受调用方自选相对文件路径。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// 用户显式选定的本机绝对目录。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    /// full 或 on_demand。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// 当前活动代际正整数字符串，旧GUI不能领取新复发。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_generation: Option<String>,
    /// agent通知记录键，仅领取投递，不清问题。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_key: Option<String>,
    /// 重试/副作用幂等身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// 瞬时登录密码；不能日志输出整个请求。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// 授权 Root 身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
    /// 问题路径或原因的大小写不敏感子串筛选。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// 连接的服务器稳定身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    /// HTTPS 地址，不允许用户信息、query 或 fragment。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    /// 登录账号。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// 固定历史版本。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

impl core::fmt::Debug for CommandParameters {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut value = f.debug_struct("CommandParameters");
        value.field("after_version", &self.after_version);
        value.field("base_revision", &self.base_revision);
        value.field("binding_id", &self.binding_id);
        value.field("ca_file", &self.ca_file);
        value.field("cache_path", &self.cache_path);
        value.field("cursor", &self.cursor);
        value.field("destination", &self.destination);
        value.field("entry_id", &self.entry_id);
        value.field("local_path", &self.local_path);
        value.field("mode", &self.mode);
        value.field("notification_generation", &self.notification_generation);
        value.field("notification_key", &self.notification_key);
        value.field("operation_id", &self.operation_id);
        value.field("password", &"[REDACTED]");
        value.field("root_id", &self.root_id);
        value.field("search", &self.search);
        value.field("server_id", &self.server_id);
        value.field("server_url", &self.server_url);
        value.field("username", &self.username);
        value.field("version_id", &self.version_id);
        value.finish()
    }
}

/// 不含 token 的连接与证书配置。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ConnectionInfo {
    /// 持有凭据；不等于服务器在线。
    pub authenticated: bool,
    /// 附加信任 CA 路径。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<String>,
    /// 服务器身份。
    pub server_id: String,
    /// 服务器地址。
    pub server_url: String,
}

/// 错误应答主体：协议级失败的原因。带 root_id/details 的同步问题由问题页命令返回，不在本契约里。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ErrorBody {
    /// 错误码。
    pub code: LocalIpcErrorCode,
    /// 界面上直接可读的说明；不含内部绝对路径或凭据。
    pub message: String,
    /// 调用方是否可原样重试；协议不兼容一律为 false。
    pub retryable: bool,
}

/// agent安全扫描及持久状态的文件投影；不是释放授权，动作时必须重新核验。无条目身份的本地新文件不能执行远端动作。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct FileStatus {
    /// online/offline/root_unavailable；离线来自持久调度事实，不臆造在线。
    pub availability: String,
    /// 无法取得当前保护句柄；不保证此后一直空闲。
    pub busy: bool,
    /// 持久未确认状态或物理修改证据。
    pub dirty: bool,
    /// 已确认基线身份；本地未导入文件省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// 核对后的相对路径。
    pub path: String,
    /// 存在相关未完成动作/事件，绝不表示committed。
    pub pending: bool,
    /// 当前UI可提示释放；后端仍重核。
    pub release_allowed: bool,
    /// 释放禁用原因或影响提示。
    pub release_reason: String,
    /// current/pending/blocked_by_local/conflicted。
    pub remote_apply: String,
    /// metadata_only/partial/materialized/unknown，不猜测完整驻留。
    pub residency: String,
    /// domain EntryState::project结果。
    pub status: String,
    /// 基线已确认且没有本地待办；不表示备份。
    pub synchronized: bool,
    /// idle/queued/running/retry_wait/failed。
    pub transfer: String,
}

/// get_status 的结果：agent 身份、IPC 协议与真实 Binding 多维状态。bindings 为 1.1 可选增量；旧处理器未接状态源时省略，不伪装成空快照。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct GetStatusResult {
    /// agent 二进制版本（构建时确定的语义化版本文本），供界面与日志显示。
    pub agent_version: String,
    /// 真实核心快照；应用入口始终返回该字段，读取失败则请求失败，不用空数组替代。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<BindingStatus>>,
    /// agent 当前支持的 IPC 协议版本。
    pub ipc_protocol: IpcVersion,
    /// 1.2 管理命令的类型化结果。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management: Option<ManagementResult>,
    /// 进程唯一身份加单调计数；不同进程的版本不可比较。丢失订阅后 get_status 始终返回全量。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_version: Option<String>,
}

/// 本机 IPC 协议版本。major 表示帧格式与命令语义的不兼容变更，minor 表示向后兼容的增量。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct IpcVersion {
    /// 不兼容版本号；与 agent 支持的 major 不同即拒绝。（取值范围 0..=65535）
    pub major: u32,
    /// 兼容增量版本号；高于 agent 支持的 minor 时同样拒绝，避免调用方假设尚未实现的命令存在。（取值范围 0..=65535）
    pub minor: u32,
}

/// 管理入口错误分类；同步错误的稳定 code 放在 problems，不泄露自由诊断或凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LocalIpcErrorCode {
    /// 取值 `unsupported_version`。
    #[serde(rename = "unsupported_version")]
    UnsupportedVersion,
    /// 取值 `invalid_argument`。
    #[serde(rename = "invalid_argument")]
    InvalidArgument,
    /// 取值 `not_found`。
    #[serde(rename = "not_found")]
    NotFound,
    /// 取值 `blocked`。
    #[serde(rename = "blocked")]
    Blocked,
    /// 取值 `unavailable`。
    #[serde(rename = "unavailable")]
    Unavailable,
    /// 取值 `unauthenticated`。
    #[serde(rename = "unauthenticated")]
    Unauthenticated,
    /// 取值 `conflict`。
    #[serde(rename = "conflict")]
    Conflict,
    /// 取值 `internal`。
    #[serde(rename = "internal")]
    Internal,
}

/// 每命令只填实际取得的结果；不以空列表代替失败。副作用 ok 代表实际完成，不是提交已排队。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
pub struct ManagementResult {
    /// 持久绑定列表。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<BindingInfo>>,
    /// 连接事实。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionInfo>,
    /// 已完整核验的恢复包文件数。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported_files: Option<String>,
    /// list_files当前页，最多100项；next_cursor可继续，不是全目录原子快照。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileStatus>>,
    /// 解绑后不保证离线可读的条目。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<Vec<String>>,
    /// 远端授权 Root/历史/回收站列表。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<RemoteItem>>,
    /// 模式任务仍需真实核对/补齐的条目数。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_remaining: Option<String>,
    /// 历史或回收站清单下一页游标。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// 问题生命周期与投递事实
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notices: Option<Vec<Notice>>,
    /// 是否成功领取本活动代际
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_claimed: Option<bool>,
    /// 实际执行的幂等操作身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// 持久问题列表。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problems: Option<Vec<Problem>>,
    /// committed后按条目身份重新查询的当前位置
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_path: Option<String>,
    /// 解绑时保留的未确认条目。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unconfirmed: Option<Vec<String>>,
}

/// 核心Root原因与恢复/投递状态；claim不等于用户已读或Windows实际展示。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Notice {
    /// Root原因，冲突副本为CONFLICT_COPY
    pub cause: String,
    /// 本代际已领取系统投递
    pub claimed: bool,
    /// 已分类错误码
    pub code: String,
    /// 可信核心分类需用户处理
    pub critical: bool,
    /// 活动代际
    pub generation: String,
    /// 稳定去重键
    pub key: String,
    /// 关联Root，非绑定身份
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_id: Option<String>,
    /// active或resolved
    pub state: String,
}

/// 真实持久问题；不返回任意日志文本。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Problem {
    /// Binding 身份。
    pub binding_id: String,
    /// 稳定原因码。
    pub code: String,
    /// 相关条目。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// 本地数据保护/驻留情况，不把数据库 clean 当真实完整字节。
    pub local_data: String,
    /// 可执行下一步。
    pub next_step: String,
    /// 已核对的相对路径；未知时省略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 待办操作身份或只读诊断身份。
    pub problem_id: String,
    /// 原因说明。
    pub reason: String,
    /// 是否可调用 retry_problem。
    pub retryable: bool,
}

/// 服务器返回的授权 Root、历史版本或回收站项目。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct RemoteItem {
    /// 冲突副本的原条目身份
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_source: Option<String>,
    /// 条目身份。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// 历史/回收站到期下限UTC秒，当前活跃版本省略
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// 内容摘要。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// 稳定身份。
    pub id: String,
    /// 显示名称。
    pub name: String,
    /// 原父目录。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// 当前 CAS 修订。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// 字节数。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// 固定版本。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
}

/// 一次命令请求：请求身份、调用方使用的协议版本与命令负载。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Request {
    /// 命令名与参数。
    pub command: Command,
    /// 调用方使用的 IPC 协议版本，agent 据此判定兼容性。
    pub ipc_version: IpcVersion,
    /// 调用方生成的不透明请求标识；agent 原样回显，不解析内容、不据此授权。
    pub request_id: String,
}

/// 一次命令的应答：回显 request_id 与应答种类，ok 只带 result、error 只带 error。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Response {
    /// kind 为 error 时的错误主体。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
    /// 应答种类，决定 result 与 error 哪个存在。
    pub kind: ResponseKind,
    /// 回显请求的 request_id，用于把并发应答与请求配对。
    pub request_id: String,
    /// kind 为 ok 时的命令结果。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<GetStatusResult>,
}

/// 应答种类：ok 只携带 result，error 只携带 error，两者恰有其一。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum ResponseKind {
    /// 取值 `ok`。
    #[serde(rename = "ok")]
    Ok,
    /// 取值 `error`。
    #[serde(rename = "error")]
    Error,
}
