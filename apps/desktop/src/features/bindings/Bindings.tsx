import { useState, type FormEvent } from "react";
import { Card, Group, NativeSelect, SimpleGrid, Stack, Text, TextInput, Title } from "@mantine/core";
import { Button, Table } from "@vaultsyncd/ui";
import type { BindingInfo, BindingStatus, RemoteItem } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";
import { BindingActions } from "./BindingActions";

export function Bindings({ agent, bindings, states, read, changed, run, busy }: {
  agent: AgentClient; bindings?: BindingInfo[]; states?: BindingStatus[]; read: boolean;
  changed(): Promise<void>; run(action: () => Promise<void>): void; busy: boolean;
}) {
  const [vaults, setVaults] = useState<RemoteItem[]>();
  const [notice, setNotice] = useState("");
  const [localPath, setLocalPath] = useState("");
  const [managing, setManaging] = useState<string>();
  function create(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const form = new FormData(event.currentTarget);
    const vaultId = String(form.get("vault_id"));
    if (!localPath) return;
    const mode = form.get("mode") === "full" ? "full" : "on_demand";
    run(async () => {
      await agent.createBinding({ bindingId: crypto.randomUUID().replaceAll("-", ""), vaultId, localPath, mode });
      setNotice("绑定已建立；agent 正在核对并合并内容，请查看下方进度。"); await changed();
    });
  }
  function state(row: BindingInfo) {
    const status = states?.find(s => s.binding_id === row.binding_id);
    if (row.detached) return "已解除绑定（本地数据保留）";
    if (row.paused) return "已暂停";
    if (!status) return "尚未取得完整状态";
    if (status.synchronized) return status.offline !== "0" ? "基线已确认 · 当前离线" : "已同步";
    return `正在核对/合并：未确认 ${status.dirty}，待办 ${status.pending}，传输 ${status.transfers}，未核对 ${status.unknown}，失败 ${status.failed}，应用差额 ${status.event_lag}`;
  }
  return <Stack gap="lg">
    <Title order={1} size="h2">同步目录</Title>
    <Card component="section" withBorder>
      <Stack>
        <Group justify="space-between">
          <Title order={2} size="h3">添加同步目录</Title>
          <Button variant="default" disabled={busy} onClick={() => run(async () => { const result = await agent.listVaults(); if (!result.items) throw new Error("agent 未返回 Vault 清单。"); setVaults(result.items); })}>读取授权 Vault</Button>
        </Group>
        <form onSubmit={create}><Stack>
          <SimpleGrid cols={{ base: 1, sm: 2 }}>
            <NativeSelect label="远端 Vault" name="vault_id" required defaultValue="">
              <option value="">请选择 Vault</option>{vaults?.map(vault => <option key={vault.id} value={vault.id}>{vault.name}</option>)}
            </NativeSelect>
            <NativeSelect label="模式" name="mode" defaultValue="full">
              <option value="full">完整同步（保留离线内容）</option><option value="on_demand">按需下载（仅完整内容可离线读取）</option>
            </NativeSelect>
          </SimpleGrid>
          {vaults?.length === 0 && <Text size="sm">服务器当前没有已授权 Vault，请联系管理员。</Text>}
          <Group align="flex-end" wrap="nowrap">
            <TextInput className="desktop-grow" label="本地目录" readOnly name="local_path" value={localPath} placeholder="请选择本地同步目录" />
            <Button variant="default" disabled={busy} onClick={() => run(async () => {
              const selected = await agent.selectDirectory();
              if (selected !== null) setLocalPath(selected);
            })}>选择目录…</Button>
          </Group>
          <Text size="sm" c="dimmed">首次合并保留双方内容，不根据“缺少文件”推导删除。</Text>
          <Group><Button type="submit" disabled={busy || !vaults?.length || !localPath}>创建绑定并开始合并</Button></Group>
        </Stack></form>
        {notice && <Text size="sm" role="status">{notice}</Text>}
      </Stack>
    </Card>
    <Table<BindingInfo> caption="本地 Binding" rows={bindings ?? []} rowKey={row => row.binding_id}
      emptyMessage={read ? "尚无同步目录。" : "未连接，尚未读取同步目录。"} columns={[
        { key: "path", title: "本地目录", render: row => row.local_path },
        { key: "mode", title: "模式", render: row => row.mode === "full" ? "完整同步" : "按需下载" },
        { key: "state", title: "状态与合并进度", render: state },
        { key: "controls", title: "操作", render: row => <Group gap="xs"><Button size="xs" variant="default" disabled={busy || row.detached} onClick={() => run(async () => { await (row.paused ? agent.resumeBinding(row.binding_id) : agent.pauseBinding(row.binding_id)); await changed(); })}>{row.paused ? "恢复" : "暂停"}</Button><Button size="xs" disabled={busy} onClick={() => setManaging(row.binding_id)}>管理与恢复</Button></Group> },
      ]} />
    {bindings?.filter(b => b.binding_id === managing).map(binding => <BindingActions key={binding.binding_id} agent={agent} binding={binding} status={states?.find(s => s.binding_id === binding.binding_id)} changed={changed} run={run} busy={busy} />)}
  </Stack>;
}
