import { useEffect, useRef, useState, type FormEvent } from "react";
import { Button as LinkButton, Card, Group, Loader, NativeSelect, Select, Stack, Text, TextInput, Title } from "@mantine/core";
import { Button, Table } from "@vaultsyncd/ui";
import type { BindingInfo, BindingStatus, ConnectionInfo, RemoteItem } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";
import { BindingActions } from "./BindingActions";

type Actions = {
  agent: AgentClient; changed(): Promise<void>; run(action: () => Promise<void>): void; busy: boolean;
};

export function Bindings({ agent, bindings, connections, states, read, changed, run, busy }: Actions & {
  bindings?: BindingInfo[]; connections: ConnectionInfo[]; states?: BindingStatus[]; read: boolean;
}) {
  const [serverId, setServerId] = useState("");
  const [managing, setManaging] = useState<string>();
  const selected = connections.find(item => item.server_id === serverId);
  function serverLabel(serverId?: string) {
    const server = connections.find(item => item.server_id === serverId);
    if (!server) return serverId ?? "—";
    const duplicateAddress = connections.some(item => item.server_id !== serverId && item.server_url === server.server_url);
    return duplicateAddress ? `${server.server_url} · ${server.server_id}` : server.server_url;
  }
  function state(row: BindingInfo) {
    const status = states?.find(s => s.binding_id === row.binding_id);
    const server = connections.find(item => item.server_id === row.server_id);
    if (row.detached) return "已解除绑定（本地数据保留）";
    if (row.paused) return "已暂停";
    if (read && server && !server.authenticated) return "等待登录服务器";
    if (!status) return "尚未取得完整状态";
    if (status.synchronized) return status.offline !== "0" ? "基线已确认 · 当前离线" : "已同步";
    return `正在核对/合并：未确认 ${status.dirty}，待办 ${status.pending}，传输 ${status.transfers}，未核对 ${status.unknown}，失败 ${status.failed}，应用差额 ${status.event_lag}`;
  }
  return <Stack gap="lg">
    <Group justify="space-between">
      <Title order={1} size="h2">同步目录</Title>
      {read && !connections.length && <LinkButton component="a" href="#/connections/add">添加服务器</LinkButton>}
    </Group>
    {read && !!connections.length && <Card component="section" withBorder>
      <Stack>
        <Title order={2} size="h3">添加同步目录</Title>
        <NativeSelect label="服务器" name="server_id" value={selected?.server_id ?? ""} disabled={busy}
          onChange={event => setServerId(event.target.value)}>
          <option value="">请选择服务器</option>
          {connections.map(server => <option key={server.server_id} value={server.server_id}>
            {serverLabel(server.server_id)}{server.authenticated ? "" : "（未登录）"}
          </option>)}
        </NativeSelect>
        {selected && (selected.authenticated
          ? <BindingForm key={JSON.stringify([selected.server_id, selected.server_url, selected.ca_file])}
            agent={agent} serverId={selected.server_id} changed={changed} run={run} busy={busy} />
          : <Group>
            <Text size="sm">登录此服务器后可选择 Vault。</Text>
            <LinkButton component="a" href="#/connections" variant="default">连接设置</LinkButton>
          </Group>)}
      </Stack>
    </Card>}
    <Table<BindingInfo> caption="本地 Binding" rows={bindings ?? []} rowKey={row => row.binding_id}
      emptyMessage={read ? "尚无同步目录。" : "尚未读取同步目录。"} columns={[
        { key: "path", title: "本地目录", render: row => row.local_path },
        { key: "server", title: "服务器", render: row => serverLabel(row.server_id) },
        { key: "vault", title: "Vault", render: row => row.vault_id },
        { key: "mode", title: "模式", render: row => row.mode === "full" ? "完整同步" : "按需下载" },
        { key: "state", title: "状态与合并进度", render: state },
        { key: "controls", title: "操作", render: row => <Group gap="xs">
          <Button size="xs" variant="default" disabled={busy || !read || row.detached} onClick={() => run(async () => {
            await (row.paused ? agent.resumeBinding(row.binding_id) : agent.pauseBinding(row.binding_id));
            await changed();
          })}>{row.paused ? "恢复" : "暂停"}</Button>
          <Button size="xs" disabled={busy || !read} onClick={() => setManaging(row.binding_id)}>管理与恢复</Button>
        </Group> },
      ]} />
    {bindings?.filter(b => b.binding_id === managing).map(binding => <BindingActions key={binding.binding_id} agent={agent} binding={binding}
      status={states?.find(s => s.binding_id === binding.binding_id)} changed={changed} run={run} busy={busy || !read} />)}
  </Stack>;
}

