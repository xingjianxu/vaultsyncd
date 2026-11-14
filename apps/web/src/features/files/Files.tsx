import { useEffect, useMemo, useRef, useState, type ReactElement, type ReactNode } from "react";
import { listVaults, type VaultSummary, type SnapshotEntry } from "@vaultsyncd/api-client";
import { ActionIcon, Alert, Anchor, Code, Group, Menu, Pagination, Paper, Select, Stack, Table, Text, TextInput, UnstyledButton } from "@mantine/core";
import { ArrowDown, ArrowUp, ArrowUpDown } from "lucide-react";
import { EntryMenu, openMenuByKeyboard } from "./EntryMenu";
import { DirectoryTree } from "./DirectoryTree";
import { useFileActions } from "./FileActions";
import { FileIcon } from "./FileIcon";
import { FileHistory } from "./FileHistory";
import { FilePreview } from "./FilePreview";
import { previewKind } from "./preview";
import { Dialog, EmptyState } from "../../components/ui";
import { Feedback, showFeedback } from "../../components/Feedback";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { ancestry, DEFAULT_FILE_SORT, directoryAtPath, filesLocation, fileModified, fileSize, fileSizeDetail, parseFilesLocation, readDirectorySnapshot, sortDirectoryEntries, type DirectorySnapshot, type FileSort, type FileSortColumn } from "./browser";

export function FilesPanel({ location }: { location: string }) {
  const auth = useAuth();
  if (!auth.account || !auth.client) return <EmptyState title="请先登录">从账号与会话页面登录后浏览授权 Vault。</EmptyState>;
  return <VaultBrowser key={auth.account.user_id} location={location} />;
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

function VaultBrowser({ location }: { location: string }) {
  const auth = useAuth();
  const client = auth.client!;
  const route = useMemo(() => parseFilesLocation(location), [location]);
  const navigation = useFolderNavigation(location);
  const [vaults, setVaults] = useState<VaultSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setError("");
    client.request(options => listVaults(options, { signal: controller.signal })).then(result => {
      if (!controller.signal.aborted) setVaults(result.vaults);
    }).catch(cause => {
      if (controller.signal.aborted) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "读取 Vault 失败，请重试。");
    }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [client, revision]);
  useEffect(() => {
    if (!loading && !error && !route.vaultId && !route.error && vaults.length) navigation.navigate(filesLocation(vaults[0].vault_id), true);
  }, [loading, error, route.vaultId, route.error, vaults, location]);
  const vault = route.vaultId ? vaults.find(item => item.vault_id === route.vaultId) : route.error ? undefined : vaults[0];
  const vaultError = route.error || error || (!loading && route.vaultId && !vault ? "Vault 不存在或未获授权。" : "");
  const vaultPicker = <Select aria-label="浏览 Vault" placeholder="选择 Vault" value={vault?.vault_id ?? null} allowDeselect={false} searchable
    nothingFoundMessage="没有匹配的 Vault" disabled={loading || !vaults.length}
    data={vaults.map(item => ({ value: item.vault_id, label: `${item.name || "未命名 Vault"} · ${item.vault_id.slice(0, 8)}` }))}
    onChange={value => { if (value) navigation.navigate(filesLocation(value)); }} />;
  return <DirectoryBrowser key={vault?.vault_id ?? "none"} vault={vault} vaultPicker={vaultPicker} segments={route.segments}
    navigation={navigation} vaultError={vaultError} vaultsLoading={loading} retryVaults={() => setRevision(value => value + 1)} />;
}

type ToolIcon = Parameters<typeof FileIcon>[0]["name"];

function Tool({ label, icon, disabled, onClick }: { label: string; icon: ToolIcon; disabled?: boolean; onClick: () => void }) {
  return <ActionIcon variant="default" size="md" aria-label={label} title={label} disabled={disabled} onClick={onClick}><FileIcon name={icon} /></ActionIcon>;
}

