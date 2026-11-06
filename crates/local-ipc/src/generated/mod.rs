//! 本文件由 `cargo xtask generate` 从 `api/local-ipc.schema.json` 生成，禁止手改。
//!
//! 契约：vaultsyncd 本机 IPC（api/local-ipc.schema.json）；生成器：vaultsyncd-xtask 0.1.0
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
pub const PROTOCOL_MINOR: u32 = 1;
/// 帧长度头的字节数：大端无符号整数，表示后续 JSON 负载的字节数。
pub const FRAME_HEADER_BYTES: usize = 4;
/// 单帧负载上限（字节）：超过即在读帧阶段拒绝，不为坏帧扩容。
pub const MAX_FRAME_BYTES: usize = 1048576;

/// 运行中 agent 从核心持久状态读取的单 Binding 多维快照。计数为十进制文本，不含本机路径或凭据；synchronized 只能由核心完整判定，不能仅按 pending 判定。每个 Binding 的字段来自同一读事务，不承诺多 Binding 原子性；本卡不提供状态推送。
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

/// 命令负载：命令名加该命令自己的参数。当前唯一的 get_status 没有参数。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Command {
    /// 要执行的命令名；未知取值在解码阶段被拒绝。
    pub name: CommandName,
}

/// 命令名闭集。S025 只实现 get_status；design §15.5 其余命令在其功能卡加入，取值与含义在契约里逐项追加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum CommandName {
    /// 取值 `get_status`。
    #[serde(rename = "get_status")]
    GetStatus,
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
}

/// 本机 IPC 协议版本。major 表示帧格式与命令语义的不兼容变更，minor 表示向后兼容的增量。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct IpcVersion {
    /// 不兼容版本号；与 agent 支持的 major 不同即拒绝。（取值范围 0..=65535）
    pub major: u32,
    /// 兼容增量版本号；高于 agent 支持的 minor 时同样拒绝，避免调用方假设尚未实现的命令存在。（取值范围 0..=65535）
    pub minor: u32,
}

/// 本机 IPC 协议级错误码闭集，不是 design §15.4 的同步错误码。S025 只有版本不兼容；需要新错误时先扩展本契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum LocalIpcErrorCode {
    /// 取值 `unsupported_version`。
    #[serde(rename = "unsupported_version")]
    UnsupportedVersion,
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