function BindingForm({ agent, serverId, changed, run, busy }: Actions & { serverId: string }) {
  const [vaults, setVaults] = useState<RemoteItem[]>();
  const [vaultId, setVaultId] = useState<string | null>(null);
  const [vaultsLoading, setVaultsLoading] = useState(false);
  const [vaultsError, setVaultsError] = useState("");
  const [notice, setNotice] = useState("");
  const [localPath, setLocalPath] = useState("");
  const readingVaults = useRef(false);
  const request = useRef(0);

  useEffect(() => () => { request.current++; }, []);
  async function readVaults() {
    if (readingVaults.current) return;
    const generation = ++request.current;
    readingVaults.current = true;
    setVaultsLoading(true);
    setVaultsError("");
    try {
      const result = await agent.listVaults(serverId);
      const items = result.items;
      if (!items) throw new Error("agent 未返回 Vault 清单。");
      if (generation === request.current) {
        setVaults(items);
        setVaultId(current => items.some(vault => vault.id === current) ? current : null);
      }
    } catch (error) {
      if (generation === request.current) {
        setVaults(undefined);
        setVaultId(null);
        setVaultsError(error instanceof Error ? error.message : "读取 Vault 失败，请重新打开选择框重试。");
      }
    } finally {
      readingVaults.current = false;
      if (generation === request.current) setVaultsLoading(false);
    }
  }
  const vaultSelected = !vaultsLoading && !vaultsError && !!vaultId && !!vaults?.some(vault => vault.id === vaultId);

  function create(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (busy || !vaultSelected || !vaultId || !localPath) return;
    const mode = new FormData(event.currentTarget).get("mode") === "on_demand" ? "on_demand" : "full";
    run(async () => {
      await agent.createBinding({ bindingId: crypto.randomUUID().replaceAll("-", ""), serverId, vaultId, localPath, mode });
      setLocalPath("");
      setNotice("绑定已建立，正在核对并合并内容。");
      await changed();
    });
  }

  return <form onSubmit={create}><Stack>
    <Select label="远端 Vault" name="vault_id" required placeholder="请选择 Vault"
      value={vaultId} onChange={setVaultId} allowDeselect={false} disabled={busy}
      data={vaultsLoading || vaultsError ? [] : vaults?.map(vault => ({ value: vault.id, label: vault.name })) ?? []}
      onDropdownOpen={() => void readVaults()} aria-busy={vaultsLoading} error={vaultsError}
      rightSection={vaultsLoading ? <Loader size="xs" /> : undefined}
      nothingFoundMessage={vaultsLoading ? "正在读取 Vault…" : vaultsError ? "读取失败，请重新打开重试。" : "此服务器没有已授权的 Vault。"} />
    <Group align="flex-end" wrap="nowrap">
      <TextInput className="desktop-grow" label="本地目录" readOnly name="local_path" value={localPath} placeholder="请选择本地同步目录" />
      <Button variant="default" disabled={busy || !vaultSelected} onClick={() => run(async () => {
        const selected = await agent.selectDirectory();
        if (selected !== null) setLocalPath(selected);
      })}>选择目录…</Button>
    </Group>
    <NativeSelect label="模式" name="mode" defaultValue="full" disabled={busy}>
      <option value="full">完整同步（保留离线内容）</option><option value="on_demand">按需下载（仅完整内容可离线读取）</option>
    </NativeSelect>
    <Group><Button type="submit" disabled={busy || !vaultSelected || !localPath}>创建绑定并开始合并</Button></Group>
    {notice && <Text size="sm" role="status">{notice}</Text>}
  </Stack></form>;
}
