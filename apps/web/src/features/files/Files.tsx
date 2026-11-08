import { useEffect, useMemo, useState } from "react";
import { listRoots, type RootSummary, type SnapshotEntry } from "@vaultsyncd/api-client";
import { Alert, Anchor, Breadcrumbs, Button, Code, Group, Pagination, Paper, Select, Stack, Text, TextInput } from "@mantine/core";
import { Dialog, EmptyState, StatusBadge, Table } from "../../components/ui";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { ancestry, fileSize, readDirectorySnapshot, type DirectorySnapshot } from "./browser";

export function FilesPanel() {
  const auth = useAuth();
  if (!auth.account || !auth.client) return <EmptyState title="请先登录">从账号与会话页面登录后浏览授权 Root。</EmptyState>;
  return <RootBrowser key={auth.account.user_id} />;
}

function RootBrowser() {
  const auth = useAuth();
  const client = auth.client!;
  const [roots, setRoots] = useState<RootSummary[]>([]);
  const [rootId, setRootId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setError(""); setRoots([]);
    client.request(options => listRoots(options, { signal: controller.signal })).then(result => {
      if (controller.signal.aborted) return;
      setRoots(result.roots);
      setRootId(previous => result.roots.some(root => root.root_id === previous) ? previous : result.roots[0]?.root_id ?? null);
    }).catch(cause => {
      if (controller.signal.aborted) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "读取 Root 失败，请重试。");
    }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [client, revision]);
  const root = roots.find(item => item.root_id === rootId);
  return <Stack>
    <Paper withBorder p="md">
      <Stack>
        <Group align="flex-end" aria-label="文件浏览器工具栏">
          <Select label="浏览 Root" placeholder="选择目录" value={rootId} allowDeselect={false} searchable
            nothingFoundMessage="没有匹配的 Root" disabled={loading || !roots.length} className="files-root-select"
            data={roots.map(item => ({ value: item.root_id, label: `${item.name || "未命名 Root"} · ${item.root_id.slice(0, 8)}` }))}
            onChange={setRootId} />
          <Button variant="default" disabled={loading} onClick={() => setRevision(value => value + 1)}>刷新 Root 列表</Button>
        </Group>
        {loading && <Text role="status">正在读取授权目录…</Text>}
        {error && <Alert color="red" role="alert">{error}</Alert>}
        {!loading && !error && !roots.length && <Text>尚无授权 Root，请先在 Root 目录页面注册目录。</Text>}
        {root?.health === "unavailable" && <Alert color="yellow">Root 当前不可用。清单仅代表已提交的元数据，请在 Root 目录页面检查健康与采集状态。</Alert>}
      </Stack>
    </Paper>
    {root && <DirectoryBrowser key={root.root_id} root={root} />}
  </Stack>;
}

