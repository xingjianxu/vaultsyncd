// 本文件由 `cargo xtask generate` 从 `api/local-ipc.schema.json` 生成，禁止手改。
//
// 契约：vaultsyncd 本机 IPC（api/local-ipc.schema.json）；生成器：vaultsyncd-xtask 0.1.0
// Windows agent 与本机 GUI/CLI 之间的最小命令契约（docs/design.md §15.5、§5.6）。
// 本文件是本机 IPC 的唯一编辑源；`cargo xtask generate` 据此生成
// crates/local-ipc 的 Rust DTO 与 packages/local-ipc-types 的 TypeScript 类型。
// 本契约与服务器管理 API 分开：它不经网络传输，也不复用 api/openapi.yaml。
// 消息按帧编码：FRAME_HEADER_BYTES 字节的大端长度头（不含头自身）加该长度的
// UTF-8 JSON 负载，单帧负载上限为 MAX_FRAME_BYTES；两个常量由 x-vaultsyncd 生成。
// 请求方按 Request 编码，agent 按 Response 应答；request_id 原样回显，
// 调用方据此把并发应答配对，agent 不解析它的内容。
// 未知命令、未知闭集取值与畸形 JSON 一律拒绝，不当作默认成功：
// 解码失败发生在产生应答之前，由传输层结束该连接。
//
// 解码器只做 wire 形态校验：未知命令、未知应答种类、缺失的必填字段都会抛错，
// 界面不会拿到“看起来可用”的默认值。

/** agent 支持的 IPC 协议 major 版本（major 不同即不兼容）。 */
export const PROTOCOL_MAJOR = 1;

/** agent 支持的 IPC 协议 minor 版本（高于此值的调用方被拒绝）。 */
export const PROTOCOL_MINOR = 1;

/** 帧长度头的字节数：大端无符号整数，表示后续 JSON 负载的字节数。 */
export const FRAME_HEADER_BYTES = 4;

/** 单帧负载上限（字节）：超过即拒绝，调用方与 agent 用同一个上限。 */
export const MAX_FRAME_BYTES = 1048576;

function expect_object(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`期望 ${what} 是对象`);
  }
  return value as Record<string, unknown>;
}

function expect_string(value: unknown, what: string): string {
  if (typeof value !== "string") {
    throw new Error(`期望 ${what} 是字符串`);
  }
  return value;
}

function expect_boolean(value: unknown, what: string): boolean {
  if (typeof value !== "boolean") {
    throw new Error(`期望 ${what} 是布尔值`);
  }
  return value;
}

function expect_array(value: unknown, what: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new Error(`期望 ${what} 是数组`);
  }
  return value;
}

function expect_number(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new Error(`期望 ${what} 是整数`);
  }
  return value;
}

/** 运行中 agent 从核心持久状态读取的单 Binding 多维快照。计数为十进制文本，不含本机路径或凭据；synchronized 只能由核心完整判定，不能仅按 pending 判定。每个 Binding 的字段来自同一读事务，不承诺多 Binding 原子性；本卡不提供状态推送。 */
export interface BindingStatus {
  /** Binding 稳定身份。 */
  binding_id: string;
  /** Binding 已解除。 */
  detached: boolean;
  /** 未确认本地内容数量（十进制）。 */
  dirty: string;
  /** 接收与应用游标间隔（十进制）。 */
  event_lag: string;
  /** 失败队列数量（十进制）。 */
  failed: string;
  /** 开放本地意图数量（十进制）。 */
  intents: string;
  /** 完整驻留数量（十进制）。 */
  materialized: string;
  /** 仅元数据占位数量（十进制）。 */
  metadata_only: string;
  /** 模式补齐尚未完成。 */
  mode_pending: boolean;
  /** 断网任务数量（十进制）。 */
  offline: string;
  /** 部分驻留数量（十进制）。 */
  partial: string;
  /** Binding 已暂停。 */
  paused: boolean;
  /** 尚未完成队列数量（十进制），不等于 committed。 */
  pending: string;
  /** 未完成恢复数量（十进制）。 */
  recovery: string;
  /** 核心根据全部维度判定的完成事实；暂停单独展示。 */
  synchronized: boolean;
  /** 未完成传输数量（十进制）。 */
  transfers: string;
  /** 未核对内容或需重新协调数量（十进制）。 */
  unknown: string;
}

