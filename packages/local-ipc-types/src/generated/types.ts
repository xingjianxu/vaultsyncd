// 本文件由 `cargo xtask generate` 从 `api/local-ipc.schema.json` 生成，禁止手改。
//
// 契约：vaultsyncd 本机 IPC（api/local-ipc.schema.json）；生成器：vaultsyncd-xtask 0.2.0
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
export const PROTOCOL_MAJOR = 2;

/** agent 支持的 IPC 协议 minor 版本（高于此值的调用方被拒绝）。 */
export const PROTOCOL_MINOR = 10;

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

/** 服务端观察到的同步与传输事实；served 仅证明服务端提供内容，不证明客户端收到、校验或应用。时间为服务端UTC秒，方向相对于客户端。 */
export interface ActivityRecord {
  /** upload_finalize、sync_commit 或 download。 */
  action: string;
  /** 本次提供的内容长度（字节）；不代表客户端已接收。 */
  bytes?: string;
  /** 认证连接身份。 */
  client_id?: string;
  /** 当时接入类型。 */
  client_kind: string;
  /** 当时客户端名称。 */
  client_label: string;
  /** 操作携带的设备身份。 */
  device_id?: string;
  /** upload 或 download，相对于客户端。 */
  direction: string;
  /** 条目身份。 */
  entry_id?: string;
  /** 失败原因码。 */
  error_code?: string;
  /** 递增记录ID，规范正十进制。 */
  id: string;
  /** 服务端观察时间，UTC秒。 */
  occurred_at: string;
  /** 下载区间起点（字节）。 */
  offset?: string;
  /** 同步操作身份。 */
  operation_id?: string;
  /** 事件时文件相对路径；未提交内容可能没有路径。 */
  path?: string;
  /** 完整内容长度（字节）。 */
  size?: string;
  /** ready、committed、served 或 failed；不得把 ready/served 当同步完成。 */
  status: string;
  /** 上传会话身份。 */
  upload_id?: string;
  /** 固定版本身份。 */
  version_id?: string;
}

export function decodeActivityRecord(value: unknown): ActivityRecord {
  const object = expect_object(value, "ActivityRecord");
  return {
    action: expect_string(object.action, "ActivityRecord.action"),
    bytes: object.bytes === undefined ? undefined : expect_string(object.bytes, "ActivityRecord.bytes"),
    client_id: object.client_id === undefined ? undefined : expect_string(object.client_id, "ActivityRecord.client_id"),
    client_kind: expect_string(object.client_kind, "ActivityRecord.client_kind"),
    client_label: expect_string(object.client_label, "ActivityRecord.client_label"),
    device_id: object.device_id === undefined ? undefined : expect_string(object.device_id, "ActivityRecord.device_id"),
    direction: expect_string(object.direction, "ActivityRecord.direction"),
    entry_id: object.entry_id === undefined ? undefined : expect_string(object.entry_id, "ActivityRecord.entry_id"),
    error_code: object.error_code === undefined ? undefined : expect_string(object.error_code, "ActivityRecord.error_code"),
    id: expect_string(object.id, "ActivityRecord.id"),
    occurred_at: expect_string(object.occurred_at, "ActivityRecord.occurred_at"),
    offset: object.offset === undefined ? undefined : expect_string(object.offset, "ActivityRecord.offset"),
    operation_id: object.operation_id === undefined ? undefined : expect_string(object.operation_id, "ActivityRecord.operation_id"),
    path: object.path === undefined ? undefined : expect_string(object.path, "ActivityRecord.path"),
    size: object.size === undefined ? undefined : expect_string(object.size, "ActivityRecord.size"),
    status: expect_string(object.status, "ActivityRecord.status"),
    upload_id: object.upload_id === undefined ? undefined : expect_string(object.upload_id, "ActivityRecord.upload_id"),
    version_id: object.version_id === undefined ? undefined : expect_string(object.version_id, "ActivityRecord.version_id"),
  };
}

