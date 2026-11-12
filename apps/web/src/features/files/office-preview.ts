import { previewOfficeVersion, type SnapshotEntry } from "@vaultsyncd/api-client";
import { sha256 } from "@noble/hashes/sha2.js";
import type { BrowserAuth } from "../../services/auth";
import { MAX_PREVIEW_BYTES, officeFileType } from "./preview";

/** 转换只使用点击时的固定版本；原始 Office 字节仍由现有下载入口提供。 */
export async function downloadOfficePreview(client: BrowserAuth, vaultId: string, source: SnapshotEntry, signal: AbortSignal,
  progress: (bytes: number, total: number) => void = () => {}): Promise<Blob> {
  const entry = { ...source };
  const fileType = officeFileType(entry.name);
  if (entry.kind !== "file" || !fileType || !entry.version_id || !entry.hash || !/^[a-f0-9]{64}$/.test(entry.hash)
    || !entry.size || !/^(0|[1-9][0-9]*)$/.test(entry.size)) throw new Error("文件缺少有效的 Office 内容版本，请刷新。");
  if (BigInt(entry.size) > BigInt(MAX_PREVIEW_BYTES)) throw new Error("在线预览仅支持不超过 50 MiB 的文件，请下载后查看。");
  signal.throwIfAborted();
  let expectedDigest = "";
  const bytes = await client.request(options => previewOfficeVersion({ ...options, fetch: async (url, init) => {
    const response = await options.fetch(url, init);
    if (signal.aborted) { await response.body?.cancel(); signal.throwIfAborted(); }
    if (!response.ok) return response;
    const reject = async (message: string): Promise<never> => { await response.body?.cancel(); throw new Error(message); };
    if (response.status !== 200 || response.headers.get("content-type")?.split(";")[0].trim().toLowerCase() !== "application/pdf")
      return reject("Office 预览响应不是 PDF。");
    if (response.headers.get("x-preview-source-sha256") !== entry.hash || response.headers.get("x-preview-source-version") !== entry.version_id)
      return reject("Office 预览的源版本或摘要不匹配。");
    const length = response.headers.get("content-length");
    const digest = response.headers.get("x-content-sha256");
    if (!length || !/^[1-9][0-9]*$/.test(length) || BigInt(length) > BigInt(MAX_PREVIEW_BYTES))
      return reject("Office 预览响应大小无效或超过 50 MiB，请下载原文件后查看。");
    if (!digest || !/^[a-f0-9]{64}$/.test(digest)) return reject("Office 预览缺少有效的 PDF 摘要。");
    if (!response.body) return reject("Office 预览响应缺少内容流。");
    expectedDigest = digest;
    const total = Number(length);
    const reader = response.body.getReader();
    let count = 0;
    const abort = () => { void reader.cancel().catch(() => undefined); };
    signal.addEventListener("abort", abort, { once: true });
    progress(0, total);
    return new Response(new ReadableStream<Uint8Array>({
      async pull(controller) {
        try {
          signal.throwIfAborted();
          const { done, value } = await reader.read();
          signal.throwIfAborted();
          if (done) {
            if (count !== total) throw new Error("Office 预览内容长度不匹配。");
            signal.removeEventListener("abort", abort); controller.close(); return;
          }
          count += value.byteLength;
          if (count > total || count > MAX_PREVIEW_BYTES) throw new Error("Office 预览内容超出声明长度。");
          progress(count, total);
          controller.enqueue(value);
        } catch (cause) {
          signal.removeEventListener("abort", abort);
          await reader.cancel().catch(() => undefined); controller.error(cause);
        }
      },
      cancel(reason) { signal.removeEventListener("abort", abort); return reader.cancel(reason); },
    }), { status: response.status, headers: response.headers });
  } }, vaultId, entry.entry_id, entry.version_id!, { file_type: fileType, hash: entry.hash!, size: entry.size! }, { signal }));
  signal.throwIfAborted();
  const actualDigest = Array.from(sha256(bytes), byte => byte.toString(16).padStart(2, "0")).join("");
  if (actualDigest !== expectedDigest) throw new Error("Office 预览 PDF 的 SHA-256 校验失败。");
  const header = new TextDecoder().decode(bytes.subarray(0, 1024));
  if (!header.startsWith("%PDF-")) throw new Error("Office 预览结果不是有效的 PDF。");
  signal.throwIfAborted();
  return new Blob([bytes as Uint8Array<ArrayBuffer>], { type: "application/pdf" });
}