export function decodeBindingStatus(value: unknown): BindingStatus {
  const object = expect_object(value, "BindingStatus");
  return {
    binding_id: expect_string(object.binding_id, "BindingStatus.binding_id"),
    detached: expect_boolean(object.detached, "BindingStatus.detached"),
    dirty: expect_string(object.dirty, "BindingStatus.dirty"),
    event_lag: expect_string(object.event_lag, "BindingStatus.event_lag"),
    failed: expect_string(object.failed, "BindingStatus.failed"),
    intents: expect_string(object.intents, "BindingStatus.intents"),
    materialized: expect_string(object.materialized, "BindingStatus.materialized"),
    metadata_only: expect_string(object.metadata_only, "BindingStatus.metadata_only"),
    mode_pending: expect_boolean(object.mode_pending, "BindingStatus.mode_pending"),
    offline: expect_string(object.offline, "BindingStatus.offline"),
    partial: expect_string(object.partial, "BindingStatus.partial"),
    paused: expect_boolean(object.paused, "BindingStatus.paused"),
    pending: expect_string(object.pending, "BindingStatus.pending"),
    recovery: expect_string(object.recovery, "BindingStatus.recovery"),
    synchronized: expect_boolean(object.synchronized, "BindingStatus.synchronized"),
    transfers: expect_string(object.transfers, "BindingStatus.transfers"),
    unknown: expect_string(object.unknown, "BindingStatus.unknown"),
  };
}

/** 命令负载：命令名加该命令自己的参数。当前唯一的 get_status 没有参数。 */
export interface Command {
  /** 要执行的命令名；未知取值在解码阶段被拒绝。 */
  name: CommandName;
}

export function decodeCommand(value: unknown): Command {
  const object = expect_object(value, "Command");
  return {
    name: decodeCommandName(object.name),
  };
}

/** 命令名闭集。S025 只实现 get_status；design §15.5 其余命令在其功能卡加入，取值与含义在契约里逐项追加。 */
export type CommandName = "get_status";

export const COMMAND_NAME_VALUES = ["get_status"] as const;

