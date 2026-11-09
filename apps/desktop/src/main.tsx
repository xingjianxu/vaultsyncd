import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "@vaultsyncd/ui/styles.css";
import "./styles.css";

// Mantine's modal scroll lock uses the same nonce as its generated styles.
const nonce = document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content;
if (nonce) (globalThis as typeof globalThis & { __webpack_nonce__?: string }).__webpack_nonce__ = nonce;

const container = document.getElementById("root");
if (!container) throw new Error("应用挂载点缺失");
createRoot(container).render(<StrictMode><App /></StrictMode>);
