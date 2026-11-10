import { downloadVersionContent, type SnapshotEntry } from "@vaultsyncd/api-client";
import { sha256 } from "@noble/hashes/sha2.js";
import type { BrowserAuth } from "../../services/auth";

const CHUNK_BYTES = 4 * 1024 * 1024;

/** 固定版本按区间读取，单次 JS 字节缓冲有界；全部校验通过后才交给浏览器另存。 */
export async function downloadFile(client: BrowserAuth, vaultId: string, source: SnapshotEntry, signal: AbortSignal,
  progress: (bytes: number, total: number) => void): Promise<{ name: string; blob: Blob }> {
  const entry = { ...source };
  if (entry.kind !== "file" || !entry.version_id || !entry.hash || !/^[a-f0-9]{64}$/.test(entry.hash)
    || !entry.size || !/^(0|[1-9][0-9]*)$/.test(entry.size)) throw new Error("文件缺少已提交的内容版本，请刷新。");
  const total = Number(entry.size);
  if (!Number.isSafeInteger(total) || total > 64 * 1024 ** 3) throw new Error("文件大小超出服务端 64 GiB 上限。");
  const digest = sha256.create();
  const parts: Blob[] = [];
  progress(0, total);
  for (let start = 0; start < total || (total === 0 && start === 0); start += CHUNK_BYTES) {
    signal.throwIfAborted();
    const end = Math.min(start + CHUNK_BYTES, total) - 1;
    const expected = total === 0 ? 0 : end - start + 1;
    const bytes = await client.request(options => downloadVersionContent({ ...options, fetch: async (url, init) => {
      const response = await options.fetch(url, init);
      if (!response.ok) return response;
      // 既有服务对覆盖整文件的Range返回200；部分区间必须206且位置/总长一致。
      const whole = start === 0 && expected === total;
      const valid = response.status === 200 ? whole : response.status === 206 && total > 0
        && response.headers.get("content-range") === `bytes ${start}-${end}/${total}`;
      if (!valid) {
        await response.body?.cancel();
        throw new Error("下载区间或文件大小不匹配。");
      }
      if (!response.body) throw new Error("下载响应缺少内容流。");
      const reader = response.body.getReader();
      let count = 0;
      return new Response(new ReadableStream<Uint8Array>({
        async pull(controller) {
          try {
            const { done, value } = await reader.read();
            signal.throwIfAborted();
            if (done) {
              if (count !== expected) throw new Error("下载内容长度不匹配。");
              controller.close(); return;
            }
            count += value.byteLength;
            if (count > expected) throw new Error("下载内容超出声明长度。");
            controller.enqueue(value);
          } catch (cause) { await reader.cancel().catch(() => undefined); controller.error(cause); }
        },
        cancel(reason) { return reader.cancel(reason); },
      }), { status: response.status, headers: response.headers });
    } }, vaultId, entry.entry_id, entry.version_id!, { signal, ...(total ? { headers: { Range: `bytes=${start}-${end}` } } : {}) }));
    signal.throwIfAborted();
    digest.update(bytes);
    parts.push(new Blob([bytes as Uint8Array<ArrayBuffer>]));
    progress(start + bytes.length, total);
    if (total === 0) break;
  }
  const hash = Array.from(digest.digest(), byte => byte.toString(16).padStart(2, "0")).join("");
  if (hash !== entry.hash) throw new Error("下载 SHA-256 校验失败，未保存文件。");
  signal.throwIfAborted();
  return { name: entry.name, blob: new Blob(parts, { type: "application/octet-stream" }) };
}

/** 原名经 download 属性传给浏览器；让浏览器接管后延迟回收 URL。 */
export function saveDownload({ name, blob }: { name: string; blob: Blob }) {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url; link.download = name;
  document.body.append(link);
  try { link.click(); }
  finally { link.remove(); window.setTimeout(() => URL.revokeObjectURL(url), 60_000); }
}
