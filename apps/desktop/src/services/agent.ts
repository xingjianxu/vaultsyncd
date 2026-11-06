import type { GetStatusResult } from "@vaultsyncd/local-ipc-types";

export interface AgentClient { getStatus(): Promise<GetStatusResult> }
// 仅供应用壳开发的明确替身；S080 替换为 Tauri command 桥，绝不返回伪造 Binding。
export const disconnectedAgent: AgentClient = {
  async getStatus() { throw new Error("未连接：当前为 IPC 测试替身，尚未接入本机 agent。"); },
};
