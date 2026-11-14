import { CloudDownload, HardDriveDownload, History, RefreshCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Alert, Card, Group, Stack, Text, Title } from "@mantine/core";
import { Button, Dialog, Icon, StatusBadge, Table } from "@vaultsyncd/ui";
import type { RemoteItem } from "@vaultsyncd/local-ipc-types";
import { nativeAgent, type AgentClient } from "../../services/agent";
import { nativeShellFile, type ShellFileClient, type ShellFileStatus, type ShellTarget } from "../../services/shell";

const labels: Record<string, string> = { unavailable: "不可用", failed: "失败", conflict: "冲突", local_changes: "待上传本地修改", syncing: "正在同步", downloaded: "已下载", cloud_only: "仅云端" };
const titles = { history: "查看文件历史", download: "下载到本地", release: "释放空间" };
const icons = { history: History, download: CloudDownload, release: HardDriveDownload };
function detail(error: unknown) { return error instanceof Error ? error.message : "文件操作失败，请重试。"; }

export function ShellFile({ client = nativeShellFile, agent = nativeAgent }: { client?: ShellFileClient; agent?: AgentClient }) {
  const [target, setTarget] = useState<ShellTarget>();
  const [status, setStatus] = useState<ShellFileStatus>();
  const [items, setItems] = useState<RemoteItem[]>(); const [cursor, setCursor] = useState<string>(); const [next, setNext] = useState<string>();
  const [error, setError] = useState(""); const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(true); const [confirm, setConfirm] = useState(false); const [acknowledged, setAcknowledged] = useState(false);
  const mounted = useRef(false); const executing = useRef(false); const operation = useRef<string | undefined>(undefined); const sequence = useRef(0);

  async function current(value: ShellTarget) {
    const result = await client.status();
    if (result.binding.detached || result.binding.binding_id !== value.binding_id || result.file.entry_id !== value.entry_id || result.file.path !== value.path) {
      throw new Error("文件位置或绑定身份已变化，请重新打开右键菜单。");
    }
    return result;
  }
  async function read(value: ShellTarget, after?: string, request = sequence.current) {
    const fresh = await current(value);
    if (!mounted.current || request !== sequence.current) return;
    setStatus(fresh);
    if (value.action === "history") {
      const result = await agent.listHistory(value.binding_id, value.entry_id, after);
      if (!result.items) throw new Error("agent 未返回文件历史。");
      if (mounted.current && request === sequence.current) { setItems(result.items); setCursor(after); setNext(result.next_cursor); }
    }
  }
  useEffect(() => {
    mounted.current = true; let active = true; const request = ++sequence.current;
    async function start() {
      try {
        const value = await client.target();
        if (!active) return;
        setTarget(value); await read(value, undefined, request);
      } catch (error) { if (active) { setStatus(undefined); setError(detail(error)); } }
      finally { if (active) setBusy(false); }
    }
    void start();
    return () => { active = false; mounted.current = false; sequence.current++; };
  }, [client, agent]);

  async function refresh(after?: string) {
    if (!target || executing.current) return;
    executing.current = true; sequence.current++; setBusy(true); setError(""); setStatus(undefined);
    try { await read(target, after); }
    catch (error) { if (mounted.current) { setStatus(undefined); setError(detail(error)); } }
    finally { executing.current = false; if (mounted.current) setBusy(false); }
  }
  async function act() {
    if (!target || target.action === "history" || executing.current || acknowledged) return;
    executing.current = true; setBusy(true); setError(""); setMessage(""); setConfirm(false);
    // Retain the same operation identity after an unknown result; never invent a fresh retry.
    operation.current ??= crypto.randomUUID().replaceAll("-", "");
    try {
      await client.action(operation.current);
      if (mounted.current) {
        setAcknowledged(true);
        setMessage(target.action === "download" ? "下载请求已由 agent 接收，请刷新查看文件状态。" : "释放请求已由 agent 确认。");
      }
      try {
        const fresh = await current(target); if (mounted.current) setStatus(fresh);
      } catch (error) { if (mounted.current) { setStatus(undefined); setError(`操作已确认，状态刷新失败：${detail(error)}`); } }
    } catch (error) {
      if (mounted.current) { setStatus(undefined); setError(`${detail(error)}；请刷新状态后核对原操作。`); }
    } finally { executing.current = false; if (mounted.current) setBusy(false); }
  }
  const file = status?.file; const binding = status?.binding;
  const displayPath = target && binding ? `${binding.local_path.replace(/[\\/]$/, "")}\\${target.path.replaceAll("/", "\\")}` : target?.path;
  const protectedFile = !file || file.dirty || file.pending || file.busy || !file.entry_id || binding?.detached;
  const firstAllowed = !!file && (target?.action === "release" ? !protectedFile && file.release_allowed : !protectedFile && !binding?.paused);
  // A completed release can lose its reply and leave release_allowed=false. Reconcile
  // its original operation after a fresh identity check; native code owns admission.
  const reconcileAllowed = !!operation.current && !!status && !binding?.detached;
  const allowed = firstAllowed || reconcileAllowed;
  return <Stack gap="lg" p="lg" data-shell-file>
    <Group gap="sm"><Icon icon={target ? icons[target.action] : History} /><Title order={1} size="h2">{target ? titles[target.action] : "文件操作"}</Title></Group>
    {target && <Text size="sm" style={{ overflowWrap: "anywhere" }} data-shell-path>{displayPath}</Text>}
    {error && <Alert color="red" role="alert">{error}</Alert>}
    {message && <Alert color="green" role="status">{message}</Alert>}
    {target && <Card withBorder><Stack gap="sm">
      <Group justify="space-between">
        <StatusBadge tone={!file || file.dirty || file.pending || file.busy ? "warning" : "neutral"}>{file ? labels[file.status] ?? "未知状态" : busy ? "正在读取" : "状态未确认"}</StatusBadge>
        <Button variant="default" disabled={busy} leftSection={<Icon icon={RefreshCw} />} onClick={() => void refresh(cursor)}>刷新</Button>
      </Group>
      {file && (file.dirty || file.pending || file.busy || binding?.paused) && <Text size="sm">{[file.dirty && "有未上传修改", file.pending && "存在待确认操作", file.busy && "文件正在使用或保护未知", binding?.paused && "同步已暂停"].filter(Boolean).join(" · ")}</Text>}
      {target.action === "release" && file?.release_reason && <Text size="sm">{file.release_reason}</Text>}
      {target.action !== "history" && <Group>
        <Button color={target.action === "release" ? "red" : undefined} disabled={busy || !allowed || acknowledged} onClick={() => target.action === "release" ? setConfirm(true) : void act()}>
          {operation.current && !acknowledged ? "核对原操作" : titles[target.action]}
        </Button>
      </Group>}
    </Stack></Card>}
    {target?.action === "history" && <>
      <Table<RemoteItem> caption="文件历史" rows={items ?? []} rowKey={row => row.id} emptyMessage={items ? "当前页没有历史版本。" : busy ? "正在读取文件历史。" : "尚未取得文件历史。"} columns={[
        { key: "name", title: "文件", render: row => <>{row.name}{row.conflict_source ? " · 冲突副本" : ""}</> },
        { key: "version", title: "版本", render: row => row.version_id ?? "目录" },
        { key: "size", title: "大小", render: row => row.size === undefined ? "—" : `${row.size} 字节` },
        { key: "hash", title: "内容摘要", render: row => <Text size="xs" style={{ overflowWrap: "anywhere" }}>{row.hash ?? "—"}</Text> },
      ]} />
      <Group justify="flex-end"><Button variant="default" disabled={busy || !cursor} onClick={() => void refresh()}>第一页</Button><Button variant="default" disabled={busy || !next} onClick={() => void refresh(next)}>下一页</Button></Group>
    </>}
    <Dialog opened={confirm} title="确认释放本地内容" onClose={() => setConfirm(false)}>
      <Text>{displayPath} 的本地内容将移除，远端数据保留；离线将不可读取。未确认或忙文件不会释放。</Text>
      <Group><Button color="red" disabled={busy || !allowed} onClick={() => void act()}>确认释放</Button></Group>
    </Dialog>
  </Stack>;
}
