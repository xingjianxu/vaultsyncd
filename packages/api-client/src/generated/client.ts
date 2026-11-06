// 本文件由 `cargo xtask generate` 从 `api/openapi.yaml` 生成，禁止手改。
//
// 契约：vaultsyncd remote API 0.1.0；生成器：vaultsyncd-xtask 0.1.0
// 服务端远端接口的首发草案（/v1）。JSON 使用 snake_case；所有清单分页有上限；未知枚举
// 不当作默认成功。健康探测与能力协商不需要身份认证，其余端点需要会话，按 design §15.2
// 在对应功能卡加入。
//
// 传输细节（fetch、request_id）由调用方注入，客户端不直接读全局对象；非 2xx 响应按错误
// 信封解码成 ApiError，未知错误码与畸形响应抛 ProtocolError。

import { decodeCapabilities, decodeChangeList, decodeErrorEnvelope, decodeFinalizedContent, decodeHealthResponse, decodeOperationResult, decodeRecoveryPage, decodeRootList, decodeRootRegistered, decodeSessionIssued, decodeSessionRevoked, decodeSnapshot, decodeSnapshotPage, decodeSubtreeToken, decodeUploadPartAck, decodeUploadSession, decodeUploadStatus, decodeVersionLease } from "./types";
import type { Capabilities, ChangeList, ErrorBody, FinalizedContent, HealthResponse, LoginRequest, OperationRequest, OperationResult, RecoveryPage, RefreshRequest, RootList, RootRegisterRequest, RootRegistered, SessionIssued, SessionRevoked, Snapshot, SnapshotPage, SubtreeToken, UploadPartAck, UploadRequest, UploadSession, UploadStatus, VersionLease, VersionLeaseRequest } from "./types";

/** 注入的 fetch 形状：浏览器/Node 的全局 fetch 或测试替身。 */
export type FetchLike = (url: string, init?: RequestInit) => Promise<Response>;

/** 一次请求需要的传输参数。 */
export interface ClientOptions {
  /** 服务端根地址，例如 https://vault.example:8443（结尾斜杠可有可无）。 */
  base_url: string;
  /** fetch 实现。 */
  fetch: FetchLike;
  /** 生成 x-request-id 头；每次调用必须给出新值，供服务端日志关联。 */
  request_id: () => string;
}

/** 服务端返回的标准错误信封（design §15.4）。 */
export class ApiError extends Error {
  /** HTTP 状态码。 */
  readonly status: number;
  /** 已解码的错误主体。 */
  readonly body: ErrorBody;

  constructor(status: number, body: ErrorBody) {
    super(`${body.code}: ${body.message}`);
    this.name = "ApiError";
    this.status = status;
    this.body = body;
  }
}

/** 响应不是契约允许的形状：未知枚举、字段缺失、非法十进制文本或非 JSON。 */
export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ProtocolError";
  }
}

function join_url(base_url: string, path: string): string {
  return `${base_url.replace(/\/$/, "")}${path}`;
}

function request_headers(
  options: ClientOptions,
  init: RequestInit | undefined,
  content_type: string | null,
  accept = "application/json",
): Headers {
  const headers = new Headers(init?.headers);
  headers.set("accept", accept);
  if (content_type !== null) {
    headers.set("content-type", content_type);
  }
  headers.set("x-request-id", options.request_id());
  return headers;
}

async function read_json(response: Response): Promise<unknown> {
  try {
    return (await response.json()) as unknown;
  } catch {
    throw new ProtocolError(`响应不是 JSON（HTTP ${response.status}）`);
  }
}

async function read_bytes(response: Response): Promise<Uint8Array> {
  return new Uint8Array(await response.arrayBuffer());
}

function api_error(status: number, body: unknown): ApiError {
  return new ApiError(status, decodeErrorEnvelope(body).error);
}

/**
 * 注册 Root
 *
 * 注册一个服务端工作目录（design §11.4、§15.2）。只接受部署配置里预置的允许基目录内的
 * 普通目录；后端不是本地 btrfs（FUSE、占位/按需回源视图等）、路径与已注册 Root 或内部
 * 状态目录重叠、以及扫描内无法证明合规的目录一律拒绝。注册只读探测，不改写、不清空、
 * 不导入既有文件；初始采集是后续步骤，因此响应报告“待导入”。
 *
 * 200：已注册（既有文件未被改动）
 * 401：没有可用凭据
 * 403：路径不在允许基目录内，或没有管理权限
 * 422：目录不是可注册的普通存储（配置阶段拒绝）
 * 503：服务端存储不可用
 */