function DirectoryBrowser({ vault, vaultPicker, segments, navigation, vaultError, vaultsLoading, retryVaults }: {
  vault?: VaultSummary; vaultPicker: ReactNode; segments: string[]; navigation: ReturnType<typeof useFolderNavigation>;
  vaultError: string; vaultsLoading: boolean; retryVaults: () => void;
}) {
  const auth = useAuth();
  const client = auth.client!;
  const actions = useFileActions();
  const uploadInput = useRef<HTMLInputElement>(null);
  const [snapshot, setSnapshot] = useState<DirectorySnapshot | null>(null);
  const [selected, setSelected] = useState<SnapshotEntry | null>(null);
  const [historyEntry, setHistoryEntry] = useState<SnapshotEntry | null>(null);
  const [previewEntry, setPreviewEntry] = useState<SnapshotEntry | null>(null);
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const [sort, setSort] = useState<FileSort>(DEFAULT_FILE_SORT);
  const [menuEntryId, setMenuEntryId] = useState<string | null>(null);
  const [loading, setLoading] = useState(!!vault);
  const [error, setError] = useState("");
  const [revision, setRevision] = useState(0);
  const currentPath = "/" + segments.join("/");
  const pathRef = useRef(currentPath);
  pathRef.current = currentPath;
  const [address, setAddress] = useState(currentPath);
  const [addressError, setAddressError] = useState("");
  useEffect(() => { setAddress(currentPath); setAddressError(""); setQuery(""); setPage(1); setSelected(null); setHistoryEntry(null); setPreviewEntry(null); setMenuEntryId(null); }, [currentPath]);
  useEffect(() => {
    if (!vault) return;
    const controller = new AbortController();
    const previousDirectory = snapshot ? directoryAtPath(snapshot, segments) : undefined;
    const previousPath = currentPath;
    setLoading(true); setSnapshot(null); setSelected(null); setPreviewEntry(null); setError(""); setPage(1);
    readDirectorySnapshot(client, vault.vault_id, controller.signal).then(result => {
      if (!controller.signal.aborted) {
        setSnapshot(result);
        // 当前目录或祖先改名后按身份更新地址；读取期间的新导航优先。
        const current = previousDirectory && result.entries.get(previousDirectory.entry_id);
        if (current && pathRef.current === previousPath) navigation.navigate(filesLocation(vault.vault_id, ancestry(result, current).slice(1).map(item => item.name)), true);
      }
    }).catch(cause => {
      if (controller.signal.aborted) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "目录读取失败。");
    }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [client, vault?.vault_id, revision, actions.revision]);
  const directory = snapshot ? directoryAtPath(snapshot, segments) : undefined;
  const children = useMemo(() => {
    const term = query.trim().toLocaleLowerCase("zh-CN");
    return sortDirectoryEntries((snapshot?.children.get(directory?.entry_id ?? "") ?? []).filter(entry => entry.name.toLocaleLowerCase("zh-CN").includes(term)), sort);
  }, [snapshot, directory?.entry_id, query, sort]);
  function sortHeader(column: FileSortColumn, label: string) {
    const active = sort.column === column;
    const next = active && sort.direction === "ascending" ? "descending" : "ascending";
    const Icon = active ? sort.direction === "ascending" ? ArrowUp : ArrowDown : ArrowUpDown;
    return <Table.Th aria-sort={active ? sort.direction : "none"}>
      <UnstyledButton className="files-sort-button" aria-label={`按${label}排序`} title={`${label}：切换为${next === "ascending" ? "升序" : "降序"}`} onClick={() => {
        setSort({ column, direction: next }); setPage(1); setMenuEntryId(null);
      }}>{label}<Icon size={14} aria-hidden="true" /></UnstyledButton>
    </Table.Th>;
  }
  function navigate(entry: SnapshotEntry) {
    navigation.navigate(filesLocation(vault!.vault_id, ancestry(snapshot!, entry).slice(1).map(item => item.name)));
  }
  /** 复制 Vault 相对路径；剪贴板不可用时如实提示，不假装成功。 */
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
    const path = filesLocation(vault!.vault_id, names);
    if (parseFilesLocation(path).error) setAddressError("请输入 Vault 内的相对路径。");
    else { setAddressError(""); navigation.navigate(path); }
  }
  const context = vault && snapshot && directory ? { vaultId: vault.vault_id, snapshot, parent: directory } : null;
  const writable = !!context && !loading && vault?.health !== "unavailable" && !actions.locked;
  const pasteInto = (entry?: SnapshotEntry) => { if (context) actions.paste({ ...context, parent: entry?.kind === "directory" ? entry : context.parent }); };
  const vaultName = vault?.name || "Vault 根目录";
  function entryMenu(entry: SnapshotEntry, child: ReactElement, source = "list") {
    const key = `${source}:${entry.entry_id}`;
    return <EntryMenu key={key} entry={entry} context={{ vaultId: vault!.vault_id, snapshot: snapshot!, parent: directory ?? snapshot!.vault }}
      writable={!loading && vault?.health !== "unavailable" && !actions.locked}
      opened={menuEntryId === key} onChange={opened => setMenuEntryId(opened ? key : null)}
      navigate={navigate} properties={setSelected} preview={entry => setPreviewEntry({ ...entry })} history={setHistoryEntry} copyPath={entry => void copyPath(entry)}>{child}</EntryMenu>;
  }
  return <Paper withBorder className="files-browser">
    <aside className="files-sidebar" aria-label="目录面板">
      <div className="files-root-picker">{vaultPicker}</div>
      <div className="files-tree-scroll">{snapshot && <DirectoryTree snapshot={snapshot} directory={directory} vaultName={vaultName} navigate={navigate} renderMenu={(entry, child) => entryMenu(entry, child, "tree")} />}</div>
    </aside>
    <section className="files-main" aria-label="文件列表面板">
      <div className="files-toolbar" role="toolbar" aria-label="文件浏览器工具栏">
        <Group gap={4} wrap="nowrap">
          <Tool label="后退" icon="back" disabled={!navigation.canBack} onClick={() => window.history.back()} />
          <Tool label="前进" icon="forward" disabled={!navigation.canForward} onClick={() => window.history.forward()} />
          <Tool label="返回上级" icon="up" disabled={!vault || !segments.length || loading} onClick={() => navigation.navigate(filesLocation(vault!.vault_id, segments.slice(0, -1)))} />
          <Tool label="刷新文件列表" icon="refresh" disabled={loading || vaultsLoading} onClick={() => vault ? setRevision(value => value + 1) : retryVaults()} />
        </Group>
        <form className="files-address" onSubmit={event => { event.preventDefault(); goToAddress(); }}>
          <TextInput aria-label="目录地址" value={address} disabled={!vault} placeholder="/" error={!!addressError}
            onChange={event => setAddress(event.currentTarget.value)} onKeyDown={event => { if (event.key === "Escape") { setAddress(currentPath); setAddressError(""); } }}
            rightSection={<ActionIcon type="submit" variant="subtle" aria-label="转到目录" title="转到目录" disabled={!vault}><FileIcon name="forward" /></ActionIcon>} />
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
      {vault?.health === "unavailable" && <Alert color="yellow">Vault 当前不可用，清单仅代表已提交的元数据。</Alert>}
      {(loading || vaultsLoading) && <Text p="md" role="status">正在读取目录…</Text>}
      <Feedback message={vaultError} />
      <Feedback message={addressError} />
      <Feedback message={error ? `${error} 未能读取完整目录，请刷新重试。` : ""} />
      {!vaultsLoading && !vault && !vaultError && <Text p="md">尚无授权 Vault，请先在 Vault 目录页面注册目录。</Text>}
      <Feedback message={snapshot && !directory ? "目录不存在或尚未采集，请检查地址或刷新。" : ""} />
      <Menu opened={menuEntryId === "background"} onChange={opened => setMenuEntryId(opened ? "background" : null)} shadow="md" width={180}>
      <Menu.ContextMenu><div className="files-list" tabIndex={0} aria-label="当前目录空白区域" onKeyDown={event => { if (event.target === event.currentTarget) openMenuByKeyboard(event); }}>
        {snapshot && directory && <Table aria-label="文件与目录" highlightOnHover verticalSpacing="xs" horizontalSpacing="sm" className="files-table">
          <Table.Thead><Table.Tr>{sortHeader("name", "名称")}{sortHeader("size", "大小")}{sortHeader("modified_at", "修改日期")}</Table.Tr></Table.Thead>
          <Table.Tbody>{children.length ? children.slice((page - 1) * 100, page * 100).map(entry =>
            entryMenu(entry, <Table.Tr onContextMenu={event => event.stopPropagation()} onTouchStart={event => event.stopPropagation()}>
                  <Table.Td><Anchor component="button" ta="left" underline="never" className="files-entry-link" title={entry.name} onKeyDown={openMenuByKeyboard}
                    onClick={() => entry.kind === "directory" ? navigate(entry) : previewKind(entry) ? setPreviewEntry({ ...entry }) : setSelected(entry)}><Group gap="xs" wrap="nowrap"><FileIcon name={entry.kind === "directory" ? "folder" : "file"} filename={entry.name} /><span className="files-entry-name">{entry.name}</span></Group></Anchor></Table.Td>
                  <Table.Td className="files-size">{fileSize(entry)}</Table.Td><Table.Td className="files-modified">{fileModified(entry)}</Table.Td>
                </Table.Tr>)) : <Table.Tr><Table.Td colSpan={3}><Text ta="center" c="dimmed" py="md">{query.trim() ? "当前目录没有匹配的名称。" : "此目录在已提交快照中为空。"}</Text></Table.Td></Table.Tr>}</Table.Tbody>
        </Table>}
      </div></Menu.ContextMenu>
      <Menu.Dropdown>
        <Menu.Item disabled={!writable} leftSection={<FileIcon name="folder-new" />} onClick={() => { if (context) actions.createDirectory(context); }}>新建目录</Menu.Item>
        <Menu.Item disabled={!writable} leftSection={<FileIcon name="upload" />} onClick={() => uploadInput.current?.click()}>上传文件</Menu.Item>
        <Menu.Item disabled={!writable || !actions.clipboard} leftSection={<FileIcon name="paste" />} onClick={() => pasteInto()}>粘贴</Menu.Item>
      </Menu.Dropdown></Menu>
      {children.length > 100 && <Pagination className="files-pagination" aria-label="文件列表分页" total={Math.ceil(children.length / 100)} value={page} onChange={setPage} size="sm" siblings={0} />}
    </section>
    <FilePreview entry={previewEntry} vaultId={vault?.vault_id ?? ""} onClose={() => setPreviewEntry(null)} />
    {vault && historyEntry && <FileHistory key={`${vault.vault_id}:${historyEntry.entry_id}`} vaultId={vault.vault_id} entry={historyEntry} onClose={() => setHistoryEntry(null)} />}
    <Dialog opened={!!snapshot && !!selected} title="文件属性" onClose={() => setSelected(null)}>
      {snapshot && selected && <Stack gap="sm">
        {[
          ["名称", selected.name || vaultName], ["类型", selected.kind === "directory" ? "文件夹" : "文件"],
          ["Vault", vaultName], ["Vault ID", vault!.vault_id],
          ["相对路径", ancestry(snapshot, selected).map(entry => entry.name).join("/") || "/"], ["大小", fileSizeDetail(selected)],
          ["修改日期", fileModified(selected)],
          ...(selected.kind === "directory" ? [["直接子项", String(snapshot.children.get(selected.entry_id)?.length ?? 0)]] : []),
          ["修订号", selected.revision], ["条目 ID", selected.entry_id], ["内容版本", selected.version_id ?? "—"], ["SHA-256", selected.hash ?? "—"],
        ].map(([label, value]) => <Stack gap={2} key={label}><Text size="xs" c="dimmed">{label}</Text><Code block className="files-property-value">{value}</Code></Stack>)}
      </Stack>}
    </Dialog>
  </Paper>;
}
