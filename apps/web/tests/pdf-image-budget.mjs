import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { deflateSync } from "node:zlib";
import { getDocument, GlobalWorkerOptions, OPS } from "pdfjs-dist/legacy/build/pdf.mjs";

// Exercise the exact patched worker shipped to browsers, not a fake PDF decoder.
const worker = import.meta.resolve("pdfjs-dist/build/pdf.worker.mjs");
GlobalWorkerOptions.workerSrc = fileURLToPath(worker);
const source = readFileSync(new URL(worker), "utf8");
assert.match(source, /VAULTSYNCD_MAX_IMAGE_PIXELS = 16 \* 1024 \* 1024/);
assert.match(source, /VAULTSYNCD_MAX_IMAGE_EDGE = 8192/);
assert.match(source, /assertVaultsyncdImageDimensions\(drawWidth, drawHeight\);/);
assert.match(source, /assertVaultsyncdImageDimensions\(frame.samplesPerLine, frame.scanLines\);/);
assert.match(source, /if \(!ignoreErrors\) \{\s+await imagePromise;/);
const renderer = readFileSync(new URL("../src/features/files/preview-renderer.ts", import.meta.url), "utf8");
assert.ok(renderer.includes('"pdfjs-dist/build/pdf.worker.mjs?url"'), "must ship the patched worker, not the upstream minified worker");
const patch = readFileSync(new URL("../../../patches/pdfjs-dist@6.5.438.patch", import.meta.url), "utf8");
assert.ok(patch.includes("assertVaultsyncdImageDimensions"), "the policy must survive a clean dependency install");

function stream(dictionary, data) {
  return Buffer.concat([Buffer.from(`<< ${dictionary} /Length ${data.length} >>\nstream\n`), data, Buffer.from("\nendstream")]);
}
function pdf({ width = 1, height = 1, maskWidth, maskHeight, maskType = "SMask", jpeg = false } = {}) {
  const imageData = jpeg
    ? Buffer.from("ffd8ffc00011081001100003011100021100031100ffd9", "hex") // SOF: 4096x4097, despite dictionary 1x1.
    : deflateSync(Buffer.alloc(width * height * 3, 255));
  const objects = [
    Buffer.from("<< /Type /Catalog /Pages 2 0 R >>"),
    Buffer.from("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
    Buffer.from("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"),
    stream(`/Type /XObject /Subtype /Image /Width ${width} /Height ${height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /${jpeg ? "DCTDecode" : "FlateDecode"} ${maskWidth ? `/${maskType} 6 0 R` : ""}`, imageData),
    stream("", Buffer.from("q 100 0 0 100 0 0 cm /Im0 Do Q")),
  ];
  if (maskWidth) objects.push(stream(`/Type /XObject /Subtype /Image /Width ${maskWidth} /Height ${maskHeight} /ColorSpace /DeviceGray /BitsPerComponent ${maskType === "Mask" ? 1 : 8} ${maskType === "Mask" ? "/ImageMask true" : ""} /Filter /FlateDecode`,
    deflateSync(Buffer.alloc(maskType === "Mask" ? Math.ceil(maskWidth / 8) * maskHeight : maskWidth * maskHeight, 255))));
  const parts = [Buffer.from("%PDF-1.4\n")], offsets = [0];
  let length = parts[0].length;
  for (let index = 0; index < objects.length; index++) {
    const part = Buffer.concat([Buffer.from(`${index + 1} 0 obj\n`), objects[index], Buffer.from("\nendobj\n")]);
    offsets.push(length); length += part.length; parts.push(part);
  }
  parts.push(Buffer.from(`xref\n0 ${offsets.length}\n0000000000 65535 f \n${offsets.slice(1).map(offset => `${String(offset).padStart(10, "0")} 00000 n \n`).join("")}trailer\n<< /Size ${offsets.length} /Root 1 0 R >>\nstartxref\n${length}\n%%EOF\n`));
  return new Uint8Array(Buffer.concat(parts));
}
async function operators(options) {
  const task = getDocument({ data: pdf(options), stopAtErrors: true, maxImageSize: 16 * 1024 * 1024,
    isOffscreenCanvasSupported: false, isImageDecoderSupported: false, useWasm: false, disableFontFace: true });
  try {
    const document = await task.promise;
    const page = await document.getPage(1);
    try {
      const result = await page.getOperatorList();
      assert.ok(result.fnArray.includes(OPS.paintImageXObject), "normal PDF images must remain enabled");
      return result;
    } finally { page.cleanup(); }
  } finally { await task.destroy(); }
}
// Keep no-mask images and small masks working. Include a narrow edge exactly at the budget.
await operators();
await operators({ width: 8192, height: 1 });
await operators({ maskWidth: 2, maskHeight: 2 });
await operators({ maskWidth: 2, maskHeight: 2, maskType: "Mask" });
for (const options of [
  { maskWidth: 4097, maskHeight: 4096 }, // Original 1x1-parent SMask bypass (16,781,312 pixels).
  { maskWidth: 4097, maskHeight: 4096, maskType: "Mask" },
  { maskWidth: 8193, maskHeight: 1 },
  { maskWidth: 8193, maskHeight: 1, maskType: "Mask" },
  { width: 8192, height: 1, maskWidth: 1, maskHeight: 8192 }, // Valid inputs, oversized effective rectangle.
  { width: 8192, height: 1, maskWidth: 1, maskHeight: 8192, maskType: "Mask" },
  { jpeg: true }, // The codec's real dimensions must be checked before JPEG component allocation.
]) await assert.rejects(operators(options), /PDF image resource limit exceeded/, JSON.stringify(options));
console.log("PDF worker 补丁：正常/无掩码图像保留；SMask、Mask、有效合成尺寸、单边及真实 JPEG 尺寸在分配前拒绝，错误真实传播");
