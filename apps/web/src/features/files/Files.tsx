import { useEffect, useMemo, useRef, useState, type ReactElement, type ReactNode } from "react";
import { listRoots, type RootSummary, type SnapshotEntry } from "@vaultsyncd/api-client";
import { ActionIcon, Alert, Anchor, Code, Group, Menu, Pagination, Paper, Select, Stack, Table, Text, TextInput } from "@mantine/core";
import { EntryMenu, openMenuByKeyboard } from "./EntryMenu";
import { DirectoryTree } from "./DirectoryTree";
import { useFileActions } from "./FileActions";
import { FileIcon } from "./FileIcon";
import { Dialog, EmptyState } from "../../components/ui";
import { Feedback, showFeedback } from "../../components/Feedback";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { ancestry, directoryAtPath, filesLocation, fileModified, fileSize, fileSizeDetail, parseFilesLocation, readDirectorySnapshot, type DirectorySnapshot } from "./browser";

export function FilesPanel({ location }: { location: string }) {
  const auth = useAuth();
  if (!auth.account || !auth.client) return <EmptyState title="请先登录">从账号与会话页面登录后浏览授权 Root。</EmptyState>;
  return <RootBrowser key={auth.account.user_id} location={location} />;
}

// 工具栏只遍历本次文件浏览历史；浏览器自身的前进/后退仍按原生历史工作。
function useFolderNavigation(location: string) {
  const [session] = useState(() => crypto.randomUUID());
  const [position, setPosition] = useState(0);
  const [last, setLast] = useState(0);
  useEffect(() => {
    const state = window.history.state?.vaultsyncdFiles;
    if (state?.session === session) setPosition(state.position);
    else {
      window.history.replaceState({ ...window.history.state, vaultsyncdFiles: { session, position: 0 } }, "");
      setPosition(0); setLast(0);
    }
  }, [location, session]);
  function navigate(path: string, replace = false) {
    if (path === location) return;
    const next = replace ? position : position + 1;
    const state = { ...window.history.state, vaultsyncdFiles: { session, position: next } };
    if (replace) window.history.replaceState(state, "", `#${path}`);
    else { window.history.pushState(state, "", `#${path}`); setLast(next); }
    setPosition(next);
    window.dispatchEvent(new HashChangeEvent("hashchange"));
  }
  return { navigate, canBack: position > 0, canForward: position < last };
}

function RootBrowser({ location }: { location: string }) {
  const auth = useAuth();
  const client = auth.client!;
  const route = useMemo(() => parseFilesLocation(location), [location]);
  const navigation = useFolderNavigation(location);
  const [roots, setRoots] = useState<RootSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setError("");
    client.request(options => listRoots(options, { signal: controller.signal })).then(result => {
      if (!controller.signal.aborted) setRoots(result.roots);
    }).catch(cause => {
      if (controller.signal.aborted) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "读取 Root 失败，请重试。");
    }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [client, revision]);
  useEffect(() => {
    if (!loading && !error && !route.rootId && !route.error && roots.length) navigation.navigate(filesLocation(roots[0].root_id), true);
  }, [loading, error, route.rootId, route.error, roots, location]);
  const root = roots.find(item => item.root_id === route.rootId);
  const rootError = route.error || error || (!loading && route.rootId && !root ? "Root 不存在或未获授权。" : "");
  const rootPicker = <Select aria-label="浏览 Root" placeholder="选择 Root" value={root?.root_id ?? null} allowDeselect={false} searchable
    nothingFoundMessage="没有匹配的 Root" disabled={loading || !roots.length}
    data={roots.map(item => ({ value: item.root_id, label: `${item.name || "未命名 Root"} · ${item.root_id.slice(0, 8)}` }))}
    onChange={value => { if (value) navigation.navigate(filesLocation(value)); }} />;
  return <DirectoryBrowser key={root?.root_id ?? "none"} root={root} rootPicker={rootPicker} segments={route.segments}
    navigation={navigation} rootError={rootError} rootsLoading={loading} retryRoots={() => setRevision(value => value + 1)} />;
}

