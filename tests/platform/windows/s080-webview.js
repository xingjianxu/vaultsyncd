// Native acceptance: execute inside the real WebView, never a mock runtime.
window.addEventListener("DOMContentLoaded", async () => {
  const report = { ok: false, status: false, unknownDenied: false, shellDenied: false, pathDenied: false };
  try {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const status = await invoke("agent_get_status");
    report.status = typeof status.agent_version === "string" && Array.isArray(status.bindings);
    for (const [field, command, args] of [
      ["unknownDenied", "arbitrary_command", {}],
      ["shellDenied", "plugin:shell|execute", { program: "cmd.exe", args: ["/c", "exit"] }],
      ["pathDenied", "plugin:fs|read_text_file", { path: "C:\\Windows\\win.ini" }],
    ]) {
      try { await invoke(command, args); } catch { report[field] = true; }
    }
    report.ok = report.status && report.unknownDenied && report.shellDenied && report.pathDenied;
  } catch (error) { report.error = String(error); }
  await window.__TAURI_INTERNALS__.invoke("acceptance_report", { report });
});
