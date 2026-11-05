// 本文件由 `cargo xtask generate` 从 `api/openapi.yaml` 生成，禁止手改。
//
// 契约：vaultsyncd remote API 0.1.0；生成器：vaultsyncd-xtask 0.1.0
// 服务端远端接口的首发草案（/v1）。JSON 使用 snake_case；所有清单分页有上限；未知枚举
// 不当作默认成功。健康探测与能力协商不需要身份认证，其余端点需要会话，按 design §15.2
// 在对应功能卡加入。
//
// 传输细节（fetch、request_id）由调用方注入，客户端不直接读全局对象；非 2xx 响应按错误
// 信封解码成 ApiError，未知错误码与畸形响应抛 ProtocolError。

import { decodeCapabilities, decodeErrorEnvelope, decodeHealthResponse, decodeRootList, decodeRootRegistered, decodeSessionIssued, decodeSessionRevoked, decodeUploadPartAck, decodeUploadSession, decodeUploadStatus } from "./types";
import type { Capabilities, ErrorBody, HealthResponse, LoginRequest, RefreshRequest, RootList, RootRegisterRequest, RootRegistered, SessionIssued, SessionRevoked, UploadPartAck, UploadRequest, UploadSession, UploadStatus } from "./types";

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
): Headers {
  const headers = new Headers(init?.headers);
  headers.set("accept", "application/json");
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
    headers: request_headers(options, init, "application/json"),
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
    headers: request_headers(options, init, "application/json"),
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
    headers: request_headers(options, init, null),
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
    headers: request_headers(options, init, "application/json"),
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
    headers: request_headers(options, init, null),
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
    headers: request_headers(options, init, null),
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
    headers: request_headers(options, init, null),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeRootList(payload);
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
    headers: request_headers(options, init, "application/json"),
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
    headers: request_headers(options, init, null),
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeUploadStatus(payload);
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
export async function putUploadPart(options: ClientOptions, root: string, upload: string, index: string, body: Uint8Array, init?: RequestInit): Promise<UploadPartAck> {
  const path = `/v1/roots/${encodeURIComponent(root)}/uploads/${encodeURIComponent(upload)}/parts/${encodeURIComponent(index)}`;
  const response = await options.fetch(join_url(options.base_url, path), {
    ...init,
    method: "PUT",
    headers: request_headers(options, init, "application/octet-stream"),
    body: body as BodyInit,
  });
  const payload = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, payload);
  }
  return decodeUploadPartAck(payload);
}
