// packages/local-ipc-types 的稳定导出入口（docs/design.md §5.6）。
//
// src/generated/ 由 `cargo xtask generate` 从 api/local-ipc.schema.json 生成，禁止手改；本文件
// 是手写的稳定边界：桌面应用只从这里取本机 IPC 的类型与解码器，生成代码的文件名或拆分方式
// 变化不会波及调用方。这里不包含任何传输实现（Tauri invoke、命名管道都在应用侧）。
export {
  COMMAND_NAME_VALUES,
  FRAME_HEADER_BYTES,
  LOCAL_IPC_ERROR_CODE_VALUES,
  MAX_FRAME_BYTES,
  PROTOCOL_MAJOR,
  PROTOCOL_MINOR,
  RESPONSE_KIND_VALUES,
  decodeCommand,
  decodeCommandName,
  decodeErrorBody,
  decodeGetStatusResult,
  decodeIpcVersion,
  decodeLocalIpcErrorCode,
  decodeRequest,
  decodeResponse,
  decodeResponseKind,
} from "./generated/types";
export type {
  Command,
  CommandName,
  ErrorBody,
  GetStatusResult,
  IpcVersion,
  LocalIpcErrorCode,
  Request,
  Response,
  ResponseKind,
} from "./generated/types";
