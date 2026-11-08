import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles.css";

const root = document.getElementById("root");
if (!root) throw new Error("应用挂载点缺失");
createRoot(root).render(<StrictMode><App /></StrictMode>);
