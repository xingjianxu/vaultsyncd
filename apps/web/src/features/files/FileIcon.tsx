import { ArrowLeft, ArrowRight, ArrowUp, ClipboardPaste, CloudDownload, CloudUpload, Copy, Database, FileArchive, FileAudio, FileChartColumn, FileCode2, FileCog, FileImage, FileJson, FileSpreadsheet, FileTerminal, FileText, FileType, FileType2, FileVideo, FolderPlus, History, Info, Pencil, RefreshCw, Scissors, Trash2 } from "lucide-react";

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

const fileTypes = {
  text: FileText, pdf: FileType, spreadsheet: FileSpreadsheet, presentation: FileChartColumn,
  image: FileImage, audio: FileAudio, video: FileVideo, archive: FileArchive,
  code: FileCode2, json: FileJson, config: FileCog, script: FileTerminal,
  font: FileType2, database: Database,
};
const extensionGroups: [keyof typeof fileTypes, string][] = [
  ["text", "txt text log md markdown rst rtf doc docx odt pages tex"],
  ["pdf", "pdf"],
  ["spreadsheet", "xls xlsx xlsm xlsb ods csv tsv numbers"],
  ["presentation", "ppt pptx pps ppsx odp key"],
  ["image", "png jpg jpeg gif webp bmp avif svg ico tif tiff heic heif psd raw"],
  ["audio", "mp3 wav flac aac ogg oga opus m4a wma aiff mid midi"],
  ["video", "mp4 mkv avi mov webm m4v wmv mpg mpeg flv 3gp ogv"],
  ["archive", "zip 7z rar tar gz gzip bz2 xz zst zstd tgz tbz2 txz cab iso"],
  ["code", "js jsx mjs cjs ts tsx mts cts html htm css scss sass less xml vue svelte py rs go c h cpp hpp cc cs java kt kts swift rb php sql lua r dart ex exs elm clj nix"],
  ["json", "json jsonc json5 ipynb"],
  ["config", "yaml yml toml ini cfg conf properties env"],
  ["script", "sh bash zsh fish ps1 psm1 bat cmd"],
  ["font", "ttf otf woff woff2 eot"],
  ["database", "db sqlite sqlite3 mdb accdb"],
];
const extensionIcons = new Map(extensionGroups.flatMap(([type, extensions]) => extensions.split(" ").map(extension => [extension, type] as const)));

/** 只按最后一个后缀推断展示类型，不代表内容可信或可预览；单独的点文件不视为后缀。 */
export function fileTypeIcon(filename: string): keyof typeof fileTypes | undefined {
  const dot = filename.lastIndexOf(".");
  return dot > 0 ? extensionIcons.get(filename.slice(dot + 1).toLowerCase()) : undefined;
}

/** 保留文件/目录与操作入口；可选文件名仅影响普通文件的展示图标。 */
export function FileIcon({ name, filename }: { name: keyof typeof actions | keyof typeof files; filename?: string }) {
  const type = name === "file" && filename ? fileTypeIcon(filename) : undefined;
  if (type) {
    const Component = fileTypes[type];
    return <Component aria-hidden="true" className="files-icon" size={20} strokeWidth={1.6} />;
  }
  if (name === "file" || name === "folder" || name === "folder-open") {
    return <svg aria-hidden="true" className="files-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={files[name]} /></svg>;
  }
  const Component = actions[name];
  return <Component aria-hidden="true" className="files-icon" size={20} strokeWidth={1.6} />;
}
