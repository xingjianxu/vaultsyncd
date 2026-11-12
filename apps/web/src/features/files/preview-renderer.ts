import { boundedCanvas, MAX_PREVIEW_PIXELS, rasterDimensions, type RasterKind } from "./preview";

export interface PreviewRenderer {
  render: (canvas: HTMLCanvasElement, signal: AbortSignal) => Promise<void>;
  destroy: () => Promise<void>;
}

/** 图片维持有界的静态画布预览。PDF 由浏览器内置查看器处理。 */
export async function createPreviewRenderer(blob: Blob, kind: RasterKind, signal: AbortSignal): Promise<PreviewRenderer> {
  signal.throwIfAborted();
  const bytes = new Uint8Array(await blob.arrayBuffer());
  signal.throwIfAborted();
  rasterDimensions(bytes, kind);
  const mime = { png: "image/png", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", bmp: "image/bmp", avif: "image/avif" }[kind];
  const bitmap = await createImageBitmap(new Blob([bytes], { type: mime }));
  if (signal.aborted) { bitmap.close(); signal.throwIfAborted(); }
  // 原生解码器的最终尺寸（含方向）也必须有界。
  if (bitmap.width * bitmap.height > MAX_PREVIEW_PIXELS || Math.max(bitmap.width, bitmap.height) > 8192) {
    bitmap.close(); throw new Error("解码后的图片尺寸超限。");
  }
  return {
    async render(canvas, renderSignal) {
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
