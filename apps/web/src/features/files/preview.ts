import type { SnapshotEntry } from "@vaultsyncd/api-client";
import type { BrowserAuth } from "../../services/auth";
import { downloadFile } from "./download";

export const MAX_PREVIEW_BYTES = 50 * 1024 * 1024;
export const MAX_PREVIEW_PIXELS = 16 * 1024 * 1024;
export const MAX_CANVAS_PIXELS = 8 * 1024 * 1024;
export const MAX_CANVAS_EDGE = 8192;
export type PreviewKind = "pdf" | "png" | "jpeg" | "gif" | "webp" | "bmp" | "avif";
const extensions: Record<string, PreviewKind> = { pdf: "pdf", png: "png", jpg: "jpeg", jpeg: "jpeg", gif: "gif", webp: "webp", bmp: "bmp", avif: "avif" };

/** 扩展名仅决定入口；实际解码前还检查内容签名和尺寸。SVG/HTML 永不进入渲染器。 */
export function previewKind(entry: SnapshotEntry): PreviewKind | null {
  const extension = entry.name.split(".").at(-1)?.toLowerCase() ?? "";
  return entry.kind === "file" && Object.hasOwn(extensions, extension) ? extensions[extension] : null;
}

export async function downloadPreview(client: BrowserAuth, vaultId: string, source: SnapshotEntry, signal: AbortSignal,
  progress: (bytes: number, total: number) => void) {
  const entry = { ...source };
  if (!previewKind(entry)) throw new Error("此文件类型不支持在线预览。");
  // 必须在任何网络请求之前拒绝缺失/非法元数据与超限文件；downloadFile 继续校验固定版本、长度和 SHA-256。
  if (!entry.size || !/^(0|[1-9][0-9]*)$/.test(entry.size)) throw new Error("文件缺少有效大小，请刷新。");
  if (BigInt(entry.size) > BigInt(MAX_PREVIEW_BYTES)) throw new Error("在线预览仅支持不超过 50 MiB 的文件，请下载后查看。");
  return downloadFile(client, vaultId, entry, signal, progress);
}

export function boundedCanvas(width: number, height: number, zoom = 1) {
  if (![width, height, zoom].every(value => Number.isFinite(value) && value > 0)) throw new Error("无效的预览尺寸。");
  const scale = Math.min(zoom, MAX_CANVAS_EDGE / width, MAX_CANVAS_EDGE / height, Math.sqrt(MAX_CANVAS_PIXELS / width / height));
  return { width: Math.max(1, Math.floor(width * scale)), height: Math.max(1, Math.floor(height * scale)), scale };
}

/** 先解析栅格头，避免把压缩体积很小的超大图片交给原生解码器。AVIF 只接受含标准 ispe 尺寸的文件。 */
export function rasterDimensions(bytes: Uint8Array, kind: Exclude<PreviewKind, "pdf">) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const text = (offset: number, length: number) => String.fromCharCode(...bytes.subarray(offset, offset + length));
  const fits = (offset: number, length: number) => offset >= 0 && offset + length <= bytes.length;
  let width = 0, height = 0;
  if (kind === "png" && fits(0, 24) && text(0, 8) === "\x89PNG\r\n\x1a\n" && text(12, 4) === "IHDR") {
    width = view.getUint32(16); height = view.getUint32(20);
  } else if (kind === "gif" && fits(0, 10) && /^(GIF87a|GIF89a)$/.test(text(0, 6))) {
    width = view.getUint16(6, true); height = view.getUint16(8, true);
  } else if (kind === "bmp" && fits(0, 26) && text(0, 2) === "BM") {
    const header = view.getUint32(14, true);
    if (header === 12) { width = view.getUint16(18, true); height = view.getUint16(20, true); }
    else if (header >= 40) { width = view.getInt32(18, true); height = Math.abs(view.getInt32(22, true)); }
  } else if (kind === "jpeg" && fits(0, 4) && bytes[0] === 0xff && bytes[1] === 0xd8) {
    let offset = 2;
    while (fits(offset, 4)) {
      if (bytes[offset++] !== 0xff) break;
      while (bytes[offset] === 0xff) offset++;
      const marker = bytes[offset++];
      if (marker === 0xda || marker === 0xd9) break;
      if (marker === 1 || (marker >= 0xd0 && marker <= 0xd7)) continue;
      if (!fits(offset, 2)) break;
      const length = view.getUint16(offset);
      if (length < 2 || !fits(offset, length)) break;
      if ([0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(marker) && length >= 7) {
        height = view.getUint16(offset + 3); width = view.getUint16(offset + 5); break;
      }
      offset += length;
    }
  } else if (kind === "webp" && fits(0, 30) && text(0, 4) === "RIFF" && text(8, 4) === "WEBP") {
    const uint24 = (offset: number) => bytes[offset] + bytes[offset + 1] * 256 + bytes[offset + 2] * 65536;
    if (text(12, 4) === "VP8X") { width = uint24(24) + 1; height = uint24(27) + 1; }
    else if (text(12, 4) === "VP8 " && text(23, 3) === "\x9d\x01\x2a") {
      width = view.getUint16(26, true) & 0x3fff; height = view.getUint16(28, true) & 0x3fff;
    } else if (text(12, 4) === "VP8L" && bytes[20] === 0x2f) {
      const bits = view.getUint32(21, true); width = (bits & 0x3fff) + 1; height = ((bits >>> 14) & 0x3fff) + 1;
    }
  } else if (kind === "avif" && fits(0, 16) && text(4, 4) === "ftyp" && ["avif", "avis"].includes(text(8, 4))) {
    let boxes = 0;
    function scan(start: number, end: number, depth: number) {
      if (depth > 8) throw new Error("AVIF 结构过深。");
      for (let offset = start; offset + 8 <= end;) {
        if (++boxes > 10000) throw new Error("AVIF 结构过于复杂。");
        const size = view.getUint32(offset), type = text(offset + 4, 4);
        if (size < 8 || offset + size > end) throw new Error("AVIF 结构无效。");
        if (type === "ispe" && size >= 20) {
          width = Math.max(width, view.getUint32(offset + 12)); height = Math.max(height, view.getUint32(offset + 16));
        } else if (["meta", "iprp", "ipco"].includes(type)) scan(offset + (type === "meta" ? 12 : 8), offset + size, depth + 1);
        offset += size;
      }
    }
    scan(0, bytes.length, 0);
  }
  if (!width || !height || width < 0 || width > MAX_CANVAS_EDGE || height > MAX_CANVAS_EDGE || width * height > MAX_PREVIEW_PIXELS)
    throw new Error("图片格式无效或尺寸超限（最多 16 百万像素，单边 8192）。");
  return { width, height };
}
