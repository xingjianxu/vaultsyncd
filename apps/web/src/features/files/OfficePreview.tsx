import { useEffect, useRef, useState } from "react";
import { loadOfficeApi, type OfficeEditor, type OfficeViewer } from "./office-preview";

/** Abort stops initialization/callbacks; unmount releases the frame after the modal exits. */
export function OfficePreview({ viewer, signal, onReady, onError }: { viewer: OfficeViewer; signal: AbortSignal; onReady: () => void; onError: (message: string) => void }) {
  const [loading, setLoading] = useState(true);
  const container = useRef<HTMLDivElement>(null);
  const callbacks = useRef({ viewer, onReady, onError });
  callbacks.current = { viewer, onReady, onError };
  useEffect(() => {
    const target = container.current;
    if (!target || signal.aborted) return;
    const controller = new AbortController();
    setLoading(true);
    let editor: OfficeEditor | null = null;
    let failed = false;
    let ready = false;
    const fail = (message: string) => {
      if (controller.signal.aborted || callbacks.current.viewer !== viewer || failed) return;
      failed = true; clearTimeout(timer);
      try { editor?.destroyEditor(); } catch { /* The iframe must still be removed if the upstream cleanup fails. */ }
      finally { editor = null; target.replaceChildren(); }
      callbacks.current.onError(message);
    };
    const timer = setTimeout(() => fail("Office 文档打开超时，请重试或下载原文件。"), 120000);
    const stop = () => { controller.abort(); clearTimeout(timer); };
    signal.addEventListener("abort", stop, { once: true });
    void loadOfficeApi(viewer.documentServer, controller.signal).then(api => {
      if (controller.signal.aborted) return;
      const placeholder = document.createElement("div");
      placeholder.id = `office-preview-${crypto.randomUUID()}`;
      target.replaceChildren(placeholder);
      const created = new api.DocEditor(placeholder.id, {
        ...viewer.config,
        width: "100%", height: "100%",
        events: {
          onDocumentReady: () => {
            if (controller.signal.aborted || callbacks.current.viewer !== viewer || failed || ready) return;
            ready = true; clearTimeout(timer); setLoading(false); callbacks.current.onReady();
          },
          onError: () => {
            // Never display an upstream URL, token or raw diagnostic in the application UI.
            fail("Office 文件无法打开，文件可能损坏、受密码保护或格式不受支持。");
          },
        },
      });
      if (failed || controller.signal.aborted) {
        try { created.destroyEditor(); } catch { /* A failed constructor callback still requires frame removal. */ }
        finally { target.replaceChildren(); }
      } else editor = created;
    }).catch(() => {
      if (!controller.signal.aborted) fail("Office 查看器加载失败，请重试或下载原文件。");
    });
    return () => {
      stop(); signal.removeEventListener("abort", stop);
      try { editor?.destroyEditor(); } catch { /* The iframe must still be removed if the upstream cleanup fails. */ }
      finally { editor = null; target.replaceChildren(); }
    };
  }, [viewer, signal]);
  return <div ref={container} className="file-preview-office" aria-busy={loading} aria-label="Office 只读预览" />;
}
