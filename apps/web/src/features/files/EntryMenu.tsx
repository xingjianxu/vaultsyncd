import type { KeyboardEvent, ReactElement } from "react";
import { Menu } from "@mantine/core";
import type { SnapshotEntry } from "@vaultsyncd/api-client";
import { useFileActions, type ActionDirectory } from "./FileActions";
import { FileIcon } from "./FileIcon";

/** 名称按钮或目录树标签通过菜单键/Shift+F10 使用同一右键入口。 */
export function openMenuByKeyboard(event: KeyboardEvent<HTMLElement>) {
  if (event.key !== "ContextMenu" && !(event.key === "F10" && event.shiftKey)) return;
  event.preventDefault();
  const box = event.currentTarget.getBoundingClientRect();
  event.currentTarget.dispatchEvent(new MouseEvent("contextmenu", {
    bubbles: true, cancelable: true, clientX: box.left + 12, clientY: box.top + box.height / 2,
  }));
}

export function EntryMenu({ children, entry, context, writable, opened, onChange, navigate, properties, history, copyPath }: {
  children: ReactElement; entry: SnapshotEntry; context: ActionDirectory; writable: boolean;
  opened: boolean; onChange: (opened: boolean) => void;
  navigate: (entry: SnapshotEntry) => void; properties: (entry: SnapshotEntry) => void; copyPath: (entry: SnapshotEntry) => void;
  history: (entry: SnapshotEntry) => void;
}) {
  const actions = useFileActions();
  const mutable = writable && entry.entry_id !== context.snapshot.vault.entry_id;
  return <Menu opened={opened} onChange={onChange} shadow="md" width={180} position="bottom-start" offset={2}>
    <Menu.ContextMenu>{children}</Menu.ContextMenu>
    <Menu.Dropdown onKeyDown={event => event.stopPropagation()}>
      {entry.kind === "directory" && <Menu.Item leftSection={<FileIcon name="folder-open" />} onClick={() => navigate(entry)}>打开</Menu.Item>}
      {entry.kind === "directory" && <Menu.Item disabled={!writable} leftSection={<FileIcon name="folder-new" />}
        onClick={() => actions.createDirectory({ ...context, parent: entry })}>新建目录</Menu.Item>}
      {entry.kind === "file" && <Menu.Item disabled={!writable || !entry.version_id || !entry.hash || entry.size === undefined}
        leftSection={<FileIcon name="download" />} onClick={() => actions.download(context, entry)}>下载</Menu.Item>}
      <Menu.Item leftSection={<FileIcon name="properties" />} onClick={() => properties(entry)}>属性</Menu.Item>
      {entry.kind === "file" && <Menu.Item leftSection={<FileIcon name="history" />} onClick={() => history(entry)}>版本历史</Menu.Item>}
      <Menu.Item leftSection={<FileIcon name="copy" />} onClick={() => copyPath(entry)}>复制路径</Menu.Item>
      <Menu.Divider />
      <Menu.Item disabled={!mutable} leftSection={<FileIcon name="copy" />} onClick={() => actions.stage(context, entry, "copy")}>复制</Menu.Item>
      <Menu.Item disabled={!mutable} leftSection={<FileIcon name="cut" />} onClick={() => actions.stage(context, entry, "cut")}>剪切</Menu.Item>
      <Menu.Item disabled={!writable || !actions.clipboard} leftSection={<FileIcon name="paste" />} onClick={() => actions.paste({ ...context, parent: entry.kind === "directory" ? entry : context.parent })}>粘贴</Menu.Item>
      <Menu.Item disabled={!mutable} leftSection={<FileIcon name="rename" />} onClick={() => actions.rename(context, entry)}>重命名</Menu.Item>
      <Menu.Item color="red" disabled={!mutable} leftSection={<FileIcon name="delete" />} onClick={() => actions.remove(context, entry)}>删除</Menu.Item>
    </Menu.Dropdown>
  </Menu>;
}
