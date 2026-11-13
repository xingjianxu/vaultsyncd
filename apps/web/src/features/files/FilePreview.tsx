import { useEffect, useRef, useState } from "react";
import type { ComponentType } from "react";
import { ActionIcon, Alert, Button, Group, Modal, Progress, Stack, Text } from "@mantine/core";
import type { SnapshotEntry } from "@vaultsyncd/api-client";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";
import { decodePreviewText, downloadPreview, previewKind } from "./preview";
import { saveDownload } from "./download";
import { FileIcon } from "./FileIcon";
import { openOfficeViewer, type OfficeViewer } from "./office-preview";
import { OfficePreview } from "./OfficePreview";
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
  const [office, setOffice] = useState<OfficeViewer | null>(null);
  const [text, setText] = useState<string | null>(null);
  const [TextViewer, setTextViewer] = useState<ComponentType<{ text: string; name: string; markdown: boolean; signal: AbortSignal }> | null>(null);
  const [rendering, setRendering] = useState(false);
  const [displayedEntry, setDisplayedEntry] = useState(entry);
  const canvas = useRef<HTMLCanvasElement>(null);
  const request = useRef<AbortController | null>(null);
  const renderRequest = useRef<AbortController | null>(null);
  const kind = entry && previewKind(entry);
  const client = auth.client;
  const generation = client?.sessionGeneration;
  const resources = useRef<{ dispose: () => void } | null>(null);
  const previewId = useRef(0);
  const context = useRef({ vaultId, client, generation, busy: auth.busy });
  const latestEntry = useRef(entry);
  latestEntry.current = entry;
  function clearContent() {
    setDisplayedEntry(null); setRenderer(null); setDownload(null); setError(""); setPdfUrl(null); setOffice(null); setText(null); setTextViewer(null); setProgress(0); setRendering(false); setStatus("");
  }
  function release() {
    resources.current?.dispose(); resources.current = null;
  }
  // 关闭只停止工作，保留退场画面；新预览或认证/Vault 变化则立即释放旧资源。
  useEffect(() => {
    const previous = context.current;
    context.current = { vaultId, client, generation, busy: auth.busy };
    if (!entry && previous.vaultId === vaultId && previous.client === client
      && previous.generation === generation && previous.busy === auth.busy) return;
    previewId.current++;
    release(); clearContent();
    if (!entry || !client || !kind || auth.busy) return;
    setDisplayedEntry(entry);
    const controller = new AbortController();
    request.current = controller;
    let resource: PreviewRenderer | null = null;
    let objectUrl: string | null = null;
    let officeViewer: OfficeViewer | null = null;
    resources.current = { dispose: () => {
      controller.abort();
      if (request.current === controller) request.current = null;
      if (objectUrl) { URL.revokeObjectURL(objectUrl); objectUrl = null; }
      const previousOffice = officeViewer; officeViewer = null;
      void previousOffice?.destroy().catch(() => undefined);
      const previous = resource; resource = null;
      void previous?.destroy().catch(() => undefined);
    } };
    controller.signal.addEventListener("abort", () => renderRequest.current?.abort(), { once: true });
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
        if (kind === "office") {
          setStatus("正在打开 Office 文档…"); setProgress(0);
          const viewer = await openOfficeViewer(client, vaultId, entry, controller.signal);
          if (!current()) { void viewer.destroy().catch(() => undefined); return; }
          officeViewer = viewer; setOffice(viewer);
          return;
        }
        const previewBlob = result.blob;
        if (kind === "pdf") {
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
    return () => controller.abort();
  }, [entry, vaultId, client, generation, auth.busy, attempt]);

  useEffect(() => () => release(), []);

  useEffect(() => {
    if (!renderer || !canvas.current || request.current?.signal.aborted) return;
    const target = canvas.current;
    const controller = new AbortController();
    renderRequest.current = controller;
    setRendering(true); setError("");
    void renderer.render(target, controller.signal).catch(cause => {
      if (!controller.signal.aborted) setError(cause instanceof Error ? `预览失败：${cause.message}` : "页面渲染失败。");
    }).finally(() => { if (!controller.signal.aborted) setRendering(false); });
    return () => {
      controller.abort(); target.width = 0; target.height = 0;
      if (renderRequest.current === controller) renderRequest.current = null;
    };
  }, [renderer]);

  // hash 路由离开、切换目录/Vault 时，事件发生即停止网络与解析（包括未完成的动态模块加载）。
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  useEffect(() => {
    const leave = () => { request.current?.abort(); release(); clearContent(); closeRef.current(); };
    window.addEventListener("hashchange", leave);
    window.addEventListener("popstate", leave);
    return () => { window.removeEventListener("hashchange", leave); window.removeEventListener("popstate", leave); };
  }, []);
  function close() { request.current?.abort(); onClose(); }
  const busy = !!status || rendering;
  const exiting = previewId.current;
  const shownEntry = entry ?? displayedEntry;
  return <Modal.Root opened={!!entry} onClose={close} size="min(1100px, 95vw)" centered closeOnEscape returnFocus
    onExitTransitionEnd={() => {
      // 被快速重开替换的退出回调不得清理新一轮预览。
      if (latestEntry.current || previewId.current !== exiting) return;
      release(); clearContent();
    }}>
    <Modal.Overlay />
    <Modal.Content>
      <Modal.Header pb={4} mih={0}>
        <Modal.Title style={{ flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
          title={`预览 · ${shownEntry?.name ?? ""}`}>预览 · {shownEntry?.name ?? ""}</Modal.Title>
        <ActionIcon variant="subtle" color="gray" aria-label="下载原文件" title="下载原文件" mr="xs"
          disabled={!download || auth.busy} onClick={() => { if (download) saveDownload(download); }}>
          <FileIcon name="download" />
        </ActionIcon>
        <Modal.CloseButton data-autofocus aria-label="关闭预览" />
      </Modal.Header>
      <Modal.Body>
    <Stack gap="sm">
      {status && <><Text role="status">{status}</Text><Progress value={progress} aria-label="文件读取进度" /></>}
      {rendering && <Text role="status">正在渲染页面…</Text>}
      {error && <Alert color="red" role="alert">{error}</Alert>}
      {(busy || error) && <Group>
        {busy && <Button variant="default" onClick={() => {
          request.current?.abort(); release(); setRenderer(null); setPdfUrl(null); setOffice(null); setText(null); setTextViewer(null); setStatus(""); setRendering(false); setError("已取消预览，可重试。");
        }}>取消预览</Button>}
        {error && <Button variant="default" disabled={auth.busy} onClick={() => setAttempt(value => value + 1)}>重试预览</Button>}
      </Group>}
      {pdfUrl && <iframe className="file-preview-pdf" src={pdfUrl} title={`PDF 预览 · ${shownEntry?.name ?? ""}`} />}
      {office && request.current && <OfficePreview viewer={office} signal={request.current.signal}
        onReady={() => { if (!request.current?.signal.aborted) setStatus(""); }}
        onError={message => {
          if (request.current?.signal.aborted) return;
          request.current?.abort(); release(); setOffice(null); setStatus(""); setError(`预览失败：${message}`);
        }} />}
      {TextViewer && text !== null && shownEntry && request.current && <TextViewer key={`${shownEntry.entry_id}:${shownEntry.version_id}:${attempt}`} text={text} name={shownEntry.name} markdown={previewKind(shownEntry) === "markdown"} signal={request.current.signal} />}
      {renderer && <div className="file-preview-viewport" aria-busy={rendering}>
        <canvas ref={canvas} role="img" aria-label={shownEntry?.name} hidden={!!error} />
      </div>}
    </Stack>
      </Modal.Body>
    </Modal.Content>
  </Modal.Root>;
}
