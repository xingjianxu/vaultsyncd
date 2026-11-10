// Native acceptance: execute inside the real WebView, never a mock runtime.
const cspViolations = [];
document.addEventListener("securitypolicyviolation", event => cspViolations.push({ directive: event.effectiveDirective, blocked: event.blockedURI }));
window.addEventListener("DOMContentLoaded", async () => {
  const report = { ok: false, status: false, unknownDenied: false, shellDenied: false, pathDenied: false, windowDenied: false };
  try {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const status = await invoke("agent_get_status");
    report.status = typeof status.agent_version === "string" && Array.isArray(status.bindings);
    report.agentVersion = status.agent_version;
    report.snapshotVersion = status.snapshot_version;
    if (status.management?.connection?.authenticated !== true) {
      const end = Date.now() + 15000;
      while (!document.querySelector('[name="username"]') && Date.now() < end) await new Promise(resolve => setTimeout(resolve, 100));
      report.loginGate = !!document.querySelector('[role="dialog"] [name="username"]') && !document.querySelector('.mantine-AppShell-root');
      const nonce = document.querySelector('meta[name="csp-nonce"]')?.content;
      report.nonceStyles = /^[0-9a-f]{32}$/.test(nonce ?? '') && [...document.querySelectorAll('style')].every(style => style.nonce === nonce);
      const prior = sessionStorage.getItem('vaultsyncd.s176.prior-nonce');
      if (prior === null) {
        sessionStorage.setItem('vaultsyncd.s176.prior-nonce', nonce);
        location.reload(); return;
      }
      sessionStorage.removeItem('vaultsyncd.s176.prior-nonce');
      report.documentNonceChanged = /^[0-9a-f]{32}$/.test(prior) && prior !== nonce;
      for (const path of ['/connections','/bindings','/files','/history','/problems']) {
        location.hash = path;
        await new Promise(resolve => setTimeout(resolve, 100));
        if (document.querySelector('.mantine-AppShell-root') || !document.querySelector('[role="dialog"]')) throw new Error(`login gate bypass: ${path}`);
      }
      report.routesRequireLogin = true;
      for (const [field, command, args] of [
        ['unknownDenied', 'arbitrary_command', {}],
        ['shellDenied', 'plugin:shell|execute', {program:'cmd.exe',args:['/c','exit']}],
        ['pathDenied', 'plugin:fs|read_text_file', {path:'C:\\Windows\\win.ini'}],
        ['windowDenied', 'plugin:window|set_title', {label:'main',title:'unauthorized'}],
      ]) { try { await invoke(command,args); } catch { report[field]=true; } }
      report.cspViolations = cspViolations;
      report.ok = report.status && report.loginGate && report.routesRequireLogin && report.nonceStyles && report.documentNonceChanged && cspViolations.length === 0 && report.unknownDenied && report.shellDenied && report.pathDenied && report.windowDenied;
      await invoke('acceptance_report', {report}); return;
    }
    const deadline = Date.now() + 15000;
    while (!document.querySelector(".mantine-AppShell-root") && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 100));
    const nonce = document.querySelector('meta[name="csp-nonce"]')?.content;
    report.mantineShell = !!document.querySelector(".mantine-AppShell-root");
    const connectionDeadline = Date.now() + 15000;
    while (document.querySelector("[data-agent-feedback]")?.textContent !== "本机 agent 状态已读取。" && Date.now() < connectionDeadline) await new Promise(resolve => setTimeout(resolve, 100));
    report.initialConnectionConsistent = document.querySelector(".mantine-AppShell-header")?.textContent.includes("agent 已连接") && document.querySelector("[data-agent-feedback]")?.textContent === "本机 agent 状态已读取。";
    report.nonceStyles = typeof nonce === "string" && /^[0-9a-f]{32}$/.test(nonce) && document.querySelectorAll("style").length > 0 && [...document.querySelectorAll("style")].every(style => style.nonce === nonce);
    const priorNonce = sessionStorage.getItem("vaultsyncd.s152.prior-nonce");
    if (priorNonce === null) {
      if (!report.nonceStyles) throw new Error("first document nonce/styles missing");
      sessionStorage.setItem("vaultsyncd.s152.prior-nonce", nonce);
      location.reload();
      return;
    }
    sessionStorage.removeItem("vaultsyncd.s152.prior-nonce");
    report.documentNonceChanged = /^[0-9a-f]{32}$/.test(priorNonce) && priorNonce !== nonce;
    report.header48 = document.querySelector(".mantine-AppShell-header")?.getBoundingClientRect().height === 48;
    const beforeTheme = document.documentElement.dataset.mantineColorScheme;
    document.querySelector('button[aria-label="切换主题"]')?.click();
    await new Promise(resolve => setTimeout(resolve, 100));
    report.themeSwitch = document.documentElement.dataset.mantineColorScheme !== beforeTheme && ["light", "dark"].includes(localStorage.getItem("mantine-color-scheme-value"));
    report.routes = [];
    for (const [path, heading] of [["/connections", "连接设置"], ["/bindings", "同步目录"], ["/files", "文件状态与操作"], ["/history", "历史与回收站"], ["/problems", "问题与恢复"]]) {
      location.hash = path;
      const end = Date.now() + 10000;
      while (document.querySelector("main h1")?.textContent !== heading && Date.now() < end) await new Promise(resolve => setTimeout(resolve, 100));
      if (document.querySelector("main h1")?.textContent !== heading) throw new Error(`native route failed: ${path}`);
      if (document.documentElement.scrollWidth > innerWidth) throw new Error(`native layout overflow: ${path}`);
      report.routes.push(path);
    }
    report.nonceStyles = report.nonceStyles && [...document.querySelectorAll("style")].every(style => style.nonce === nonce);
    report.cspViolations = cspViolations;
    for (const [field, command, args] of [
      ["unknownDenied", "arbitrary_command", {}],
      ["shellDenied", "plugin:shell|execute", { program: "cmd.exe", args: ["/c", "exit"] }],
      ["pathDenied", "plugin:fs|read_text_file", { path: "C:\\Windows\\win.ini" }],
      ["windowDenied", "plugin:window|set_title", { label: "main", title: "unauthorized" }],
    ]) {
      try { await invoke(command, args); } catch { report[field] = true; }
    }
    report.ok = report.status && report.initialConnectionConsistent && report.documentNonceChanged && report.mantineShell && report.header48 && report.themeSwitch && report.routes.length === 5 && report.nonceStyles && report.cspViolations.length === 0 && report.unknownDenied && report.shellDenied && report.pathDenied && report.windowDenied;
  } catch (error) { report.error = String(error); }
  await window.__TAURI_INTERNALS__.invoke("acceptance_report", { report });
});
