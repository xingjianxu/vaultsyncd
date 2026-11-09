import { useEffect, useState } from "react";
import { ApiError, listManagedVaults, registerVault, type ManagedVault, type ManagedVaultList, type VaultCollectionState } from "@vaultsyncd/api-client";
import { Anchor, Button, Code, Group, Modal, Stack, Text, TextInput } from "@mantine/core";
import { useForm } from "@mantine/form";
import { EmptyState, StatusBadge, Table } from "../../components/ui";
import { Feedback, showFeedback } from "../../components/Feedback";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { filesLocation, formatBytes } from "../files/browser";

const collectionLabels: Record<VaultCollectionState, string> = {
  not_scanned: "尚未扫描", scanning: "正在采集", pending: "等待稳定或重试", complete: "本进程初始采集完成", blocked: "采集受阻",
};
export function registrationError(error: unknown): string {
  if (!(error instanceof ApiError) || error.status >= 500 || error.status === 408) return "无法确认注册结果。请先刷新清单核对路径，再决定是否重试；不要把网络失败当作未注册。";
  const reason = error.body.details?.reason ?? error.body.code;
  const steps: Record<string, string> = {
    outside_allowed_bases: "请选择允许基目录内的服务器绝对路径；账号无管理权限时请联系 owner。",
    vault_overlap: "使用与既有 Vault 无父子或别名重叠的独立目录。",
    registered_overlap: "使用与既有 Vault 无父子或别名重叠的独立目录。",
    probe_failed: "核对目录存在、服务账号读取权限和挂载，再刷新重试。",
  };
  const next = steps[reason] ?? "请检查目录是本地 btrfs、已启用 CoW，且没有嵌套挂载、子卷或内部状态重叠；按服务端原因修复后再试。";
  return `${error.body.message}（${reason}） ${next} 请求编号：${error.body.request_id}`;
}
function Collection({ vault }: { vault: ManagedVault }) {
  return <Stack gap="xs"><StatusBadge tone={vault.collection.state === "blocked" ? "warning" : "info"}>{collectionLabels[vault.collection.state]}</StatusBadge>
    <Text size="xs" c="dimmed">最近一轮检查 {vault.collection.examined} 项 · 待处理 {vault.collection.pending} 项</Text>
    {(vault.collection.state === "blocked" || vault.collection.state === "pending") && <Text size="sm" c="dimmed">{vault.collection.detail}</Text>}</Stack>;
}
export function VaultsPanel() {
  const auth = useAuth();
  const [data, setData] = useState<ManagedVaultList | null>(null);
  const [loading, setLoading] = useState(false);
  const [registering, setRegistering] = useState(false);
  const [dialog, setDialog] = useState<"health" | "register" | null>(null);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  const client = auth.client;
  const user = auth.account?.user_id;
  const form = useForm({ mode: "uncontrolled", initialValues: { path: "" } });
  useEffect(() => {
    setData(null); setError("");
    if (!client || !user) return;
    let active = true;
    const controller = new AbortController();
    setLoading(true);
    client.request(options => listManagedVaults(options, { signal: controller.signal })).then(value => {
      if (active) setData(value);
    }).catch(cause => {
      if (!active) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "读取失败，请刷新重试。");
    }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [client, user, revision]);
  async function register({ path }: typeof form.values) {
    if (!client || registering || !data) return;
    setRegistering(true); setError("");
    try {
      const registered = await client.request(options => registerVault(options, { path }));
      showFeedback(`已注册 Vault ${registered.vault_id}；等待初始采集，现有文件保留。`);
      setDialog(null);
      form.reset();
      setRevision(value => value + 1);
    } catch (cause) {
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(registrationError(cause));
    } finally { setRegistering(false); }
  }
  if (!auth.account) return <EmptyState title="请先登录">从账号与会话页面登录 owner 后查看 Vault 配置与健康。</EmptyState>;
  return <Stack className="vaults-panel">
    <div className="vaults-toolbar" role="group" aria-label="Vault 目录工具栏">
      <Button variant="default" onClick={() => setDialog("health")}>Vault 健康与采集</Button>
      <Button disabled={!data?.allowed_bases.length || loading || registering} onClick={() => setDialog("register")}>注册服务器目录</Button>
      {data && <div className="vaults-allowed-bases">
        <Text size="sm" c="dimmed">允许基目录</Text>
        {data.allowed_bases.length ? data.allowed_bases.map(base => <Code key={base}>{base}</Code>)
          : <Text size="sm" c="dimmed">未配置，无法注册</Text>}
      </div>}
    </div>
    <Feedback message={error} />
    {loading && <Text role="status" size="sm" c="dimmed">正在核验目录与采集状态…</Text>}
    {data && <Table caption="已注册 Vault" rows={data.vaults} rowKey={vault => vault.vault_id} emptyMessage="尚未注册 Vault" columns={[
      { key: "path", title: "服务器路径", render: vault => <Anchor href={`#${filesLocation(vault.vault_id)}`} size="sm" className="break-all"><code>{vault.path}</code></Anchor> },
      { key: "size", title: "文件容量", render: vault => <Text size="sm" title={vault.file_bytes === undefined ? undefined : `${BigInt(vault.file_bytes).toLocaleString("zh-CN")} 字节（当前已提交文件）`}>{formatBytes(vault.file_bytes)}</Text> },
      { key: "id", title: "Vault ID", render: vault => <Text component="code" size="sm" className="break-all">{vault.vault_id}</Text> },
      { key: "health", title: "状态", render: vault => <StatusBadge tone={vault.health === "unavailable" ? "danger" : "success"}>{vault.health === "healthy" ? "可用" : "Vault 不可用"}</StatusBadge> },
      { key: "seq", title: "已提交序号", render: vault => <Text component="code" size="sm">{vault.vault_seq}</Text> },
    ]} />}
    <Modal opened={dialog === "register"} onClose={() => { if (!registering) setDialog(null); }} title="注册服务器目录" centered
      closeOnEscape={!registering} closeOnClickOutside={!registering} withCloseButton={!registering}>
      <form onSubmit={form.onSubmit(register)}>
        <Stack>
          <TextInput name="path" label="服务器目录绝对路径" placeholder="输入已有目录的绝对路径"
            data-autofocus required disabled={registering} maxLength={4096}
            key={form.key("path")} {...form.getInputProps("path")} />
          <Group justify="flex-end"><Button type="submit" disabled={registering || loading || !data?.allowed_bases.length}>{registering ? "校验并注册中…" : "校验并注册 Vault"}</Button></Group>
        </Stack>
      </form>
    </Modal>
    <Modal opened={dialog === "health"} onClose={() => setDialog(null)} title="Vault 健康与采集" size="xl" centered>
      <Stack>
        <Group justify="flex-end"><Button variant="default" disabled={loading || registering} onClick={() => setRevision(value => value + 1)}>刷新目录状态</Button></Group>
        {loading && <Text role="status" size="sm" c="dimmed">正在核验目录与采集状态…</Text>}
        {!loading && !data && <Text size="sm" c="dimmed">未能读取目录状态，请刷新重试。</Text>}
        {data && <Table caption="目录状态" rows={data.vaults} rowKey={vault => vault.vault_id} emptyMessage="尚未注册 Vault" columns={[
          { key: "path", title: "服务器路径 / 身份", render: vault => <Stack gap="xs"><Text component="code" size="sm" className="break-all">{vault.path}</Text><Text size="xs" c="dimmed" className="break-all">{vault.vault_id}</Text></Stack> },
          { key: "health", title: "现场健康", render: vault => <Stack gap="xs"><StatusBadge tone={vault.health === "unavailable" ? "danger" : "success"}>{vault.health === "healthy" ? "挂载核对通过" : "Vault 不可用"}</StatusBadge>{vault.reason && <Text role="alert" size="sm" c="red">{vault.reason}</Text>}{vault.health === "unavailable" && <Text size="sm" c="dimmed">{vault.next_step}</Text>}</Stack> },
          { key: "collection", title: "采集进度", render: vault => <Collection vault={vault} /> },
          { key: "seq", title: "已提交序号", render: vault => <Text component="code" size="sm">{vault.vault_seq}</Text> },
        ]} />}
      </Stack>
    </Modal>
  </Stack>;
}
