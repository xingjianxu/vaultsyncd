// Native acceptance: execute inside the real WebView, never a mock runtime.
window.addEventListener("DOMContentLoaded", async () => {
  const report = { ok: false, status: false, unknownDenied: false, shellDenied: false, pathDenied: false, windowDenied: false };
  try {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const status = await invoke("agent_get_status");
    report.status = typeof status.agent_version === "string" && Array.isArray(status.bindings);
    report.agentVersion = status.agent_version;
    report.snapshotVersion = status.snapshot_version;
    for (const [field, command, args] of [
      ["unknownDenied", "arbitrary_command", {}],
      ["shellDenied", "plugin:shell|execute", { program: "cmd.exe", args: ["/c", "exit"] }],
      ["pathDenied", "plugin:fs|read_text_file", { path: "C:\\Windows\\win.ini" }],
      ["windowDenied", "plugin:window|set_title", { label: "main", title: "unauthorized" }],
    ]) {
      try { await invoke(command, args); } catch { report[field] = true; }
    }
    report.ok = report.status && report.unknownDenied && report.shellDenied && report.pathDenied && report.windowDenied;
  } catch (error) { report.error = String(error); }
  await window.__TAURI_INTERNALS__.invoke("acceptance_report", { report });
});
