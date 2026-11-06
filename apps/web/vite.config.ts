import { defineConfig, loadEnv } from "vite";

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
    server: { host: "127.0.0.1", strictPort: true, allowedHosts: ["localhost"], cors: false,
      proxy: target ? { "/v1": { target, changeOrigin: false } } : undefined },
    preview: { host: "127.0.0.1", strictPort: true, allowedHosts: ["localhost"], cors: false },
  };
});
