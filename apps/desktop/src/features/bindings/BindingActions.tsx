import { useState } from "react";
import { Card, Divider, Group, NativeSelect, Stack, Text, Textarea, TextInput, Title } from "@mantine/core";
import { Button, Dialog } from "@vaultsyncd/ui";
import { notifications } from "@mantine/notifications";
import type { BindingInfo, BindingStatus } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";

export function BindingActions({ agent, binding, status, run, busy, changed }: { agent: AgentClient; binding: BindingInfo; status?: BindingStatus; run(action: () => Promise<void>): void; busy: boolean; changed(): Promise<void> }) {
  const [mode, setMode] = useState<"full" | "on_demand">(binding.mode === "full" ? "full" : "on_demand");
  const [rules, setRules] = useState(""); const [rulesLoaded, setRulesLoaded] = useState(false);
  const [destination, setDestination] = useState(""); const [confirm, setConfirm] = useState(false); const [message, setMessage] = useState("");
  return <Card component="section" withBorder>
    <Stack>
      <Title order={2} size="h3">绑定管理：{binding.local_path}</Title>
      {message && <Text size="sm" role="status">{message}</Text>}
      <Group align="flex-end">
        <NativeSelect label="目标模式" name="target_mode" value={mode} onChange={event => setMode(event.target.value === "full" ? "full" : "on_demand")}>
          <option value="full">完整同步</option><option value="on_demand">按需下载</option>
        </NativeSelect>
        <Button disabled={busy || binding.detached} onClick={() => run(async () => {
          const result = await agent.setMode(binding.binding_id, mode);
          setMessage(result.mode_remaining ? `模式补齐仍需核对 ${result.mode_remaining} 项。` : "模式目标已保存，请查看真实进度。"); await changed();
        })}>保存模式</Button>
      </Group>
      <Text size="sm" c="dimmed">切到完整同步会补齐内容；完成前仍显示进行中。切到按需不会自动释放已下载内容。</Text>
      <Divider />
      <Group justify="space-between">
        <Title order={3} size="h4">排除规则</Title>
        <Button variant="default" disabled={busy || binding.detached} onClick={() => run(async () => {
          const result = await agent.getRules(binding.binding_id); if (result.rules === undefined) throw new Error("未取得当前排除清单");
          setRules(result.rules); setRulesLoaded(true);
        })}>读取排除规则</Button>
      </Group>
      <Text size="sm" c="dimmed">排除路径停止新同步，保留两端内容；取消排除后先重新核对。支持 # 注释、*、**、! 与目录规则，父目录被排除时不能仅重新包含子文件。</Text>
      <Textarea label="完整排除清单" name="exclusion_rules" minRows={4} autosize value={rules} onChange={event => setRules(event.target.value)} disabled={!rulesLoaded || busy || binding.detached} />
      <Group><Button disabled={!rulesLoaded || busy || binding.detached} onClick={() => run(async () => {
        await agent.setRules(binding.binding_id, rules, crypto.randomUUID().replaceAll("-", ""));
        setMessage("排除清单已保存；未删除本地或服务器内容，取消排除的条目正在重新协调。"); await changed();
      })}>保存排除规则</Button></Group>
      <Divider />
      <Title order={3} size="h4">恢复与解绑</Title>
      <TextInput label="恢复导出到新目录" name="recovery_destination" value={destination} onChange={event => setDestination(event.target.value)} placeholder="现有父目录下的新绝对路径" />
      <Text size="sm" c="dimmed">先导出未确认数据并核对恢复包；失败时源内容保留。</Text>
      <Group>
        <Button disabled={busy || !destination} onClick={() => run(async () => {
          const result = await agent.exportRecovery(binding.binding_id, destination); if (result.exported_files === undefined) throw new Error("agent 未返回导出完成证明。");
          setMessage(`恢复包已核验 ${result.exported_files} 份内容。`);
        })}>导出恢复包</Button>
        <Button color="red" variant="light" disabled={busy || binding.detached} onClick={() => setConfirm(true)}>解除绑定（保留数据）</Button>
      </Group>
    </Stack>
    <Dialog opened={confirm} title="确认解除绑定" onClose={() => setConfirm(false)}>
      <Text>停止管理此本地目录，保留本地内容、未确认数据和缓存；不会删除服务器文件。仅云端 {status?.metadata_only ?? "未知"} 项、部分内容 {status?.partial ?? "未知"} 项不能承诺离线可读，建议先导出恢复包。</Text>
      <Group><Button color="red" disabled={busy} onClick={() => {
        setConfirm(false); run(async () => {
          const result = await agent.detachBinding(binding.binding_id); if (!result.unconfirmed || !result.incomplete) throw new Error("agent 未返回解绑保留报告。");
          const needsRecovery = result.unconfirmed.length > 0 || result.incomplete.length > 0;
          // This card disappears after refresh; keep the data-preservation report visible.
          notifications.show({
            message: `已停止管理，保留未确认 ${result.unconfirmed.length} 项；非完整内容 ${result.incomplete.length} 项仍不能保证离线读取。`,
            color: needsRecovery ? "yellow" : "green", autoClose: needsRecovery ? false : 4000,
          });
          await changed();
        });
      }}>确认保留数据并解除</Button></Group>
    </Dialog>
  </Card>;
}
