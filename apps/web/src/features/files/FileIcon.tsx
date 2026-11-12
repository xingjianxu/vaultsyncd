import { ArrowLeft, ArrowRight, ArrowUp, ClipboardPaste, CloudDownload, CloudUpload, Copy, FolderPlus, History, Info, Pencil, RefreshCw, Scissors, Trash2 } from "lucide-react";

const actions = {
  back: ArrowLeft, forward: ArrowRight, up: ArrowUp, refresh: RefreshCw,
  properties: Info, history: History, rename: Pencil, copy: Copy, cut: Scissors,
  paste: ClipboardPaste, upload: CloudUpload, download: CloudDownload, "folder-new": FolderPlus, delete: Trash2,
};
const files = {
  folder: "M3 7h6l2 2h10v11H3z M3 7V4h6l2 3",
  file: "M6 3h8l4 4v14H6z M14 3v5h4",
  "folder-open": "M3 9V4h6l2 3h10v3 M3 9h6l2 2h11l-4 9H3z",
};

/** 保留现有文件/目录图标与调用入口；通用操作统一使用 Lucide。 */
export function FileIcon({ name }: { name: keyof typeof actions | keyof typeof files }) {
  if (name === "file" || name === "folder" || name === "folder-open") {
    return <svg aria-hidden="true" className="files-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={files[name]} /></svg>;
  }
  const Component = actions[name];
  return <Component aria-hidden="true" className="files-icon" size={20} strokeWidth={1.6} />;
}
