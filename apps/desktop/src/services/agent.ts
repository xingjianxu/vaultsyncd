import type { GetStatusResult, ManagementResult } from "@vaultsyncd/local-ipc-types";
import { invoke } from "@tauri-apps/api/core";

export interface ConnectionInput { serverId: string; serverUrl: string; caFile?: string }
export interface BindingInput { bindingId: string; rootId: string; localPath: string; mode: "full" | "on_demand" }
export interface RestoreInput { bindingId:string;entryId:string;operationId:string;baseRevision:string;versionId?:string }
export interface NotificationSettings { enabled:boolean;delivered:number;blocked:number;last_error?:string }
export interface AgentClient {
  getStatus(): Promise<GetStatusResult>;
  setConnection(input: ConnectionInput): Promise<ManagementResult>;
  login(username: string, password: string): Promise<ManagementResult>;
  logout(): Promise<ManagementResult>;
  listRoots(): Promise<ManagementResult>;
  createBinding(input: BindingInput): Promise<ManagementResult>;
  pauseBinding(bindingId: string): Promise<ManagementResult>;
  resumeBinding(bindingId: string): Promise<ManagementResult>;
  listFiles(bindingId: string, cursor?: string): Promise<ManagementResult>;
  downloadEntry(bindingId: string, entryId: string, operationId: string): Promise<ManagementResult>;
  releaseEntry(bindingId: string, entryId: string, operationId: string): Promise<ManagementResult>;
  listProblems(bindingId?: string, search?: string): Promise<ManagementResult>;
  retryProblem(bindingId: string, operationId: string): Promise<ManagementResult>;
  setMode(bindingId: string, mode: "full" | "on_demand"): Promise<ManagementResult>;
  detachBinding(bindingId: string): Promise<ManagementResult>;
  exportRecovery(bindingId: string, destination: string): Promise<ManagementResult>;
  listHistory(bindingId:string,entryId:string,cursor?:string):Promise<ManagementResult>;
  listTrash(bindingId:string,cursor?:string):Promise<ManagementResult>;
  restoreEntry(input:RestoreInput):Promise<ManagementResult>;
  notificationSettings():Promise<NotificationSettings>;
  setNotifications(enabled:boolean):Promise<NotificationSettings>;
}
const unavailable = async (): Promise<never> => { throw new Error("未连接：当前为 IPC 测试替身，尚未接入本机 agent。"); };
// 仅供应用壳开发的明确替身；S080 替换为 Tauri command 桥，绝不返回伪造 Binding。
export const disconnectedAgent: AgentClient = {
  getStatus: unavailable, setConnection: unavailable, login: unavailable, logout: unavailable,
  listRoots: unavailable, createBinding: unavailable, pauseBinding: unavailable, resumeBinding: unavailable,
  listFiles: unavailable, downloadEntry: unavailable, releaseEntry: unavailable, listProblems: unavailable,
  retryProblem: unavailable, setMode: unavailable, detachBinding: unavailable, exportRecovery: unavailable,
  listHistory:unavailable,listTrash:unavailable,restoreEntry:unavailable,notificationSettings:unavailable,setNotifications:unavailable,
};
async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try { return await invoke<T>(command, args); }
  catch (error) {
    if (typeof error === "object" && error !== null && "message" in error && typeof error.message === "string") throw new Error(error.message);
    throw new Error("无法连接本机 agent；请从 Windows 桌面应用运行。");
  }
}
// A fixed command with no caller-selected pipe name, path, shell or credential access.
export const nativeAgent: AgentClient = {
  getStatus: () => call("agent_get_status"),
  setConnection: input => call("agent_set_connection", { input }),
  login: (username, password) => call("agent_login", { username, password }),
  logout: () => call("agent_logout"),
  listRoots: () => call("agent_list_roots"),
  createBinding: input => call("agent_create_binding", { input }),
  pauseBinding: bindingId => call("agent_pause_binding", { bindingId }),
  resumeBinding: bindingId => call("agent_resume_binding", { bindingId }),
  listFiles: (bindingId, cursor) => call("agent_list_files", { bindingId, cursor }),
  downloadEntry: (bindingId, entryId, operationId) => call("agent_download_entry", { bindingId, entryId, operationId }),
  releaseEntry: (bindingId, entryId, operationId) => call("agent_release_entry", { bindingId, entryId, operationId }),
  listProblems: (bindingId, search) => call("agent_list_problems", { bindingId, search }),
  retryProblem: (bindingId, operationId) => call("agent_retry_problem", { bindingId, operationId }),
  setMode: (bindingId, mode) => call("agent_set_mode", { bindingId, mode }),
  detachBinding: bindingId => call("agent_detach_binding", { bindingId }),
  exportRecovery: (bindingId, destination) => call("agent_export_recovery", { bindingId, destination }),
  listHistory:(bindingId,entryId,cursor)=>call("agent_list_history",{bindingId,entryId,cursor}),
  listTrash:(bindingId,cursor)=>call("agent_list_trash",{bindingId,cursor}),
  restoreEntry:input=>call("agent_restore_entry",{input}),
  notificationSettings:()=>call("agent_notification_settings"),
  setNotifications:enabled=>call("agent_set_notifications",{enabled}),
};
