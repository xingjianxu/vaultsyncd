import { AnnotationMode, GlobalWorkerOptions, getDocument } from "pdfjs-dist";
// Use the audited, pnpm-patched worker; the upstream minified worker is not patched.
import workerUrl from "pdfjs-dist/build/pdf.worker.mjs?url";
import { boundedCanvas, MAX_CANVAS_PIXELS, MAX_PREVIEW_PIXELS, rasterDimensions, type PreviewKind } from "./preview";

GlobalWorkerOptions.workerSrc = workerUrl;

export interface PreviewRenderer {
  pages: number;
  render: (canvas: HTMLCanvasElement, page: number, zoom: number, signal: AbortSignal) => Promise<void>;
  destroy: () => Promise<void>;
}

/** 只渲染画布；不创建 HTML、链接、表单、附件、注释层或脚本沙箱。 */
export async function createPreviewRenderer(blob: Blob, kind: PreviewKind, signal: AbortSignal): Promise<PreviewRenderer> {
  signal.throwIfAborted();
  const bytes = new Uint8Array(await blob.arrayBuffer());
  signal.throwIfAborted();
  if (kind !== "pdf") {
    rasterDimensions(bytes, kind);
    const mime = { png: "image/png", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", bmp: "image/bmp", avif: "image/avif" }[kind];
    const bitmap = await createImageBitmap(new Blob([bytes], { type: mime }));
    if (signal.aborted) { bitmap.close(); signal.throwIfAborted(); }
    // 原生解码器的最终尺寸（含方向）也必须有界。
    if (bitmap.width * bitmap.height > MAX_PREVIEW_PIXELS || Math.max(bitmap.width, bitmap.height) > 8192) {
      bitmap.close(); throw new Error("解码后的图片尺寸超限。");
    }
    return {
      pages: 1,
      async render(canvas, _page, _zoom, renderSignal) {
        renderSignal.throwIfAborted();
        const size = boundedCanvas(bitmap.width, bitmap.height);
        canvas.width = size.width; canvas.height = size.height;
        const context = canvas.getContext("2d");
        if (!context) throw new Error("浏览器无法创建预览画布。");
        context.drawImage(bitmap, 0, 0, size.width, size.height);
      },
      async destroy() { bitmap.close(); },
    };
  }
  if (!new TextDecoder("ascii").decode(bytes.subarray(0, 1024)).includes("%PDF-")) throw new Error("文件不是有效的 PDF。");
  const assets = new URL(`${import.meta.env.BASE_URL}assets/pdfjs/`, window.location.href).href;
  const options = {
    data: bytes, isEvalSupported: false, enableXfa: false, disableFontFace: true, useSystemFonts: false, useWasm: false,
    cMapUrl: `${assets}cmaps/`, cMapPacked: true, standardFontDataUrl: `${assets}standard_fonts/`,
    maxImageSize: MAX_PREVIEW_PIXELS, canvasMaxAreaInBytes: MAX_CANVAS_PIXELS * 4,
    stopAtErrors: true, isOffscreenCanvasSupported: false,
  };
  const task = getDocument(options);
  const abort = () => { void task.destroy().catch(() => undefined); };
  signal.addEventListener("abort", abort, { once: true });
  try {
    const document = await task.promise;
    signal.throwIfAborted();
    return {
      pages: document.numPages,
      async render(canvas, number, zoom, renderSignal) {
        renderSignal.throwIfAborted();
        const page = await document.getPage(number);
        try {
          renderSignal.throwIfAborted();
          const initial = page.getViewport({ scale: 1 });
          const size = boundedCanvas(initial.width, initial.height, zoom);
          const viewport = page.getViewport({ scale: size.scale });
          canvas.width = size.width; canvas.height = size.height;
          const context = canvas.getContext("2d");
          if (!context) throw new Error("浏览器无法创建预览画布。");
          const render = page.render({ canvas, canvasContext: context, viewport, annotationMode: AnnotationMode.DISABLE });
          const cancel = () => render.cancel();
          renderSignal.addEventListener("abort", cancel, { once: true });
          try { await render.promise; renderSignal.throwIfAborted(); }
          finally { renderSignal.removeEventListener("abort", cancel); }
        } finally { page.cleanup(); }
      },
      async destroy() { signal.removeEventListener("abort", abort); await task.destroy(); },
    };
  } catch (cause) {
    signal.removeEventListener("abort", abort);
    await task.destroy().catch(() => undefined);
    throw cause;
  }
}
