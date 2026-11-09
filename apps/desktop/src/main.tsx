import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "@vaultsyncd/ui/styles.css";
import "./styles.css";

const container = document.getElementById("root");
if (!container) throw new Error("应用挂载点缺失");
createRoot(container).render(<StrictMode><App /></StrictMode>);
