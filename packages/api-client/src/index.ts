// packages/api-client 的稳定导出入口（docs/design.md §5.6）。
//
// src/generated/ 由 `cargo xtask generate` 从 api/openapi.yaml 生成，禁止手改；本文件是手写的
// 稳定边界：Web 应用只从这里取类型与调用函数，生成代码的文件名/拆分方式变化不会波及调用方。
export { ApiError, ProtocolError, getCapabilities, getHealth } from "./generated/client";
export type { ClientOptions, FetchLike } from "./generated/client";
export {
  decodeCapabilities,
  decodeErrorCode,
  decodeErrorEnvelope,
  decodeHealthResponse,
  decodeHealthStatus,
} from "./generated/types";
export type {
  Capabilities,
  CapabilityLimits,
  ErrorBody,
  ErrorCode,
  ErrorEnvelope,
  HealthResponse,
  HealthStatus,
  ProtocolVersion,
} from "./generated/types";