export async function registerRoot(options: ClientOptions, body: RootRegisterRequest, init?: RequestInit): Promise<RootRegistered> {
  const response = await options.fetch(join_url(options.base_url, "/v1/admin/roots"), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeRootRegistered(payload);
}

/**
 * 登录
 *
 * 用登录名与密码换一对凭据（design §15.2、§17.1）。失败一律 401，不区分“账号不存在”
 * 与“密码不对”；触发限流时 429。浏览器走同源 HttpOnly cookie，agent 把响应里的凭据
 * 存入 OS 凭据库或权限受限的凭据文件（见 docs/adr/0001-auth-credential-carriers.md）。
 * 响应不带服务器绝对路径。
 *
 * 200：登录成功，返回一对凭据
 * 401：凭据无效（不区分账号不存在与密码不对）
 * 403：跨源 cookie 写请求被拒绝
 * 429：登录尝试过于频繁
 */
export async function login(options: ClientOptions, body: LoginRequest, init?: RequestInit): Promise<SessionIssued> {
  const response = await options.fetch(join_url(options.base_url, "/v1/auth/login"), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeSessionIssued(payload);
}

/**
 * 注销当前会话
 *
 * 失效当前会话的凭据（design §15.2）。只动服务端会话，不清理任何本地文件；重复注销是
 * 幂等的。
 *
 * 200：会话已失效
 * 401：没有可用凭据
 */
export async function logout(options: ClientOptions, init?: RequestInit): Promise<SessionRevoked> {
  const response = await options.fetch(join_url(options.base_url, "/v1/auth/logout"), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeSessionRevoked(payload);
}

/**
 * 刷新会话凭据
 *
 * 用刷新凭据换新的一对凭据（design §15.2）。轮换是原子的：旧会话随即失效，同一个刷新
 * 凭据不能再用。被用过或已撤销的刷新凭据一律 401，不回放后继凭据。
 *
 * 200：轮换成功，返回新的凭据对
 * 401：刷新凭据无效、过期、已撤销或已被使用
 */
export async function refresh(options: ClientOptions, body: RefreshRequest, init?: RequestInit): Promise<SessionIssued> {
  const response = await options.fetch(join_url(options.base_url, "/v1/auth/refresh"), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeSessionIssued(payload);
}

/**
 * 能力协商
 *
 * 返回协议版本、服务器身份、epoch、功能集合与限额，供客户端显式协商。不泄露任何
 * 服务器目录路径。
 *
 * 200：当前协议版本与能力
 * 503：服务端暂不可用（例如需要恢复）
 */
export async function getCapabilities(options: ClientOptions, init?: RequestInit): Promise<Capabilities> {
  const response = await options.fetch(join_url(options.base_url, "/v1/capabilities"), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeCapabilities(payload);
}

/**
 * 健康探测
 *
 * 无需认证的存活与健康状态探测。服务端仍可用 503 报告需要恢复等不可用状态，
 * 此时响应体是标准错误信封。
 *
 * 200：服务端可接受请求
 * 503：服务端暂不可用（例如需要恢复）
 */
export async function getHealth(options: ClientOptions, init?: RequestInit): Promise<HealthResponse> {
  const response = await options.fetch(join_url(options.base_url, "/v1/health"), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeHealthResponse(payload);
}

/**
 * 授权 Root 清单
 *
 * 列出当前调用者可见的 Root 及其健康状态（design §15.2）。响应只有身份与状态：
 * 普通客户端不获得服务器绝对路径，管理面要用路径时走本机或管理员通道。
 *
 * 200：当前调用者的 Root 清单
 * 401：没有可用凭据
 * 503：服务端存储不可用
 */
export async function listRoots(options: ClientOptions, init?: RequestInit): Promise<RootList> {
  const response = await options.fetch(join_url(options.base_url, "/v1/roots"), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeRootList(payload);
}

/**
 * 拉取有序事务批次
 *
 * 返回 `after` 之后（不含 `after`）的事务批次，严格按 RootSeq 升序（design §9.3）。每个批次是
 * 一个完整事务：`limit` 只决定在哪个事务边界截断，**绝不把一个事务拆到两页之间**——如果第
 * `limit` 个批次与后续批次属于同一个 `tx_id` 且 seq 连续，本页会延伸到把它们取完。
 *
 * `after=0` 表示从头开始；新客户端通常用快照的 `high_water_seq` 作为 `after`。`has_more` 为真
 * 时用 `next_after` 继续拉取。重复拉取同一游标得到同样的批次（拉取没有副作用），客户端按
 * seq 去重即可。
 *
 * 游标早于事件保留窗口（默认 90 天）或晚于服务端当前头时返回 410 `CURSOR_EXPIRED`，
 * **不返回空列表**：`details.reason` 为 `before_retention_window` 或 `ahead_of_head`，
 * 客户端重新获取快照并保留本地基线与未上传队列（design §9.4）。
 *
 * 参数 `root`（/v1/roots/{root}/changes）：授权 Root 的身份（32 位小写十六进制）。
 * 查询参数 `after`（必填）：已处理到的 RootSeq（十进制文本；0 表示从头）。
 * 查询参数 `limit`（可选）：本页至少包含的批次数上限（1 至 500，缺省 100）；越界是 400。
 *
 * 200：有序事务批次（可能为空，空表示已追平到读取时的头）
 * 400：`after` 缺失或不是规范十进制，或 limit 越界
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 410：游标已失效（`CURSOR_EXPIRED`），需要重新获取快照
 * 503：服务端存储不可用或需要恢复
 */
export async function listChanges(options: ClientOptions, root: string, query: { after: string; limit?: string }, init?: RequestInit): Promise<ChangeList> {
  const path = `/v1/roots/${encodeURIComponent(root)}/changes`;
  const search = new URLSearchParams();
  search.set("after", query.after);
  if (query.limit !== undefined) {
    search.set("limit", query.limit);
  }
  const query_string = search.toString();
  const target = query_string === "" ? path : `${path}?${query_string}`;
  const response = await options.fetch(join_url(options.base_url, target), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeChangeList(payload);
}

/**
 * 列出条目历史版本
 *
 * 需要 Root 授权；按版本身份分页，最多100条，不泄露服务器路径。
 *
 * 参数 `root`（/v1/roots/{root}/entries/{entry}/history）：路径参数
 * 参数 `entry`（/v1/roots/{root}/entries/{entry}/history）：路径参数
 * 查询参数 `after`（可选）：查询参数
 *
 * 200：一页历史
 * 400：参数错误
 * 403：没有 Root 授权
 * 404：条目不存在
 */
export async function listEntryHistory(options: ClientOptions, root: string, entry: string, query?: { after?: string }, init?: RequestInit): Promise<RecoveryPage> {
  const path = `/v1/roots/${encodeURIComponent(root)}/entries/${encodeURIComponent(entry)}/history`;
  const search = new URLSearchParams();
  if (query?.after !== undefined) {
    search.set("after", query.after);
  }
  const query_string = search.toString();
  const target = query_string === "" ? path : `${path}?${query_string}`;
  const response = await options.fetch(join_url(options.base_url, target), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeRecoveryPage(payload);
}

/**
 * 取目录子树的服务端 token
 *
 * 取一个目录条目的冻结子树清单与 token（design §10.3、§11.2、T12）。目录删除必须携带
 * 这个 token：从看到清单到发出删除之间，子树任何新增/改名/删改都会让 token 变化，删除因此
 * 返回 409 SUBTREE_CHANGED 而不是删掉并发新增。token 不是安全凭据，只是乐观并发证明。
 *
 * 对普通文件返回 422 UNSUPPORTED_ENTRY（文件删除不需要 token）。
 *
 * 参数 `root`（/v1/roots/{root}/entries/{entry}/subtree）：授权 Root 的身份（32 位小写十六进制）。
 * 参数 `entry`（/v1/roots/{root}/entries/{entry}/subtree）：目录条目身份（32 位小写十六进制）。
 *
 * 200：子树清单与 token
 * 401：没有可用凭据
 * 403：Root 不在授权范围
 * 404：条目不存在或已删除
 * 422：条目不是目录（文件删除不需要 token）
 * 503：存储故障或 Root 暂不可写
 */
export async function getSubtreeToken(options: ClientOptions, root: string, entry: string, init?: RequestInit): Promise<SubtreeToken> {
  const path = `/v1/roots/${encodeURIComponent(root)}/entries/${encodeURIComponent(entry)}/subtree`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeSubtreeToken(payload);
}

/**
 * 下载固定版本的内容
 *
 * 按 `(root, entry, version)` 取该版本的**固定**内容字节（design §15.2、I12、I15）：内容地址
 * 只来自那条版本行，路径上后来出现的新 head 与本请求无关。
 *
 * 整份请求返回 **200**，带 `Content-Length` 与整份内容的 `x-content-sha256`。请求头
 * `Range: bytes=start-end` / `bytes=start-` / `bytes=-suffix` 请求部分内容时返回 **206**
 * 与 `Content-Range: bytes start-end/total`。多区间、非 `bytes=` 形式、反向区间与越界
 * 区间一律 400；空对象上的任何区间不可满足。部分切片无法在服务端核对整份摘要
 * （design §13.2 的整文件校验在客户端）。
 *
 * 版本已过期或已清理时返回 410 VERSION_GONE，绝不回退成“同路径的当前内容”。每次读取
 * 前建立内容租约，读取结束才解除，因此历史对象不会在读取期间被 GC 回收。
 *
 * Range 是可选请求头，由调用方经 `init.headers` 传入（可选头不生成函数参数）。
 *
 * 参数 `root`（/v1/roots/{root}/entries/{entry}/versions/{version}/content）：授权 Root 的身份（32 位小写十六进制）。
 * 参数 `entry`（/v1/roots/{root}/entries/{entry}/versions/{version}/content）：条目身份（32 位小写十六进制）。
 * 参数 `version`（/v1/roots/{root}/entries/{entry}/versions/{version}/content）：版本身份（32 位小写十六进制）。
 *
 * 200：整份内容字节（`application/octet-stream`）
 * 206：请求区间的字节，`Content-Range` 给出实际区间
 * 400：Range 头语法不合法，或区间落在对象之外
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 410：版本已过期、已清理，或不属于该条目/Root（不泄露是否存在）
 * 503：服务端不可用（对象读取或租约失败、需要恢复）
 */
export async function downloadVersionContent(options: ClientOptions, root: string, entry: string, version: string, init?: RequestInit): Promise<Uint8Array> {
  const path = `/v1/roots/${encodeURIComponent(root)}/entries/${encodeURIComponent(entry)}/versions/${encodeURIComponent(version)}/content`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/octet-stream"),
  });
  if (!response.ok) {
    throw api_error(response.status, await read_json(response));
  }
  return await read_bytes(response);
}

/**
 * 建立固定版本短租约
 *
 * S069：授权后在同一写事务核对固定版本和live对象并记录object_leases GC保护。
 * TTL由服务器固定为120秒；同lease_id仅允许同Root/Entry/Version/摘要/长度。
 * acquire重放不延长期限；renew只续未过期租约；release幂等，过期或已解除不得复活。
 * 不可读对象或无法确认租约时拒绝释放本地内容。请求仅用于已认证调用。
 *
 * 参数 `root`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases）：路径参数
 * 参数 `entry`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases）：路径参数
 * 参数 `version`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases）：路径参数
 *
 * 200：固定版本租约事实
 * 400：非法请求
 * 401：没有可用会话
 * 403：Root越权
 * 409：租约ID复用不同请求
 * 410：版本或租约已失效
 * 503：存储或对象不可用
 */
export async function acquireVersionLease(options: ClientOptions, root: string, entry: string, version: string, body: VersionLeaseRequest, init?: RequestInit): Promise<VersionLease> {
  const path = `/v1/roots/${encodeURIComponent(root)}/entries/${encodeURIComponent(entry)}/versions/${encodeURIComponent(version)}/leases`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeVersionLease(payload);
}

/**
 * 解除固定版本短租约
 *
 * S069：授权后在同一写事务核对固定版本和live对象并记录object_leases GC保护。
 * TTL由服务器固定为120秒；同lease_id仅允许同Root/Entry/Version/摘要/长度。
 * acquire重放不延长期限；renew只续未过期租约；release幂等，过期或已解除不得复活。
 * 不可读对象或无法确认租约时拒绝释放本地内容。请求仅用于已认证调用。
 *
 * 参数 `root`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/release）：路径参数
 * 参数 `entry`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/release）：路径参数
 * 参数 `version`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/release）：路径参数
 * 参数 `lease`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/release）：路径参数
 *
 * 200：固定版本租约事实
 * 400：非法请求
 * 401：没有可用会话
 * 403：Root越权
 * 409：租约ID复用不同请求
 * 410：版本或租约已失效
 * 503：存储或对象不可用
 */
export async function releaseVersionLease(options: ClientOptions, root: string, entry: string, version: string, lease: string, body: VersionLeaseRequest, init?: RequestInit): Promise<VersionLease> {
  const path = `/v1/roots/${encodeURIComponent(root)}/entries/${encodeURIComponent(entry)}/versions/${encodeURIComponent(version)}/leases/${encodeURIComponent(lease)}/release`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeVersionLease(payload);
}

/**
 * 续期固定版本短租约
 *
 * S069：授权后在同一写事务核对固定版本和live对象并记录object_leases GC保护。
 * TTL由服务器固定为120秒；同lease_id仅允许同Root/Entry/Version/摘要/长度。
 * acquire重放不延长期限；renew只续未过期租约；release幂等，过期或已解除不得复活。
 * 不可读对象或无法确认租约时拒绝释放本地内容。请求仅用于已认证调用。
 *
 * 参数 `root`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/renew）：路径参数
 * 参数 `entry`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/renew）：路径参数
 * 参数 `version`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/renew）：路径参数
 * 参数 `lease`（/v1/roots/{root}/entries/{entry}/versions/{version}/leases/{lease}/renew）：路径参数
 *
 * 200：固定版本租约事实
 * 400：非法请求
 * 401：没有可用会话
 * 403：Root越权
 * 409：租约ID复用不同请求
 * 410：版本或租约已失效
 * 503：存储或对象不可用
 */
export async function renewVersionLease(options: ClientOptions, root: string, entry: string, version: string, lease: string, body: VersionLeaseRequest, init?: RequestInit): Promise<VersionLease> {
  const path = `/v1/roots/${encodeURIComponent(root)}/entries/${encodeURIComponent(entry)}/versions/${encodeURIComponent(version)}/leases/${encodeURIComponent(lease)}/renew`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeVersionLease(payload);
}

/**
 * SSE 唤醒提示
 *
 * `text/event-stream` 长连接（design §9.3）。**SSE 只是提示，不是数据来源**：事件只说“Root 的
 * RootSeq 变了”，不携带批次内容；客户端据此立刻调用 `changes` 拉取。
 *
 * 每条提示是 `event: seq`，`id` 与 `data`（`SeqHint` 的 JSON）都带当前头。新连接立即收到一条
 * 当前头的提示；之后头每变化一次提示一次（可能跳号合并）。服务端另发保活注释行。提示可能
 * 丢失、重复或乱序（断线重连、代理缓冲），客户端不得依赖它的次序或次数——凭 `changes` 的游标
 * 与 seq 去重修复，断线期间只需轮询 `changes` 即可恢复。
 *
 * 认证与其他端点相同（`Authorization` 头），因此浏览器原生 `EventSource` 不能直接使用，
 * 需用 fetch 流式读取。
 *
 * 参数 `root`（/v1/roots/{root}/events）：授权 Root 的身份（32 位小写十六进制）。
 *
 * 200：事件流（`text/event-stream`），`data` 为 `SeqHint` 的 JSON
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 503：服务端存储不可用或需要恢复
 */
export async function subscribeEvents(options: ClientOptions, root: string, init?: RequestInit): Promise<Response> {
  const path = `/v1/roots/${encodeURIComponent(root)}/events`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "text/event-stream"),
  });
  if (!response.ok) {
    throw api_error(response.status, await read_json(response));
  }
  return response;
}

/**
 * 提交一次修改操作
 *
 * 提交 Put/Create/Move/Delete/Restore 命令之一（design §15.2、§15.3）。命令只引用已上传并
 * 校验的内容（`upload_id`），不携带字节；服务端按 `(root, device_id, op_id)` 做幂等
 * （I06）：
 *
 * - 首次出现的 OpId 被登记为 `pending`，命令体随后执行；
 * - 同一 OpId 与同一语义再次出现是**重放**：返回记录里的同一结果（`replayed=true`），不重做；
 * - 同一 OpId 用于不同语义是协议错误 `OP_ID_REUSED`（409），绝不用旧结果回答新请求；
 * - `server_epoch` 不是 Root 当前 epoch 时 410 `EPOCH_CHANGED`，客户端进入保留基线的重建流程；
 * - 过期基线的 CAS/子树冲突按 design §15.4 返回 409（`BASE_MISMATCH`/`SUBTREE_CHANGED`/
 *   `RECONCILE_REQUIRED`）；`preserve_both` 下能提交的过期修改返回 committed 并带 `conflict`。
 *
 * 响应是本次操作的状态：`committed`/`rejected` 用 200（终态结果稳定），**`pending` 用 202**，
 * 且 `pending` 不携带 `revision`/`root_seq`/`version_id`/`entry_id`——已受理不等于已同步
 * （I01）。响应丢失后的重试必须得到同一份结果。
 *
 * 本服务端的命令执行链路尚未装配时（见 503 `SERVICE_NOT_READY`），请求不被受理：不登记
 * `operations` 行、不追加事件批次、不返回 202。
 *
 * 参数 `root`（/v1/roots/{root}/operations）：授权 Root 的身份（32 位小写十六进制）。
 *
 * 200：操作已有终态结果（committed 或 rejected）
 * 202：操作已受理但还没有业务结果（pending，不判已同步）
 * 400：请求形状不成立（缺少该命令的前提字段、混用两套命令语义、身份文本非法）
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 409：同一 OpId 不同请求（`OP_ID_REUSED`），或条目基线/子树已过期（`BASE_MISMATCH`、
`SUBTREE_CHANGED`、`RECONCILE_REQUIRED`）
 * 410：请求的服务器 epoch 已过期（`EPOCH_CHANGED`），或恢复所需的历史/回收站内容已过保留期
（`VERSION_GONE`，不拿同路径的新内容冒充）
 * 422：名称不可用（`INVALID_NAME`）或条目种类不支持该命令（`UNSUPPORTED_ENTRY`）
 * 503：Root 当前不可写（`ROOT_UNAVAILABLE`）、存储需要恢复（`NEEDS_RECOVERY`），或本服务端尚未
装配命令执行器（`SERVICE_NOT_READY`，请求未被受理）
 * 507：服务端空间不足（`INSUFFICIENT_STORAGE`），事务未提交
 */
export async function submitOperation(options: ClientOptions, root: string, body: OperationRequest, init?: RequestInit): Promise<OperationResult> {
  const path = `/v1/roots/${encodeURIComponent(root)}/operations`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeOperationResult(payload);
}

/**
 * 查询一次操作的结果
 *
 * 按 OpId 取操作结果，用于响应丢失与重启后的恢复（design §15.2）。同一记录的回答与提交端点
 * 一致：`committed`/`rejected` 用 200 带终态结果，**`pending` 用 202** 且不含修订与序号。
 *
 * 查询不要求 `server_epoch`：客户端要先知道结果才可能决定是否重建，因此 epoch 变化不影响
 * 读取既有记录。记录不存在、不属于该 Root 或不属于该登录会话都按同一 404 回答，不泄露是否存在。
 *
 * 参数 `root`（/v1/roots/{root}/operations/{op}）：授权 Root 的身份（32 位小写十六进制）。
 * 参数 `op`（/v1/roots/{root}/operations/{op}）：客户端生成的逻辑操作身份（32 位小写十六进制）。
 *
 * 200：操作已有终态结果（committed 或 rejected）
 * 202：操作仍是 pending（已受理、没有业务结果，不判已同步）
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 404：操作记录不存在、不属于该 Root 或不属于该登录会话
 * 503：服务端存储不可用或需要恢复
 */
export async function getOperation(options: ClientOptions, root: string, op: string, init?: RequestInit): Promise<OperationResult> {
  const path = `/v1/roots/${encodeURIComponent(root)}/operations/${encodeURIComponent(op)}`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeOperationResult(payload);
}

/**
 * 创建一致性快照
 *
 * 生成一份短期只读的元数据清单（design §9.3）：服务端在同一个事务里读取 Root 当前的
 * `RootSeq`（`high_water_seq`）并复制当时的活跃条目。之后 `…/snapshots/{snapshot}/entries`
 * 的翻页只读这份清单，与翻页期间的新增/删除/改名无关；客户端应用清单后从
 * `changes?after=<high_water_seq>` 继续追赶。
 *
 * 清单只存活 `expires_at` 之前；每个 Root 同时存在的快照数有上限，超出时最旧的先被回收。
 * 下载文件内容仍须引用 VersionId，而不是“当前路径”。
 *
 * 参数 `root`（/v1/roots/{root}/snapshots）：授权 Root 的身份（32 位小写十六进制）。
 *
 * 201：快照已创建
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 503：服务端存储不可用或需要恢复
 */
export async function createSnapshot(options: ClientOptions, root: string, init?: RequestInit): Promise<Snapshot> {
  const path = `/v1/roots/${encodeURIComponent(root)}/snapshots`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeSnapshot(payload);
}

/**
 * 翻页读取快照清单
 *
 * 只读固定的快照清单（design §9.3），按 EntryId 升序分页。`next_cursor` 缺省表示已是最后一页。
 *
 * 快照不存在、不属于该 Root 或已过期时返回 410 `CURSOR_EXPIRED`（`details.reason=snapshot_expired`），
 * **绝不返回空页或错乱数据**；客户端应重新创建快照并从头翻页。`limit` 超过上限是 400。
 *
 * 参数 `root`（/v1/roots/{root}/snapshots/{snapshot}/entries）：授权 Root 的身份（32 位小写十六进制）。
 * 参数 `snapshot`（/v1/roots/{root}/snapshots/{snapshot}/entries）：快照身份（32 位小写十六进制）。
 * 查询参数 `cursor`（可选）：起始序号（十进制文本）；首页省略，之后用上一页的 `next_cursor`。
 * 查询参数 `limit`（可选）：一页最多返回的条目数（1 至 1000，缺省 500）；越界是 400。
 *
 * 200：一页清单
 * 400：游标不是规范十进制、超过清单长度，或 limit 越界
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 410：快照不存在或已过期（`CURSOR_EXPIRED`），需要重新创建
 * 503：服务端存储不可用或需要恢复
 */
export async function listSnapshotEntries(options: ClientOptions, root: string, snapshot: string, query?: { cursor?: string; limit?: string }, init?: RequestInit): Promise<SnapshotPage> {
  const path = `/v1/roots/${encodeURIComponent(root)}/snapshots/${encodeURIComponent(snapshot)}/entries`;
  const search = new URLSearchParams();
  if (query?.cursor !== undefined) {
    search.set("cursor", query.cursor);
  }
  if (query?.limit !== undefined) {
    search.set("limit", query.limit);
  }
  const query_string = search.toString();
  const target = query_string === "" ? path : `${path}?${query_string}`;
  const response = await options.fetch(join_url(options.base_url, target), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeSnapshotPage(payload);
}

/**
 * 列出回收站
 *
 * 需要 Root 授权；按条目身份分页，最多100条，仅返回仍保留的回收站事实。
 *
 * 参数 `root`（/v1/roots/{root}/trash）：路径参数
 * 查询参数 `after`（可选）：查询参数
 *
 * 200：一页回收站
 * 400：参数错误
 * 403：没有 Root 授权
 */
export async function listTrash(options: ClientOptions, root: string, query?: { after?: string }, init?: RequestInit): Promise<RecoveryPage> {
  const path = `/v1/roots/${encodeURIComponent(root)}/trash`;
  const search = new URLSearchParams();
  if (query?.after !== undefined) {
    search.set("after", query.after);
  }
  const query_string = search.toString();
  const target = query_string === "" ? path : `${path}?${query_string}`;
  const response = await options.fetch(join_url(options.base_url, target), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeRecoveryPage(payload);
}

/**
 * 创建上传会话
 *
 * 为一个不可变本地快照申请上传会话（design §13.1—13.2、§15.2）。会话绑定总大小与总
 * SHA-256，块大小由服务端决定并随会话返回；同一会话重复申请不会创建第二个身份。
 * 会话与签发它的登录会话、授权 Root 绑定，容量与总大小上限由服务端限定。
 *
 * 参数 `root`（/v1/roots/{root}/uploads）：授权 Root 的身份（32 位小写十六进制）。
 *
 * 201：已创建上传会话
 * 400：请求形状不合法（哈希不是 64 位小写十六进制、大小越界）
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围（不区分不存在与越权）
 * 503：服务端不可用（例如需要恢复）
 * 507：服务端空间或会话容量不足
 */
export async function beginUpload(options: ClientOptions, root: string, body: UploadRequest, init?: RequestInit): Promise<UploadSession> {
  const path = `/v1/roots/${encodeURIComponent(root)}/uploads`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, "application/json", "application/json"),
    body: JSON.stringify(body),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeUploadSession(payload);
}

/**
 * 查询上传进度
 *
 * 返回会话状态与**服务端真实已收块**，客户端的续传依据是这里的结果而不是自己记录的位图
 * （design §13.2「查询已收块」）。已收块的哈希与长度都来自持久化记录。
 *
 * 参数 `root`（/v1/roots/{root}/uploads/{upload}）：授权 Root 的身份（32 位小写十六进制）。
 * 参数 `upload`（/v1/roots/{root}/uploads/{upload}）：上传会话身份（32 位小写十六进制）。
 *
 * 200：会话状态与已收块
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 404：会话不存在、不属于该 Root 或不属于该登录会话
 * 503：服务端不可用（例如需要恢复）
 */
export async function getUpload(options: ClientOptions, root: string, upload: string, init?: RequestInit): Promise<UploadStatus> {
  const path = `/v1/roots/${encodeURIComponent(root)}/uploads/${encodeURIComponent(upload)}`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "GET",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeUploadStatus(payload);
}

/**
 * 完成上传并发布内容
 *
 * 按块几何核对收齐后拼装并发布内容对象（design §13.1—13.2、§15.2）。顺序固定为
 * “核对块齐 → 标 complete → 拼装并发布对象 → 核对总长度与总 SHA-256 → 登记对象 →
 * 标 finalized”。
 *
 * 本端点**不提交条目**：它只返回可提交内容引用（`hash`/`size`/`storage_path`），条目的
 * head 由后续 put/create 操作按 CAS 提交（I01：finalize 成功不等于同步完成）。
 *
 * 块未收齐时不改会话状态，直接 400；拼装内容与快照声明不符时放弃会话
 * （`aborted`）并返回 422，不登记对象（I05）。已 `finalized` 的会话重复调用是幂等重放：
 * 返回同一内容引用与 `replayed=true`，不重新发布。
 *
 * 参数 `root`（/v1/roots/{root}/uploads/{upload}/finalize）：授权 Root 的身份（32 位小写十六进制）。
 * 参数 `upload`（/v1/roots/{root}/uploads/{upload}/finalize）：上传会话身份（32 位小写十六进制）。
 *
 * 200：内容已发布（首次或幂等重放），返回可提交内容引用
 * 400：身份/形状不合法，或块还没有收齐（不改会话状态）
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围
 * 404：会话不存在、不属于该 Root 或不属于该登录会话
 * 409：会话已被放弃（不再接收块，也不能 finalize）
 * 422：拼装内容与声明的总长度或总 SHA-256 不符（会话被放弃）
 * 503：服务端不可用（存储失败或需要恢复）
 * 507：对象区空间不足，对象未发布
 */
export async function finalizeUpload(options: ClientOptions, root: string, upload: string, init?: RequestInit): Promise<FinalizedContent> {
  const path = `/v1/roots/${encodeURIComponent(root)}/uploads/${encodeURIComponent(upload)}/finalize`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "POST",
    headers: request_headers(options, init, null, "application/json"),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeFinalizedContent(payload);
}

/**
 * 上传一个固定块
 *
 * 按会话的块几何上传第 `index` 块原始字节（design §13.1、§15.2）。请求体是
 * `application/octet-stream`；块长度必须等于会话几何给出的期望长度，`x-part-sha256`
 * 头给出该块的小写十六进制 SHA-256。服务端先把字节落到私有暂存并持久化，再登记该块；
 * 同一索引重传必须字节一致（幂等返回既有记录），不一致一律拒绝，不能把两个本地
 * generation 的字节混进一个会话。
 *
 * 参数 `root`（/v1/roots/{root}/uploads/{upload}/parts/{index}）：授权 Root 的身份（32 位小写十六进制）。
 * 参数 `upload`（/v1/roots/{root}/uploads/{upload}/parts/{index}）：上传会话身份（32 位小写十六进制）。
 * 参数 `index`（/v1/roots/{root}/uploads/{upload}/parts/{index}）：块索引（从 0 起的十进制文本）。
 * 请求头 `x-part-sha256`：该块原始字节的 SHA-256（64 位小写十六进制），与该块字节分开传：服务端先核对再登记。
 *
 * 200：块已登记（首次或幂等重传）
 * 400：块索引越界、长度与几何不符或载荷超过上限
 * 401：没有可用会话凭据
 * 403：Root 不在调用方授权范围或会话属于其他登录会话
 * 404：会话不存在或不属于该 Root
 * 409：同一块索引已登记不同字节
 * 422：块内容与声明的 SHA-256 或长度不符
 * 503：服务端不可用（例如需要恢复）
 * 507：暂存空间不足
 */
export async function putUploadPart(options: ClientOptions, root: string, upload: string, index: string, x_part_sha256: string, body: Uint8Array, init?: RequestInit): Promise<UploadPartAck> {
  const path = `/v1/roots/${encodeURIComponent(root)}/uploads/${encodeURIComponent(upload)}/parts/${encodeURIComponent(index)}`;
  const headers = request_headers(options, init, "application/octet-stream", "application/json");
  headers.set("x-part-sha256", x_part_sha256);
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "PUT",
    headers,
    body: body as BodyInit,
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeUploadPartAck(payload);
}