type ToolIcon = Parameters<typeof FileIcon>[0]["name"];

function Tool({ label, icon, disabled, onClick }: { label: string; icon: ToolIcon; disabled?: boolean; onClick: () => void }) {
  return <ActionIcon variant="default" size="md" aria-label={label} title={label} disabled={disabled} onClick={onClick}><FileIcon name={icon} /></ActionIcon>;
}

function DirectoryBrowser({ root, rootPicker, segments, navigation, rootError, rootsLoading, retryRoots }: {
  root?: RootSummary; rootPicker: ReactNode; segments: string[]; navigation: ReturnType<typeof useFolderNavigation>;
  rootError: string; rootsLoading: boolean; retryRoots: () => void;
}) {
  const auth = useAuth();
  const client = auth.client!;
  const actions = useFileActions();
  const uploadInput = useRef<HTMLInputElement>(null);
  const [snapshot, setSnapshot] = useState<DirectorySnapshot | null>(null);
  const [selected, setSelected] = useState<SnapshotEntry | null>(null);
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const [menuEntryId, setMenuEntryId] = useState<string | null>(null);
  const [loading, setLoading] = useState(!!root);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  const currentPath = "/" + segments.join("/");
  const pathRef = useRef(currentPath);
  pathRef.current = currentPath;
  const [address, setAddress] = useState(currentPath);
  const [addressError, setAddressError] = useState("");
  useEffect(() => { setAddress(currentPath); setAddressError(""); setQuery(""); setPage(1); setSelected(null); setMenuEntryId(null); }, [currentPath]);
  useEffect(() => {
    if (!root) return;
    const controller = new AbortController();
    const previousDirectory = snapshot ? directoryAtPath(snapshot, segments) : undefined;
    const previousPath = currentPath;
    setLoading(true); setSnapshot(null); setSelected(null); setError(""); setPage(1);
    readDirectorySnapshot(client, root.root_id, controller.signal).then(result => {
      if (!controller.signal.aborted) {
        setSnapshot(result);
        // 当前目录或祖先改名后按身份更新地址；读取期间的新导航优先。
        const current = previousDirectory && result.entries.get(previousDirectory.entry_id);
        if (current && pathRef.current === previousPath) navigation.navigate(filesLocation(root.root_id, ancestry(result, current).slice(1).map(item => item.name)), true);
      }
    }).catch(cause => {
      if (controller.signal.aborted) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "目录读取失败。");
    }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [client, root?.root_id, revision, actions.revision]);
  const directory = snapshot ? directoryAtPath(snapshot, segments) : undefined;
  const children = useMemo(() => {
    const term = query.trim().toLocaleLowerCase("zh-CN");
    return (snapshot?.children.get(directory?.entry_id ?? "") ?? []).filter(entry => entry.name.toLocaleLowerCase("zh-CN").includes(term));
  }, [snapshot, directory?.entry_id, query]);
  function navigate(entry: SnapshotEntry) {
    navigation.navigate(filesLocation(root!.root_id, ancestry(snapshot!, entry).slice(1).map(item => item.name)));
  }
  /** 复制 Root 相对路径；剪贴板不可用时如实提示，不假装成功。 */
  async function copyPath(entry: SnapshotEntry) {
    if (!snapshot) return;
    const path = ancestry(snapshot, entry).map(item => item.name).join("/") || "/";
    try {
      await navigator.clipboard.writeText(path);
      showFeedback(`已复制路径 ${path}`);
    } catch {
      showFeedback(`复制失败，请手动复制路径 ${path}`, "red");
    }
  }
  function goToAddress() {
    const parts = address.replace(/^\/+|\/+$/g, "").split("/");
    const names = parts.length === 1 && parts[0] === "" ? [] : parts;
    const path = filesLocation(root!.root_id, names);
    if (parseFilesLocation(path).error) setAddressError("请输入 Root 内的相对路径。");
    else { setAddressError(""); navigation.navigate(path); }
  }
  const context = root && snapshot && directory ? { rootId: root.root_id, snapshot, parent: directory } : null;
  const writable = !!context && !loading && root?.health !== "unavailable" && !actions.locked;
  const pasteInto = (entry?: SnapshotEntry) => { if (context) actions.paste({ ...context, parent: entry?.kind === "directory" ? entry : context.parent }); };
  const rootName = root?.name || "Root 根目录";
  function entryMenu(entry: SnapshotEntry, child: ReactElement, source = "list") {
    const key = `${source}:${entry.entry_id}`;
    return <EntryMenu key={key} entry={entry} context={{ rootId: root!.root_id, snapshot: snapshot!, parent: directory ?? snapshot!.root }}
      writable={!loading && root?.health !== "unavailable" && !actions.locked}
      opened={menuEntryId === key} onChange={opened => setMenuEntryId(opened ? key : null)}
      navigate={navigate} properties={setSelected} copyPath={entry => void copyPath(entry)}>{child}</EntryMenu>;
  }
  return <Paper withBorder className="files-browser">
    <aside className="files-sidebar" aria-label="目录面板">
      <div className="files-root-picker">{rootPicker}</div>
      <div className="files-tree-scroll">{snapshot && <DirectoryTree snapshot={snapshot} directory={directory} rootName={rootName} navigate={navigate} renderMenu={(entry, child) => entryMenu(entry, child, "tree")} />}</div>
    </aside>
    <section className="files-main" aria-label="文件列表面板">
      <div className="files-toolbar" role="toolbar" aria-label="文件浏览器工具栏">
        <Group gap={4} wrap="nowrap">
          <Tool label="后退" icon="back" disabled={!navigation.canBack} onClick={() => window.history.back()} />
          <Tool label="前进" icon="forward" disabled={!navigation.canForward} onClick={() => window.history.forward()} />
          <Tool label="返回上级" icon="up" disabled={!root || !segments.length || loading} onClick={() => navigation.navigate(filesLocation(root!.root_id, segments.slice(0, -1)))} />
          <Tool label="刷新文件列表" icon="refresh" disabled={loading || rootsLoading} onClick={() => root ? setRevision(value => value + 1) : retryRoots()} />
        </Group>
        <form className="files-address" onSubmit={event => { event.preventDefault(); goToAddress(); }}>
          <TextInput aria-label="目录地址" value={address} disabled={!root} placeholder="/" error={!!addressError}
            onChange={event => setAddress(event.currentTarget.value)} onKeyDown={event => { if (event.key === "Escape") { setAddress(currentPath); setAddressError(""); } }}
            rightSection={<ActionIcon type="submit" variant="subtle" aria-label="转到目录" title="转到目录" disabled={!root}><FileIcon name="forward" /></ActionIcon>} />
        </form>
        <TextInput aria-label="筛选当前目录" placeholder="筛选名称" className="files-filter" value={query} disabled={!directory || loading}
          onChange={event => { setQuery(event.currentTarget.value); setPage(1); }} />
        <input ref={uploadInput} type="file" multiple hidden aria-label="选择上传文件" onChange={event => {
          const files = Array.from(event.currentTarget.files ?? []);
          event.currentTarget.value = "";
          if (context && files.length) actions.upload(context, files);
        }} />
        <Tool label="上传文件" icon="upload" disabled={!writable} onClick={() => uploadInput.current?.click()} />
        <Tool label="粘贴" icon="paste" disabled={!writable || !actions.clipboard} onClick={() => pasteInto()} />
        <Tool label="当前目录属性" icon="properties" disabled={!directory || loading} onClick={() => setSelected(directory!)} />
      </div>
      {root?.health === "unavailable" && <Alert color="yellow">Root 当前不可用，清单仅代表已提交的元数据。</Alert>}
      {(loading || rootsLoading) && <Text p="md" role="status">正在读取目录…</Text>}
      <Feedback message={rootError} />
      <Feedback message={addressError} />
      <Feedback message={error ? `${error} 未能读取完整目录，请刷新重试。` : ""} />
      {!rootsLoading && !root && !rootError && <Text p="md">尚无授权 Root，请先在 Root 目录页面注册目录。</Text>}
      <Feedback message={snapshot && !directory ? "目录不存在或尚未采集，请检查地址或刷新。" : ""} />
      <Menu opened={menuEntryId === "background"} onChange={opened => setMenuEntryId(opened ? "background" : null)} shadow="md" width={180}>
      <Menu.ContextMenu><div className="files-list" tabIndex={0} aria-label="当前目录空白区域" onKeyDown={event => { if (event.target === event.currentTarget) openMenuByKeyboard(event); }}>
        {snapshot && directory && <Table aria-label="文件与目录" highlightOnHover verticalSpacing="xs" horizontalSpacing="sm" className="files-table">
          <Table.Thead><Table.Tr><Table.Th>名称</Table.Th><Table.Th>大小</Table.Th><Table.Th>修改日期</Table.Th></Table.Tr></Table.Thead>
          <Table.Tbody>{children.length ? children.slice((page - 1) * 100, page * 100).map(entry =>
            entryMenu(entry, <Table.Tr onContextMenu={event => event.stopPropagation()} onTouchStart={event => event.stopPropagation()}>
                  <Table.Td><Anchor component="button" ta="left" underline="never" className="files-entry-link" title={entry.name} onKeyDown={openMenuByKeyboard}
                    onClick={() => entry.kind === "directory" ? navigate(entry) : setSelected(entry)}><Group gap="xs" wrap="nowrap"><FileIcon name={entry.kind === "directory" ? "folder" : "file"} /><span className="files-entry-name">{entry.name}</span></Group></Anchor></Table.Td>
                  <Table.Td className="files-size">{fileSize(entry)}</Table.Td><Table.Td className="files-modified">{fileModified(entry)}</Table.Td>
                </Table.Tr>)) : <Table.Tr><Table.Td colSpan={3}><Text ta="center" c="dimmed" py="md">{query.trim() ? "当前目录没有匹配的名称。" : "此目录在已提交快照中为空。"}</Text></Table.Td></Table.Tr>}</Table.Tbody>
        </Table>}
      </div></Menu.ContextMenu>
      <Menu.Dropdown>
        <Menu.Item disabled={!writable} leftSection={<FileIcon name="upload" />} onClick={() => uploadInput.current?.click()}>上传文件</Menu.Item>
        <Menu.Item disabled={!writable || !actions.clipboard} leftSection={<FileIcon name="paste" />} onClick={() => pasteInto()}>粘贴</Menu.Item>
      </Menu.Dropdown></Menu>
      {children.length > 100 && <Pagination className="files-pagination" aria-label="文件列表分页" total={Math.ceil(children.length / 100)} value={page} onChange={setPage} size="sm" siblings={0} />}
    </section>
    <Dialog opened={!!snapshot && !!selected} title="文件属性" onClose={() => setSelected(null)}>
      {snapshot && selected && <Stack gap="sm">
        {[
          ["名称", selected.name || rootName], ["类型", selected.kind === "directory" ? "文件夹" : "文件"],
          ["Root", rootName], ["Root ID", root!.root_id],
          ["相对路径", ancestry(snapshot, selected).map(entry => entry.name).join("/") || "/"], ["大小", fileSizeDetail(selected)],
          ["修改日期", fileModified(selected)],
          ...(selected.kind === "directory" ? [["直接子项", String(snapshot.children.get(selected.entry_id)?.length ?? 0)]] : []),
          ["修订号", selected.revision], ["条目 ID", selected.entry_id], ["内容版本", selected.version_id ?? "—"], ["SHA-256", selected.hash ?? "—"],
        ].map(([label, value]) => <Stack gap={2} key={label}><Text size="xs" c="dimmed">{label}</Text><Code block className="files-property-value">{value}</Code></Stack>)}
      </Stack>}
    </Dialog>
  </Paper>;
}