function DirectoryBrowser({ root }: { root: RootSummary }) {
  const auth = useAuth();
  const client = auth.client!;
  const [snapshot, setSnapshot] = useState<DirectorySnapshot | null>(null);
  const [directoryId, setDirectoryId] = useState<string>();
  const [selected, setSelected] = useState<SnapshotEntry | null>(null);
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setSnapshot(null); setSelected(null); setError(""); setPage(1);
    readDirectorySnapshot(client, root.root_id, controller.signal).then(result => {
      if (controller.signal.aborted) return;
      setSnapshot(result);
      setDirectoryId(previous => previous && result.entries.get(previous)?.kind === "directory" ? previous : result.root.entry_id);
    }).catch(cause => {
      if (controller.signal.aborted) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "目录读取失败，请刷新重试。");
    }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [client, root.root_id, revision]);
  const directory = snapshot && directoryId ? snapshot.entries.get(directoryId) : undefined;
  const path = snapshot && directory ? ancestry(snapshot, directory) : [];
  const children = useMemo(() => {
    const term = query.trim().toLocaleLowerCase("zh-CN");
    return (snapshot?.children.get(directoryId ?? "") ?? []).filter(entry => entry.name.toLocaleLowerCase("zh-CN").includes(term));
  }, [snapshot, directoryId, query]);
  function navigate(entry: SnapshotEntry) { setDirectoryId(entry.entry_id); setQuery(""); setPage(1); setSelected(null); }
  const rootName = root.name || "Root 根目录";
  return <Stack>
    <Paper withBorder p="md">
      <Stack>
        <Group justify="space-between">
          <Group><Button variant="default" disabled={!directory?.parent_id || loading} onClick={() => navigate(snapshot!.entries.get(directory!.parent_id!)!)}>返回上级</Button>
            <Button variant="default" disabled={loading} onClick={() => setRevision(value => value + 1)}>刷新文件列表</Button>
            <Button variant="subtle" disabled={!directory || loading} onClick={() => setSelected(directory!)}>当前目录属性</Button></Group>
          <StatusBadge tone="info">只读浏览</StatusBadge>
        </Group>
        {!!path.length && <Breadcrumbs aria-label="目录位置" className="files-breadcrumbs">{path.map((entry, index) => index === path.length - 1
          ? <Text key={entry.entry_id} size="sm" className="break-all" aria-current="location">{entry.name || rootName}</Text>
          : <Anchor component="button" key={entry.entry_id} size="sm" className="break-all" onClick={() => navigate(entry)}>{entry.name || rootName}</Anchor>)}</Breadcrumbs>}
        <TextInput label="筛选当前目录" placeholder="按文件或文件夹名称筛选" value={query} disabled={!snapshot || loading}
          onChange={event => { setQuery(event.currentTarget.value); setPage(1); }} />
        <Text size="xs" c="dimmed">显示已采集并提交的文件与目录；外部修改在采集后可刷新查看。</Text>
        {snapshot && <Text size="xs" c="dimmed">快照提交序号 {snapshot.metadata.high_water_seq} · 当前目录 {snapshot.children.get(directoryId ?? "")?.length ?? 0} 项 · 匹配 {children.length} 项</Text>}
        {loading && <Text role="status">正在读取完整目录快照…</Text>}
        {error && <Alert color="red" role="alert">{error} 未能读取完整目录，请刷新重试。</Alert>}
      </Stack>
    </Paper>
    {snapshot && directory && <>
      <Table caption="文件与目录" rows={children.slice((page - 1) * 100, page * 100)} rowKey={entry => entry.entry_id}
        emptyMessage={query.trim() ? "当前目录没有匹配的名称。" : "此目录在已提交快照中为空。"} columns={[
          { key: "name", title: "名称", render: entry => <Anchor component="button" ta="left" className="break-all" onClick={() => entry.kind === "directory" ? navigate(entry) : setSelected(entry)}>{entry.name}</Anchor> },
          { key: "kind", title: "类型", render: entry => <Text size="sm">{entry.kind === "directory" ? "文件夹" : "文件"}</Text> },
          { key: "size", title: "大小", render: entry => <Text size="sm">{fileSize(entry)}</Text> },
          { key: "revision", title: "修订号", render: entry => <Text size="sm">{entry.revision}</Text> },
          { key: "properties", title: "操作", render: entry => <Button size="compact-sm" variant="subtle" aria-label={`查看 ${entry.name} 的属性`} onClick={() => setSelected(entry)}>属性</Button> },
        ]} />
      {children.length > 100 && <Pagination aria-label="文件列表分页" total={Math.ceil(children.length / 100)} value={page} onChange={setPage} size="sm" siblings={0} />}
    </>}
    <Dialog opened={!!snapshot && !!selected} title="文件属性" onClose={() => setSelected(null)}>
      {snapshot && selected && <Stack gap="sm">
        {[
          ["名称", selected.name || rootName],
          ["类型", selected.kind === "directory" ? "文件夹" : "文件"],
          ["Root", rootName],
          ["Root ID", root.root_id],
          ["相对路径", ancestry(snapshot, selected).map(entry => entry.name).join("/") || "/"],
          ["大小", fileSize(selected)],
          ...(selected.kind === "directory" ? [["直接子项", String(snapshot.children.get(selected.entry_id)?.length ?? 0)]] : []),
          ["修订号", selected.revision],
          ["条目 ID", selected.entry_id],
          ["内容版本", selected.version_id ?? "—"],
          ["SHA-256", selected.hash ?? "—"],
        ].map(([label, value]) => <Stack gap={2} key={label}><Text size="xs" c="dimmed">{label}</Text><Code block className="files-property-value">{value}</Code></Stack>)}
      </Stack>}
    </Dialog>
  </Stack>;
}
