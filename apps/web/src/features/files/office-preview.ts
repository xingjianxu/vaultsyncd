import { createOfficeViewer, closeOfficeViewer, type SnapshotEntry } from "@vaultsyncd/api-client";
import type { BrowserAuth } from "../../services/auth";
import { MAX_PREVIEW_BYTES, officeFileType } from "./preview";

export interface OfficeViewer {
  documentServer: string;
  config: Record<string, unknown>;
  destroy(): Promise<void>;
}

/** Only a configured HTTPS origin (or literal loopback for local development) may supply API.js. */
export function officeDocumentServer(value: string): string {
  let url: URL;
  try { url = new URL(value); } catch { throw new Error("Office 查看服务地址无效。"); }
  const loopback = url.hostname === "[::1]" || /^127\.(?:\d{1,3}\.){2}\d{1,3}$/.test(url.hostname);
  if ((url.protocol !== "https:" && !(url.protocol === "http:" && loopback)) || url.username || url.password
    || url.pathname !== "/" || url.search || url.hash) throw new Error("Office 查看服务地址无效。");
  return url.origin;
}

const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value);

/** This describes a single immutable version; no application save callback is supplied. */
export async function openOfficeViewer(client: BrowserAuth, vaultId: string, source: SnapshotEntry, signal: AbortSignal): Promise<OfficeViewer> {
  const entry = { ...source };
  const fileType = officeFileType(entry.name);
  if (entry.kind !== "file" || !fileType || !entry.version_id || !entry.hash || !/^[a-f0-9]{64}$/.test(entry.hash)
    || !entry.size || !/^(0|[1-9][0-9]*)$/.test(entry.size)) throw new Error("文件缺少有效的 Office 内容版本，请刷新。");
  if (BigInt(entry.size) > BigInt(MAX_PREVIEW_BYTES)) throw new Error("在线预览仅支持不超过 50 MiB 的文件，请下载后查看。");
  signal.throwIfAborted();
  let sessionId: string | null = null;
  let destroyed = false;
  const destroy = async () => {
    if (destroyed || !sessionId) return;
    destroyed = true;
    // The source also expires server-side after 120 seconds, including lost responses and logout.
    await client.request(options => closeOfficeViewer(options, vaultId, entry.entry_id, entry.version_id!, sessionId!, { keepalive: true }));
  };
  try {
    const session = await client.request(async options => {
      const result = await createOfficeViewer(options, vaultId, entry.entry_id, entry.version_id!,
        { file_type: fileType, hash: entry.hash!, size: entry.size! }, { signal });
      if (/^[a-f0-9]{64}$/.test(result.session_id)) sessionId = result.session_id;
      return result;
    });
    signal.throwIfAborted();
    if (!sessionId || session.source_hash !== entry.hash || session.source_version !== entry.version_id)
      throw new Error("Office 预览的源版本或摘要不匹配。");
    const documentServer = officeDocumentServer(session.document_server_url);
    if (session.config_json.length > 64 * 1024) throw new Error("Office 查看配置超过大小限制。");
    let config: unknown;
    try { config = JSON.parse(session.config_json); } catch { throw new Error("Office 查看配置无效。"); }
    if (!object(config) || !object(config.document) || !object(config.document.permissions) || !object(config.editorConfig)
      || config.editorConfig.mode !== "view" || config.document.fileType !== fileType || typeof config.token !== "string" || !config.token
      || ["edit", "comment", "review", "fillForms"].some(key => config.document && object(config.document)
        && object(config.document.permissions) && config.document.permissions[key] !== false))
      throw new Error("Office 查看配置未限制为只读。");
    signal.throwIfAborted();
    return { documentServer, config, destroy };
  } catch (cause) {
    void destroy().catch(() => undefined);
    throw cause;
  }
}

export interface OfficeEditor { destroyEditor(): void; }
export interface OfficeApi { DocEditor: new (id: string, config: Record<string, unknown>) => OfficeEditor; }
declare global { interface Window { DocsAPI?: OfficeApi; } }
let script: { origin: string; promise: Promise<OfficeApi> } | null = null;

/** API.js runs as trusted deployment code. Only one configured origin may run in a page. */
export async function loadOfficeApi(documentServer: string, signal: AbortSignal): Promise<OfficeApi> {
  const origin = officeDocumentServer(documentServer);
  signal.throwIfAborted();
  if (script && script.origin !== origin) throw new Error("Office 查看服务地址已变更，请刷新页面。");
  if (!script) {
    const element = document.createElement("script");
    element.src = `${origin}/web-apps/apps/api/documents/api.js`;
    element.async = true;
    element.referrerPolicy = "no-referrer";
    const promise = new Promise<OfficeApi>((resolve, reject) => {
      const finish = (error?: Error) => {
        clearTimeout(timer); element.onload = null; element.onerror = null;
        if (!error && window.DocsAPI?.DocEditor) resolve(window.DocsAPI);
        else { element.remove(); script = null; reject(error ?? new Error("Office 查看器加载失败。")); }
      };
      const timer = setTimeout(() => finish(new Error("Office 查看器加载超时，请重试。")), 30000);
      element.onload = () => finish();
      element.onerror = () => finish(new Error("Office 查看器加载失败，请重试。"));
      document.head.append(element);
    });
    script = { origin, promise };
  }
  const pending = script.promise;
  return new Promise<OfficeApi>((resolve, reject) => {
    const abort = () => reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    signal.addEventListener("abort", abort, { once: true });
    pending.then(api => { signal.removeEventListener("abort", abort); if (!signal.aborted) resolve(api); }, cause => {
      signal.removeEventListener("abort", abort); if (!signal.aborted) reject(cause);
    });
  });
}
