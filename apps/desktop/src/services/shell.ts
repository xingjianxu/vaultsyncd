import { invoke } from "@tauri-apps/api/core";
import { decodeBindingInfo, decodeFileStatus, decodeManagementResult, type BindingInfo, type FileStatus, type ManagementResult } from "@vaultsyncd/local-ipc-types";

export interface ShellTarget {
  action: "history" | "download" | "release";
  binding_id: string;
  entry_id: string;
  path: string;
}
export interface ShellFileStatus { file: FileStatus; binding: BindingInfo }
export interface ShellFileClient {
  target(): Promise<ShellTarget>;
  status(): Promise<ShellFileStatus>;
  action(operationId: string): Promise<ManagementResult>;
}
function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("文件操作应答无效，请重新打开右键菜单。");
  return value as Record<string, unknown>;
}
function decodeTarget(value: unknown): ShellTarget {
  const result = object(value);
  if ((result.action !== "history" && result.action !== "download" && result.action !== "release")
    || typeof result.binding_id !== "string" || !result.binding_id
    || typeof result.entry_id !== "string" || !result.entry_id
    || typeof result.path !== "string" || !result.path) {
    throw new Error("右键操作目标无效，请重新打开右键菜单。");
  }
  return { action: result.action, binding_id: result.binding_id, entry_id: result.entry_id, path: result.path };
}
async function call(command: string, args?: Record<string, unknown>): Promise<unknown> {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) throw new Error("请从 Windows 文件右键菜单打开此窗口。");
  try { return await invoke(command, args); }
  catch (error) {
    if (error instanceof Error) throw error;
    if (typeof error === "string") throw new Error(error);
    if (typeof error === "object" && error !== null && "message" in error && typeof error.message === "string") throw new Error(error.message);
    throw new Error("文件操作失败，请检查本机 agent。");
  }
}
// The native window owns its selection. JavaScript cannot choose an arbitrary file or action.
export const nativeShellFile: ShellFileClient = {
  target: async () => decodeTarget(await call("get_shell_target")),
  status: async () => {
    const result = object(await call("shell_file_status"));
    return { file: decodeFileStatus(result.file), binding: decodeBindingInfo(result.binding) };
  },
  action: async operationId => decodeManagementResult(await call("shell_file_action", { operationId })),
};