/** 持久 Binding 配置与控制事实，包括解绑记录。 */
export interface BindingInfo {
  /** Binding 身份。 */
  binding_id: string;
  /** 保留的缓存。 */
  cache_path: string;
  /** 保留数据停止管理。 */
  detached: boolean;
  /** 用户选择的目录。 */
  local_path: string;
  /** 持久目标模式。 */
  mode: string;
  /** 已暂停。 */
  paused: boolean;
  /** 绑定所属服务器的稳定身份；旧 agent 省略。 */
  server_id?: string;
  /** Vault 身份。 */
  vault_id: string;
}

export function decodeBindingInfo(value: unknown): BindingInfo {
  const object = expect_object(value, "BindingInfo");
  return {
    binding_id: expect_string(object.binding_id, "BindingInfo.binding_id"),
    cache_path: expect_string(object.cache_path, "BindingInfo.cache_path"),
    detached: expect_boolean(object.detached, "BindingInfo.detached"),
    local_path: expect_string(object.local_path, "BindingInfo.local_path"),
    mode: expect_string(object.mode, "BindingInfo.mode"),
    paused: expect_boolean(object.paused, "BindingInfo.paused"),
    server_id: object.server_id === undefined ? undefined : expect_string(object.server_id, "BindingInfo.server_id"),
    vault_id: expect_string(object.vault_id, "BindingInfo.vault_id"),
  };
}

/** 运行中 agent 从核心持久状态读取的单 Binding 多维快照。计数为十进制文本，不含本机路径或凭据；synchronized 只能由核心完整判定，不能仅按 pending 判定。每个 Binding 的字段来自同一读事务，不承诺多 Binding 原子性；1.2 的 subscribe_status 按实例版本长轮询推送全量，get_status 可无条件恢复丢失提示。 */
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

/** 命令负载。parameters 按命令逐项校验必填项与允许项；多余参数、非规范身份、相对路径、越界文本一律失败。1.0/1.1 仍支持无参数 get_status。 */
export interface Command {
  /** 要执行的命令名；未知取值在解码阶段被拒绝。 */
  name: CommandName;
  /** 命令专属参数；执行器复核，不能仅信任界面。 */
  parameters?: CommandParameters;
}

export function decodeCommand(value: unknown): Command {
  const object = expect_object(value, "Command");
  if (Object.keys(object).some((key) => !["name", "parameters"].includes(key))) {
    throw new Error("Command contains unknown fields");
  }
  return {
    name: decodeCommandName(object.name),
    parameters: object.parameters === undefined ? undefined : decodeCommandParameters(object.parameters),
  };
}

/** 本机管理命令闭集；新增命令自协议 1.2 起提供，旧版本只支持 get_status。 */
export type CommandName = "get_status" | "subscribe_status" | "list_bindings" | "create_binding" | "pause_binding" | "resume_binding" | "download_entry" | "release_entry" | "list_problems" | "retry_problem" | "detach_binding" | "export_recovery" | "get_connection" | "list_connections" | "discover_connection" | "set_connection" | "login" | "logout" | "list_vaults" | "set_mode" | "list_history" | "list_trash" | "restore_entry" | "list_files" | "claim_notification" | "prepare_maintenance" | "get_rules" | "set_rules" | "list_activity";

export const COMMAND_NAME_VALUES = ["get_status", "subscribe_status", "list_bindings", "create_binding", "pause_binding", "resume_binding", "download_entry", "release_entry", "list_problems", "retry_problem", "detach_binding", "export_recovery", "get_connection", "list_connections", "discover_connection", "set_connection", "login", "logout", "list_vaults", "set_mode", "list_history", "list_trash", "restore_entry", "list_files", "claim_notification", "prepare_maintenance", "get_rules", "set_rules", "list_activity"] as const;

