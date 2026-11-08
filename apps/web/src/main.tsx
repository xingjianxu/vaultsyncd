import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles.css";

// Modal scroll locking reads the standard nonce global through get-nonce.
const nonce = document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content;
if (nonce) (globalThis as typeof globalThis & { __webpack_nonce__?: string }).__webpack_nonce__ = nonce;

const root = document.getElementById("root");
if (!root) throw new Error("应用挂载点缺失");
createRoot(root).render(<StrictMode><App /></StrictMode>);
