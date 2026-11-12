import { defineConfig, loadEnv, type Plugin } from "vite";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";
import tailwindcss from "@tailwindcss/vite";

/** PDF.js 的可信字体/CMap 随构建同源交付；开发服务使用同一路径，不依赖 CDN。 */
function pdfResources(): Plugin {
  const root = dirname(createRequire(import.meta.url).resolve("pdfjs-dist/package.json"));
  const resources = new Map<string, Buffer>();
  for (const directory of ["cmaps", "standard_fonts"]) {
    for (const file of readdirSync(join(root, directory))) {
      resources.set(`assets/pdfjs/${directory}/${file}`, readFileSync(join(root, directory, file)));
    }
  }
  return {
    name: "pdfjs-local-resources",
    generateBundle() {
      for (const [fileName, source] of resources) this.emitFile({ type: "asset", fileName, source });
    },
    configureServer(server) {
      server.middlewares.use((request, response, next) => {
        const path = request.url?.split("?")[0].slice(1) ?? "";
        if (!path.startsWith("assets/pdfjs/")) return next();
        const bytes = resources.get(path);
        response.setHeader("Content-Type", "application/octet-stream");
        response.setHeader("X-Content-Type-Options", "nosniff");
        response.statusCode = bytes ? 200 : 404;
        response.end(bytes);
      });
    },
  };
}

export default defineConfig(({ mode }) => {
  const target = loadEnv(mode, ".", "VAULTSYNCD_").VAULTSYNCD_DEV_API;
  if (target) {
    const url = new URL(target);
    if (!["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) || !["http:", "https:"].includes(url.protocol)
      || url.username || url.password || url.pathname !== "/" || url.search || url.hash) {
      throw new Error("VAULTSYNCD_DEV_API 只接受无凭据/路径的本机测试服务地址");
    }
  }
  return {
    base: "./",
    plugins: [tailwindcss(), pdfResources()],
    server: { host: "127.0.0.1", strictPort: true, allowedHosts: ["localhost"], cors: false,
      proxy: target ? { "/v1": { target, changeOrigin: false } } : undefined },
    preview: { host: "127.0.0.1", strictPort: true, allowedHosts: ["localhost"], cors: false },
  };
});