export function decodeCommandName(value: unknown): CommandName {
  if (typeof value !== "string" || !(COMMAND_NAME_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 CommandName 取值：${JSON.stringify(value)}`);
  }
  return value as CommandName;
}

/** 可选字段的组合由处理器按命令校验；所有身份规范小写十六进制，所有路径由 Windows 安全句柄复核。登录凭据仅请求中瞬时使用，不返回、不记录日志。 */
export interface CommandParameters {
  /** 订阅最后看到的 instance:counter；缺省或不匹配立即返回全量。 */
  after_version?: string;
  /** 恢复前当前修订 CAS。 */
  base_revision?: string;
  /** Binding 身份。 */
  binding_id?: string;
  /** 用户显式选择的附加 CA，不禁用系统证书验证。 */
  ca_file?: string;
  /** 专属且与所有同步根/状态目录分离的缓存。 */
  cache_path?: string;
  /** 历史/回收站/文件分页的 after 身份；list_activity 为规范正十进制 before 记录ID。 */
  cursor?: string;
  /** 新的恢复导出目录；不可覆盖。 */
  destination?: string;
  /** list_activity 的方向筛选，仅 upload 或 download；相对于客户端。 */
  direction?: string;
  /** 条目身份，不接受调用方自选相对文件路径。 */
  entry_id?: string;
  /** 用户显式选定的本机绝对目录。 */
  local_path?: string;
  /** full 或 on_demand。 */
  mode?: string;
  /** 当前活动代际正整数字符串，旧GUI不能领取新复发。 */
  notification_generation?: string;
  /** agent通知记录键，仅领取投递，不清问题。 */
  notification_key?: string;
  /** 重试/副作用幂等身份。 */
  operation_id?: string;
  /** 瞬时登录密码；不能日志输出整个请求。 */
  password?: string;
  /** 完整绑定级排除清单，允许空文本，最多4096字节；保存不删除任何内容。 */
  rules?: string;
  /** 不可复用的规则版本；同版本同文本幂等。 */
  rules_version?: string;
  /** 问题路径或原因的大小写不敏感子串筛选。 */
  search?: string;
  /** 连接的服务器稳定身份。 */
  server_id?: string;
  /** HTTPS 地址，不允许用户信息、query 或 fragment。 */
  server_url?: string;
  /** 登录账号。 */
  username?: string;
  /** 授权 Vault 身份。 */
  vault_id?: string;
  /** 固定历史版本。 */
  version_id?: string;
}

export function decodeCommandParameters(value: unknown): CommandParameters {
  const object = expect_object(value, "CommandParameters");
  if (Object.keys(object).some((key) => !["after_version", "base_revision", "binding_id", "ca_file", "cache_path", "cursor", "destination", "direction", "entry_id", "local_path", "mode", "notification_generation", "notification_key", "operation_id", "password", "rules", "rules_version", "search", "server_id", "server_url", "username", "vault_id", "version_id"].includes(key))) {
    throw new Error("CommandParameters contains unknown fields");
  }
  return {
    after_version: object.after_version === undefined ? undefined : expect_string(object.after_version, "CommandParameters.after_version"),
    base_revision: object.base_revision === undefined ? undefined : expect_string(object.base_revision, "CommandParameters.base_revision"),
    binding_id: object.binding_id === undefined ? undefined : expect_string(object.binding_id, "CommandParameters.binding_id"),
    ca_file: object.ca_file === undefined ? undefined : expect_string(object.ca_file, "CommandParameters.ca_file"),
    cache_path: object.cache_path === undefined ? undefined : expect_string(object.cache_path, "CommandParameters.cache_path"),
    cursor: object.cursor === undefined ? undefined : expect_string(object.cursor, "CommandParameters.cursor"),
    destination: object.destination === undefined ? undefined : expect_string(object.destination, "CommandParameters.destination"),
    direction: object.direction === undefined ? undefined : expect_string(object.direction, "CommandParameters.direction"),
    entry_id: object.entry_id === undefined ? undefined : expect_string(object.entry_id, "CommandParameters.entry_id"),
    local_path: object.local_path === undefined ? undefined : expect_string(object.local_path, "CommandParameters.local_path"),
    mode: object.mode === undefined ? undefined : expect_string(object.mode, "CommandParameters.mode"),
    notification_generation: object.notification_generation === undefined ? undefined : expect_string(object.notification_generation, "CommandParameters.notification_generation"),
    notification_key: object.notification_key === undefined ? undefined : expect_string(object.notification_key, "CommandParameters.notification_key"),
    operation_id: object.operation_id === undefined ? undefined : expect_string(object.operation_id, "CommandParameters.operation_id"),
    password: object.password === undefined ? undefined : expect_string(object.password, "CommandParameters.password"),
    rules: object.rules === undefined ? undefined : expect_string(object.rules, "CommandParameters.rules"),
    rules_version: object.rules_version === undefined ? undefined : expect_string(object.rules_version, "CommandParameters.rules_version"),
    search: object.search === undefined ? undefined : expect_string(object.search, "CommandParameters.search"),
    server_id: object.server_id === undefined ? undefined : expect_string(object.server_id, "CommandParameters.server_id"),
    server_url: object.server_url === undefined ? undefined : expect_string(object.server_url, "CommandParameters.server_url"),
    username: object.username === undefined ? undefined : expect_string(object.username, "CommandParameters.username"),
    vault_id: object.vault_id === undefined ? undefined : expect_string(object.vault_id, "CommandParameters.vault_id"),
    version_id: object.version_id === undefined ? undefined : expect_string(object.version_id, "CommandParameters.version_id"),
  };
}

/** 不含 token 的连接与证书配置。 */
export interface ConnectionInfo {
  /** 持有凭据且未确认需要重新登录；不等于服务器在线。 */
  authenticated: boolean;
  /** 附加信任 CA 路径。 */
  ca_file?: string;
  /** 服务器身份。 */
  server_id: string;
  /** 服务器地址。 */
  server_url: string;
  /** 本服务上次成功登录的用户名；旧资料缺省，不含密码或令牌。 */
  username?: string;
}

export function decodeConnectionInfo(value: unknown): ConnectionInfo {
  const object = expect_object(value, "ConnectionInfo");
  return {
    authenticated: expect_boolean(object.authenticated, "ConnectionInfo.authenticated"),
    ca_file: object.ca_file === undefined ? undefined : expect_string(object.ca_file, "ConnectionInfo.ca_file"),
    server_id: expect_string(object.server_id, "ConnectionInfo.server_id"),
    server_url: expect_string(object.server_url, "ConnectionInfo.server_url"),
    username: object.username === undefined ? undefined : expect_string(object.username, "ConnectionInfo.username"),
  };
}

/** 错误应答主体：协议级失败的原因。带 vault_id/details 的同步问题由问题页命令返回，不在本契约里。 */
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

/** agent安全扫描及持久状态的文件投影；不是释放授权，动作时必须重新核验。无条目身份的本地新文件不能执行远端动作。 */
export interface FileStatus {
  /** online/offline/vault_unavailable；离线来自持久调度事实，不臆造在线。 */
  availability: string;
  /** 无法取得当前保护句柄；不保证此后一直空闲。 */
  busy: boolean;
  /** 持久未确认状态或物理修改证据。 */
  dirty: boolean;
  /** 已确认基线身份；本地未导入文件省略。 */
  entry_id?: string;
  /** 核对后的相对路径。 */
  path: string;
  /** 存在相关未完成动作/事件，绝不表示committed。 */
  pending: boolean;
  /** 当前UI可提示释放；后端仍重核。 */
  release_allowed: boolean;
  /** 释放禁用原因或影响提示。 */
  release_reason: string;
  /** current/pending/blocked_by_local/conflicted。 */
  remote_apply: string;
  /** metadata_only/partial/materialized/unknown，不猜测完整驻留。 */
  residency: string;
  /** domain EntryState::project结果。 */
  status: string;
  /** 基线已确认且没有本地待办；不表示备份。 */
  synchronized: boolean;
  /** idle/queued/running/retry_wait/failed。 */
  transfer: string;
}

export function decodeFileStatus(value: unknown): FileStatus {
  const object = expect_object(value, "FileStatus");
  return {
    availability: expect_string(object.availability, "FileStatus.availability"),
    busy: expect_boolean(object.busy, "FileStatus.busy"),
    dirty: expect_boolean(object.dirty, "FileStatus.dirty"),
    entry_id: object.entry_id === undefined ? undefined : expect_string(object.entry_id, "FileStatus.entry_id"),
    path: expect_string(object.path, "FileStatus.path"),
    pending: expect_boolean(object.pending, "FileStatus.pending"),
    release_allowed: expect_boolean(object.release_allowed, "FileStatus.release_allowed"),
    release_reason: expect_string(object.release_reason, "FileStatus.release_reason"),
    remote_apply: expect_string(object.remote_apply, "FileStatus.remote_apply"),
    residency: expect_string(object.residency, "FileStatus.residency"),
    status: expect_string(object.status, "FileStatus.status"),
    synchronized: expect_boolean(object.synchronized, "FileStatus.synchronized"),
    transfer: expect_string(object.transfer, "FileStatus.transfer"),
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
  /** 1.2 管理命令的类型化结果。 */
  management?: ManagementResult;
  /** 进程唯一身份加单调计数；不同进程的版本不可比较。丢失订阅后 get_status 始终返回全量。 */
  snapshot_version?: string;
}

export function decodeGetStatusResult(value: unknown): GetStatusResult {
  const object = expect_object(value, "GetStatusResult");
  return {
    agent_version: expect_string(object.agent_version, "GetStatusResult.agent_version"),
    bindings: object.bindings === undefined ? undefined : expect_array(object.bindings, "GetStatusResult.bindings").map((item) => decodeBindingStatus(item)),
    ipc_protocol: decodeIpcVersion(object.ipc_protocol),
    management: object.management === undefined ? undefined : decodeManagementResult(object.management),
    snapshot_version: object.snapshot_version === undefined ? undefined : expect_string(object.snapshot_version, "GetStatusResult.snapshot_version"),
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

/** 管理入口错误分类；同步错误的稳定 code 放在 problems，不泄露自由诊断或凭据。 */
export type LocalIpcErrorCode = "unsupported_version" | "invalid_argument" | "not_found" | "blocked" | "unavailable" | "unauthenticated" | "conflict" | "internal";

export const LOCAL_IPC_ERROR_CODE_VALUES = ["unsupported_version", "invalid_argument", "not_found", "blocked", "unavailable", "unauthenticated", "conflict", "internal"] as const;

export function decodeLocalIpcErrorCode(value: unknown): LocalIpcErrorCode {
  if (typeof value !== "string" || !(LOCAL_IPC_ERROR_CODE_VALUES as readonly string[]).includes(value)) {
    throw new Error(`未知的 LocalIpcErrorCode 取值：${JSON.stringify(value)}`);
  }
  return value as LocalIpcErrorCode;
}

/** 每命令只填实际取得的结果；不以空列表代替失败。副作用 ok 代表实际完成，不是提交已排队。 */
export interface ManagementResult {
  /** 所选绑定授权Vault的所有客户端服务端记录当前页。 */
  activity?: Array<ActivityRecord>;
  /** 持久绑定列表。 */
  bindings?: Array<BindingInfo>;
  /** 当前操作涉及的连接；状态应答仅在恰有一条连接时提供。 */
  connection?: ConnectionInfo;
  /** 本机已配置的服务器；未配置时为空列表。 */
  connections?: Array<ConnectionInfo>;
  /** 已完整核验的恢复包文件数。 */
  exported_files?: string;
  /** list_files当前页，最多100项；next_cursor可继续，不是全目录原子快照。 */
  files?: Array<FileStatus>;
  /** 解绑后不保证离线可读的条目。 */
  incomplete?: Array<string>;
  /** 远端授权 Vault/历史/回收站列表。 */
  items?: Array<RemoteItem>;
  /** 模式任务仍需真实核对/补齐的条目数。 */
  mode_remaining?: string;
  /** 文件/历史/回收站/同步与传输记录下一页游标；list_activity 为 before 记录ID。 */
  next_cursor?: string;
  /** 问题生命周期与投递事实 */
  notices?: Array<Notice>;
  /** 是否成功领取本活动代际 */
  notification_claimed?: boolean;
  /** 实际执行的幂等操作身份。 */
  operation_id?: string;
  /** 持久问题列表。 */
  problems?: Array<Problem>;
  /** committed后按条目身份重新查询的当前位置 */
  restored_path?: string;
  /** 当前完整排除清单，空字符串表示无排除规则。 */
  rules?: string;
  /** get_rules携带entry_id时，按持久身份解析路径并返回命中的规则/行号或包含原因。 */
  rules_explanation?: string;
  /** 当前持久规则版本。 */
  rules_version?: string;
  /** 解绑时保留的未确认条目。 */
  unconfirmed?: Array<string>;
}

export function decodeManagementResult(value: unknown): ManagementResult {
  const object = expect_object(value, "ManagementResult");
  return {
    activity: object.activity === undefined ? undefined : expect_array(object.activity, "ManagementResult.activity").map((item) => decodeActivityRecord(item)),
    bindings: object.bindings === undefined ? undefined : expect_array(object.bindings, "ManagementResult.bindings").map((item) => decodeBindingInfo(item)),
    connection: object.connection === undefined ? undefined : decodeConnectionInfo(object.connection),
    connections: object.connections === undefined ? undefined : expect_array(object.connections, "ManagementResult.connections").map((item) => decodeConnectionInfo(item)),
    exported_files: object.exported_files === undefined ? undefined : expect_string(object.exported_files, "ManagementResult.exported_files"),
    files: object.files === undefined ? undefined : expect_array(object.files, "ManagementResult.files").map((item) => decodeFileStatus(item)),
    incomplete: object.incomplete === undefined ? undefined : expect_array(object.incomplete, "ManagementResult.incomplete").map((item) => expect_string(item, "ManagementResult.incomplete 的元素")),
    items: object.items === undefined ? undefined : expect_array(object.items, "ManagementResult.items").map((item) => decodeRemoteItem(item)),
    mode_remaining: object.mode_remaining === undefined ? undefined : expect_string(object.mode_remaining, "ManagementResult.mode_remaining"),
    next_cursor: object.next_cursor === undefined ? undefined : expect_string(object.next_cursor, "ManagementResult.next_cursor"),
    notices: object.notices === undefined ? undefined : expect_array(object.notices, "ManagementResult.notices").map((item) => decodeNotice(item)),
    notification_claimed: object.notification_claimed === undefined ? undefined : expect_boolean(object.notification_claimed, "ManagementResult.notification_claimed"),
    operation_id: object.operation_id === undefined ? undefined : expect_string(object.operation_id, "ManagementResult.operation_id"),
    problems: object.problems === undefined ? undefined : expect_array(object.problems, "ManagementResult.problems").map((item) => decodeProblem(item)),
    restored_path: object.restored_path === undefined ? undefined : expect_string(object.restored_path, "ManagementResult.restored_path"),
    rules: object.rules === undefined ? undefined : expect_string(object.rules, "ManagementResult.rules"),
    rules_explanation: object.rules_explanation === undefined ? undefined : expect_string(object.rules_explanation, "ManagementResult.rules_explanation"),
    rules_version: object.rules_version === undefined ? undefined : expect_string(object.rules_version, "ManagementResult.rules_version"),
    unconfirmed: object.unconfirmed === undefined ? undefined : expect_array(object.unconfirmed, "ManagementResult.unconfirmed").map((item) => expect_string(item, "ManagementResult.unconfirmed 的元素")),
  };
}

/** 核心Vault原因与恢复/投递状态；claim不等于用户已读或Windows实际展示。 */
export interface Notice {
  /** Vault原因，冲突副本为CONFLICT_COPY */
  cause: string;
  /** 本代际已领取系统投递 */
  claimed: boolean;
  /** 已分类错误码 */
  code: string;
  /** 可信核心分类需用户处理 */
  critical: boolean;
  /** 活动代际 */
  generation: string;
  /** 稳定去重键 */
  key: string;
  /** 关联服务器身份；全局或来源未知的历史通知省略 */
  server_id?: string;
  /** active或resolved */
  state: string;
  /** 关联Vault，非绑定身份 */
  vault_id?: string;
}

export function decodeNotice(value: unknown): Notice {
  const object = expect_object(value, "Notice");
  return {
    cause: expect_string(object.cause, "Notice.cause"),
    claimed: expect_boolean(object.claimed, "Notice.claimed"),
    code: expect_string(object.code, "Notice.code"),
    critical: expect_boolean(object.critical, "Notice.critical"),
    generation: expect_string(object.generation, "Notice.generation"),
    key: expect_string(object.key, "Notice.key"),
    server_id: object.server_id === undefined ? undefined : expect_string(object.server_id, "Notice.server_id"),
    state: expect_string(object.state, "Notice.state"),
    vault_id: object.vault_id === undefined ? undefined : expect_string(object.vault_id, "Notice.vault_id"),
  };
}

/** 真实持久问题；不返回任意日志文本。 */
export interface Problem {
  /** Binding 身份。 */
  binding_id: string;
  /** 稳定原因码。 */
  code: string;
  /** 相关条目。 */
  entry_id?: string;
  /** 本地数据保护/驻留情况，不把数据库 clean 当真实完整字节。 */
  local_data: string;
  /** 可执行下一步。 */
  next_step: string;
  /** 已核对的相对路径；未知时省略。 */
  path?: string;
  /** 待办操作身份或只读诊断身份。 */
  problem_id: string;
  /** 原因说明。 */
  reason: string;
  /** 是否可调用 retry_problem。 */
  retryable: boolean;
}

export function decodeProblem(value: unknown): Problem {
  const object = expect_object(value, "Problem");
  return {
    binding_id: expect_string(object.binding_id, "Problem.binding_id"),
    code: expect_string(object.code, "Problem.code"),
    entry_id: object.entry_id === undefined ? undefined : expect_string(object.entry_id, "Problem.entry_id"),
    local_data: expect_string(object.local_data, "Problem.local_data"),
    next_step: expect_string(object.next_step, "Problem.next_step"),
    path: object.path === undefined ? undefined : expect_string(object.path, "Problem.path"),
    problem_id: expect_string(object.problem_id, "Problem.problem_id"),
    reason: expect_string(object.reason, "Problem.reason"),
    retryable: expect_boolean(object.retryable, "Problem.retryable"),
  };
}

/** 服务器返回的授权 Vault、历史版本或回收站项目。 */
export interface RemoteItem {
  /** 冲突副本的原条目身份 */
  conflict_source?: string;
  /** 条目身份。 */
  entry_id?: string;
  /** 历史/回收站到期下限UTC秒，当前活跃版本省略 */
  expires_at?: string;
  /** 内容摘要。 */
  hash?: string;
  /** 稳定身份。 */
  id: string;
  /** 显示名称。 */
  name: string;
  /** 原父目录。 */
  parent_id?: string;
  /** 当前 CAS 修订。 */
  revision?: string;
  /** 字节数。 */
  size?: string;
  /** 固定版本。 */
  version_id?: string;
}

export function decodeRemoteItem(value: unknown): RemoteItem {
  const object = expect_object(value, "RemoteItem");
  return {
    conflict_source: object.conflict_source === undefined ? undefined : expect_string(object.conflict_source, "RemoteItem.conflict_source"),
    entry_id: object.entry_id === undefined ? undefined : expect_string(object.entry_id, "RemoteItem.entry_id"),
    expires_at: object.expires_at === undefined ? undefined : expect_string(object.expires_at, "RemoteItem.expires_at"),
    hash: object.hash === undefined ? undefined : expect_string(object.hash, "RemoteItem.hash"),
    id: expect_string(object.id, "RemoteItem.id"),
    name: expect_string(object.name, "RemoteItem.name"),
    parent_id: object.parent_id === undefined ? undefined : expect_string(object.parent_id, "RemoteItem.parent_id"),
    revision: object.revision === undefined ? undefined : expect_string(object.revision, "RemoteItem.revision"),
    size: object.size === undefined ? undefined : expect_string(object.size, "RemoteItem.size"),
    version_id: object.version_id === undefined ? undefined : expect_string(object.version_id, "RemoteItem.version_id"),
  };
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
