import type { BindingInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "./agent";

// Only the main desktop's first complete snapshot is eligible. Polling and navigation
// must never undo a pause requested later in the same desktop session.
export async function restoreStartupBindings(agent: Pick<AgentClient, "resumeBinding">, bindings: BindingInfo[]): Promise<string[]> {
  const failures: string[] = [];
  for (const binding of bindings) {
    if (binding.detached || !binding.paused) continue;
    try {
      await agent.resumeBinding(binding.binding_id);
    } catch (error) {
      failures.push(`${binding.local_path}：${error instanceof Error ? error.message : "恢复绑定失败。"}`);
    }
  }
  return failures;
}
