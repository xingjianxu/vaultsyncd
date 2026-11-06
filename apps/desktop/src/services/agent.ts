import type { GetStatusResult } from "@vaultsyncd/local-ipc-types";
import { invoke } from "@tauri-apps/api/core";

export interface AgentClient { getStatus(): Promise<GetStatusResult> }
// 仅供应用壳开发的明确替身；S080 替换为 Tauri command 桥，绝不返回伪造 Binding。
export const disconnectedAgent: AgentClient = {
  async getStatus() { throw new Error("未连接：当前为 IPC 测试替身，尚未接入本机 agent。"); },
};
// A fixed command with no caller-selected pipe name, path, shell or credential access.
export const nativeAgent: AgentClient = {
  async getStatus() {
    try { return await invoke<GetStatusResult>("agent_get_status"); }
    catch (error) {
      if (typeof error === "object" && error !== null && "message" in error) {
        throw new Error(String(error.message));
      }
      throw new Error("无法连接本机 agent；请从 Windows 桌面应用运行。");
    }
  },
};
