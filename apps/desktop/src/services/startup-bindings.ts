import type { BindingInfo } from "@vaultsyncd/local-ipc-types";
import { AgentRequestError, type AgentClient } from "./agent";

// Only the main desktop's first complete snapshot is eligible. Polling and navigation
// must never undo a pause requested later in the same desktop session.
export async function restoreStartupBindings(agent: Pick<AgentClient, "resumeBinding" | "getStatus">, bindings: BindingInfo[]): Promise<string[]> {
  const failures: string[] = [];
  for (const binding of bindings) {
    if (binding.detached || !binding.paused) continue;
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        if (attempt > 0) {
          // A timed-out acknowledgement is ambiguous. Re-read before retrying, so
          // a completed resume or concurrent detachment is never replayed.
          const current = (await agent.getStatus()).management?.bindings;
          if (!current) throw new Error("agent 未返回完整绑定状态。");
          const latest = current.find(item => item.binding_id === binding.binding_id);
          if (!latest || latest.detached || !latest.paused) break;
        }
        await agent.resumeBinding(binding.binding_id);
        break;
      } catch (error) {
        if (error instanceof AgentRequestError && error.retryable && attempt < 2) {
          await new Promise(resolve => setTimeout(resolve, 500 * (attempt + 1)));
          continue;
        }
        failures.push(`${binding.local_path}：${error instanceof Error ? error.message : "恢复绑定失败。"}`);
        break;
      }
    }
  }
  return failures;
}
