// 本文件由 `cargo xtask generate` 从 `api/openapi.yaml` 生成，禁止手改。
//
// 契约：vaultsyncd remote API 0.1.0；生成器：vaultsyncd-xtask 0.1.0
// 服务端远端接口的首发草案（/v1）。JSON 使用 snake_case；所有清单分页有上限；未知枚举
// 不当作默认成功。健康探测与能力协商不需要身份认证，其余端点需要会话，按 design §15.2
// 在对应功能卡加入。
//
// 传输细节（fetch、request_id）由调用方注入，客户端不直接读全局对象；非 2xx 响应按错误
// 信封解码成 ApiError，未知错误码与畸形响应抛 ProtocolError。

import { decodeCapabilities, decodeErrorEnvelope, decodeHealthResponse } from "./types";
import type { Capabilities, ErrorBody, HealthResponse } from "./types";

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

function request_headers(options: ClientOptions, init?: RequestInit): Headers {
  const headers = new Headers(init?.headers);
  headers.set("accept", "application/json");
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
    headers: request_headers(options, init),
  });
  const body = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, body);
  }
  return decodeCapabilities(body);
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
    headers: request_headers(options, init),
  });
  const body = await read_json(response);
  if (!response.ok) {
    throw api_error(response.status, body);
  }
  return decodeHealthResponse(body);
}
