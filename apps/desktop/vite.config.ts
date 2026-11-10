import { defineConfig } from "vite";
import { version } from "./package.json";
export default defineConfig({ base: "./", define: { __APP_VERSION__: JSON.stringify(version) }, server: { host: "127.0.0.1", strictPort: true }, preview: { host: "127.0.0.1", strictPort: true } });