export function decodeCommandName(value: unknown): CommandName {
  if (typeof value !== "string" || !(COMMAND_NAME_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 CommandName 取值：${JSON.stringify(value)}`);
  }
  return value as CommandName;
}

/** 错误应答主体：协议级失败的原因。带 root_id/details 的同步问题由问题页命令返回，不在本契约里。 */
export interface ErrorBody {
  /** 错误码。 */
  code: LocalIpcErrorCode;
  /** 界面上直接可读的说明；不含内部绝对路径或凭据。 */
  message: string;
  /** 调用方是否可原样重试；协议不兼容一律为 false。 */
  retryable: boolean;
}

export function decodeErrorBody(value: unknown): ErrorBody {
  const object = expect_object(value, "ErrorBody");
  return {
    code: decodeLocalIpcErrorCode(object.code),
    message: expect_string(object.message, "ErrorBody.message"),
    retryable: expect_boolean(object.retryable, "ErrorBody.retryable"),
  };
}

/** get_status 的结果：agent 身份、IPC 协议与真实 Binding 多维状态。bindings 为 1.1 可选增量；旧处理器未接状态源时省略，不伪装成空快照。 */
export interface GetStatusResult {
  /** agent 二进制版本（构建时确定的语义化版本文本），供界面与日志显示。 */
  agent_version: string;
  /** 真实核心快照；应用入口始终返回该字段，读取失败则请求失败，不用空数组替代。 */
  bindings?: Array<BindingStatus>;
  /** agent 当前支持的 IPC 协议版本。 */
  ipc_protocol: IpcVersion;
}

export function decodeGetStatusResult(value: unknown): GetStatusResult {
  const object = expect_object(value, "GetStatusResult");
  return {
    agent_version: expect_string(object.agent_version, "GetStatusResult.agent_version"),
    bindings: object.bindings === undefined ? undefined : expect_array(object.bindings, "GetStatusResult.bindings").map((item) => decodeBindingStatus(item)),
    ipc_protocol: decodeIpcVersion(object.ipc_protocol),
  };
}

/** 本机 IPC 协议版本。major 表示帧格式与命令语义的不兼容变更，minor 表示向后兼容的增量。 */
export interface IpcVersion {
  /** 不兼容版本号；与 agent 支持的 major 不同即拒绝。（取值范围 0..=65535） */
  major: number;
  /** 兼容增量版本号；高于 agent 支持的 minor 时同样拒绝，避免调用方假设尚未实现的命令存在。（取值范围 0..=65535） */
  minor: number;
}

export function decodeIpcVersion(value: unknown): IpcVersion {
  const object = expect_object(value, "IpcVersion");
  return {
    major: expect_number(object.major, "IpcVersion.major"),
    minor: expect_number(object.minor, "IpcVersion.minor"),
  };
}

/** 本机 IPC 协议级错误码闭集，不是 design §15.4 的同步错误码。S025 只有版本不兼容；需要新错误时先扩展本契约。 */
export type LocalIpcErrorCode = "unsupported_version";

export const LOCAL_IPC_ERROR_CODE_VALUES = ["unsupported_version"] as const;

export function decodeLocalIpcErrorCode(value: unknown): LocalIpcErrorCode {
  if (typeof value !== "string" || !(LOCAL_IPC_ERROR_CODE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 LocalIpcErrorCode 取值：${JSON.stringify(value)}`);
  }
  return value as LocalIpcErrorCode;
}

/** 一次命令请求：请求身份、调用方使用的协议版本与命令负载。 */
export interface Request {
  /** 命令名与参数。 */
  command: Command;
  /** 调用方使用的 IPC 协议版本，agent 据此判定兼容性。 */
  ipc_version: IpcVersion;
  /** 调用方生成的不透明请求标识；agent 原样回显，不解析内容、不据此授权。 */
  request_id: string;
}

export function decodeRequest(value: unknown): Request {
  const object = expect_object(value, "Request");
  return {
    command: decodeCommand(object.command),
    ipc_version: decodeIpcVersion(object.ipc_version),
    request_id: expect_string(object.request_id, "Request.request_id"),
  };
}

/** 一次命令的应答：回显 request_id 与应答种类，ok 只带 result、error 只带 error。 */
export interface Response {
  /** kind 为 error 时的错误主体。 */
  error?: ErrorBody;
  /** 应答种类，决定 result 与 error 哪个存在。 */
  kind: ResponseKind;
  /** 回显请求的 request_id，用于把并发应答与请求配对。 */
  request_id: string;
  /** kind 为 ok 时的命令结果。 */
  result?: GetStatusResult;
}

export function decodeResponse(value: unknown): Response {
  const object = expect_object(value, "Response");
  return {
    error: object.error === undefined ? undefined : decodeErrorBody(object.error),
    kind: decodeResponseKind(object.kind),
    request_id: expect_string(object.request_id, "Response.request_id"),
    result: object.result === undefined ? undefined : decodeGetStatusResult(object.result),
  };
}

/** 应答种类：ok 只携带 result，error 只携带 error，两者恰有其一。 */
export type ResponseKind = "ok" | "error";

export const RESPONSE_KIND_VALUES = ["ok", "error"] as const;

export function decodeResponseKind(value: unknown): ResponseKind {
  if (typeof value !== "string" || !(RESPONSE_KIND_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 ResponseKind 取值：${JSON.stringify(value)}`);
  }
  return value as ResponseKind;
}
