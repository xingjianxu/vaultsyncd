import type { GetStatusResult, ManagementResult } from "@vaultsyncd/local-ipc-types";
import { invoke } from "@tauri-apps/api/core";

export interface ConnectionInput { serverId: string; serverUrl: string; caFile?: string }
export interface BindingInput { bindingId: string; rootId: string; localPath: string; mode: "full" | "on_demand" }
export interface AgentClient {
  getStatus(): Promise<GetStatusResult>;
  setConnection(input: ConnectionInput): Promise<ManagementResult>;
  login(username: string, password: string): Promise<ManagementResult>;
  logout(): Promise<ManagementResult>;
  listRoots(): Promise<ManagementResult>;
  createBinding(input: BindingInput): Promise<ManagementResult>;
  pauseBinding(bindingId: string): Promise<ManagementResult>;
  resumeBinding(bindingId: string): Promise<ManagementResult>;
}
const unavailable = async (): Promise<never> => { throw new Error("未连接：当前为 IPC 测试替身，尚未接入本机 agent。"); };
// 仅供应用壳开发的明确替身；S080 替换为 Tauri command 桥，绝不返回伪造 Binding。
export const disconnectedAgent: AgentClient = {
  getStatus: unavailable, setConnection: unavailable, login: unavailable, logout: unavailable,
  listRoots: unavailable, createBinding: unavailable, pauseBinding: unavailable, resumeBinding: unavailable,
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
};
