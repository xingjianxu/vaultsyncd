import { useEffect, useRef, useState } from "react";
import type { ComponentType } from "react";
import { Alert, Button, Group, Modal, Progress, Stack, Text } from "@mantine/core";
import type { SnapshotEntry } from "@vaultsyncd/api-client";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { decodePreviewText, downloadPreview, previewKind } from "./preview";
import { saveDownload } from "./download";
import { downloadOfficePreview } from "./office-preview";
import type { PreviewRenderer } from "./preview-renderer";
import "./FilePreview.css";

export function FilePreview({ entry, vaultId, onClose }: { entry: SnapshotEntry | null; vaultId: string; onClose: () => void }) {
  const auth = useAuth();
  const [attempt, setAttempt] = useState(0);
  const [status, setStatus] = useState("");
  const [error, setError] = useState("");
  const [progress, setProgress] = useState(0);
  const [renderer, setRenderer] = useState<PreviewRenderer | null>(null);
  const [download, setDownload] = useState<{ name: string; blob: Blob } | null>(null);
  const [pdfUrl, setPdfUrl] = useState<string | null>(null);
  const [text, setText] = useState<string | null>(null);
  const [TextViewer, setTextViewer] = useState<ComponentType<{ text: string; name: string; markdown: boolean }> | null>(null);
  const [rendering, setRendering] = useState(false);
  const canvas = useRef<HTMLCanvasElement>(null);
  const request = useRef<AbortController | null>(null);
  const kind = entry && previewKind(entry);
  const client = auth.client;
  const generation = client?.sessionGeneration;
  // 同一 Modal 保持挂载以恢复焦点；内容和资源在关闭时立即释放，不等退场动画。
  useEffect(() => {
    setRenderer(null); setDownload(null); setError(""); setPdfUrl(null); setText(null); setTextViewer(null); setProgress(0); setRendering(false);
    if (!entry || !client || !kind || auth.busy) { setStatus(""); return; }
    const controller = new AbortController();
    request.current = controller;
    let resource: PreviewRenderer | null = null;
    let objectUrl: string | null = null;
    controller.signal.addEventListener("abort", () => {
      if (objectUrl) { URL.revokeObjectURL(objectUrl); objectUrl = null; }
      const previous = resource; resource = null;
      void previous?.destroy().catch(() => undefined);
    }, { once: true });
    const current = () => !controller.signal.aborted && client.sessionGeneration === generation;
    setStatus("正在读取并校验文件…");
    void (async () => {
      try {
        const result = await downloadPreview(client, vaultId, entry, controller.signal, (bytes, total) => {
          if (current()) setProgress(total ? Math.round(bytes / total * 100) : 100);
        });
        if (!current()) return;
        setDownload(result); setStatus("正在解析文件…");
        if (kind === "text" || kind === "markdown") {
          const decoded = await decodePreviewText(result.blob);
          if (!current()) return;
          const { TextPreview } = await import("./TextPreview");
          if (!current()) return;
          setText(decoded); setTextViewer(() => TextPreview); setStatus("");
          return;
        }
        let previewBlob = result.blob;
        if (kind === "office") {
          if (navigator.pdfViewerEnabled === false) throw new Error("浏览器未启用 PDF 预览，请下载后打开。");
          setStatus("正在转换 Office 文件…"); setProgress(0);
          previewBlob = await downloadOfficePreview(client, vaultId, entry, controller.signal, (bytes, total) => {
            if (current()) setProgress(Math.round(bytes / total * 100));
          });
          if (!current()) return;
        }
        if (kind === "pdf" || kind === "office") {
          const header = await previewBlob.slice(0, 1024).text();
          if (!current()) return;
          if (!header.includes("%PDF-")) throw new Error("文件不是有效的 PDF。");
          if (navigator.pdfViewerEnabled === false) throw new Error("浏览器未启用 PDF 预览，请下载后打开。");
          // 固定 MIME，避免文件内容被当作同源 HTML 执行；只嵌入已校验的本地 Blob。
          objectUrl = URL.createObjectURL(previewBlob.slice(0, previewBlob.size, "application/pdf"));
          setPdfUrl(objectUrl); setStatus("");
          return;
        }
        const { createPreviewRenderer } = await import("./preview-renderer");
        if (!current()) return;
        const next = await createPreviewRenderer(result.blob, kind, controller.signal);
        if (!current()) { await next.destroy(); return; }
        resource = next; setRenderer(next); setStatus("");
      } catch (cause) {
        if (!current()) return;
        if (cause instanceof SignedOut) auth.reportSignedOut();
        setStatus("");
        setError(cause instanceof Error ? `预览失败：${cause.message}` : "文件无法解析，请下载后查看。");
      }
    })();
    return () => {
      controller.abort();
      if (request.current === controller) request.current = null;
    };
  }, [entry, vaultId, client, generation, auth.busy, attempt]);

  useEffect(() => {
    if (!renderer || !canvas.current) return;
    const target = canvas.current;
    const controller = new AbortController();
    setRendering(true); setError("");
    void renderer.render(target, controller.signal).catch(cause => {
      if (!controller.signal.aborted) setError(cause instanceof Error ? `预览失败：${cause.message}` : "页面渲染失败。");
    }).finally(() => { if (!controller.signal.aborted) setRendering(false); });
    return () => { controller.abort(); target.width = 0; target.height = 0; };
  }, [renderer]);

  // hash 路由离开、切换目录/Vault 时，事件发生即停止网络与解析（包括未完成的动态模块加载）。
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  useEffect(() => {
    const leave = () => { request.current?.abort(); closeRef.current(); };
    window.addEventListener("hashchange", leave);
    window.addEventListener("popstate", leave);
    return () => { window.removeEventListener("hashchange", leave); window.removeEventListener("popstate", leave); };
  }, []);
  function close() { request.current?.abort(); onClose(); }
  const busy = !!status || rendering;
  return <Modal opened={!!entry} onClose={close} title={`预览 · ${entry?.name ?? ""}`} size="min(1100px, 95vw)" centered
    closeButtonProps={{ "aria-label": "关闭预览" }} closeOnEscape returnFocus>
    <Stack gap="sm">
      <Group justify="flex-end">
        <Group gap="xs">
          <Button variant="default" disabled={!download || auth.busy} onClick={() => { if (download) saveDownload(download); }}>下载原文件</Button>
          <Button data-autofocus variant="default" onClick={close}>关闭</Button>
        </Group>
      </Group>
      {status && <><Text role="status">{status}</Text><Progress value={progress} aria-label="文件读取进度" /></>}
      {rendering && <Text role="status">正在渲染页面…</Text>}
      {error && <Alert color="red" role="alert">{error}</Alert>}
      <Group>
        {busy && <Button variant="default" onClick={() => {
          request.current?.abort(); setRenderer(null); setPdfUrl(null); setText(null); setTextViewer(null); setStatus(""); setRendering(false); setError("已取消预览，可重试。");
        }}>取消预览</Button>}
        {error && <Button variant="default" disabled={auth.busy} onClick={() => setAttempt(value => value + 1)}>重试预览</Button>}
      </Group>
      {pdfUrl && <iframe className="file-preview-pdf" src={pdfUrl} title={`${kind === "office" ? "Office" : "PDF"} 预览 · ${entry?.name ?? ""}`} />}
      {TextViewer && text !== null && entry && <TextViewer key={`${entry.entry_id}:${entry.version_id}:${attempt}`} text={text} name={entry.name} markdown={kind === "markdown"} />}
      {renderer && <div className="file-preview-viewport" aria-busy={rendering}>
        <canvas ref={canvas} role="img" aria-label={entry?.name} hidden={!!error} />
      </div>}
    </Stack>
  </Modal>;
}
