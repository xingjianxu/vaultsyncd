// Test-only script in the actual Windows WebView, real named pipe and authenticated server.
const cspViolations = [];
document.addEventListener("securitypolicyviolation", event => cspViolations.push(event.effectiveDirective));
window.addEventListener("DOMContentLoaded", async () => {
  const report = { ok: false, checks: [] };
  const fixture = window.__S172_FIXTURE__;
  const invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args);
  const sleep = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
  const text = () => document.body.innerText;
  const check = (condition, label) => { if (!condition) throw new Error(label); report.checks.push(label); };
  async function wait(label, predicate, timeout = 120000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) { if (await predicate()) return; await sleep(100); }
    throw new Error(`timeout: ${label}`);
  }
  function select(name, value) {
    const element = document.querySelector(`[name="${name}"]`);
    if (!element) throw new Error(`missing select ${name}`);
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value").set.call(element, value);
    element.dispatchEvent(new Event("change", { bubbles: true }));
  }
  async function click(label) {
    await wait(label, () => [...document.querySelectorAll("button")].some(button => button.textContent === label && !button.disabled));
    [...document.querySelectorAll("button")].find(button => button.textContent === label && !button.disabled).click();
    await sleep(100);
  }
  async function rejected(label, args, expected) {
    try { await invoke("agent_list_activity", args); throw new Error("unexpected success"); }
    catch (error) { check(String(error.message ?? error).includes(expected), label); }
  }
  const tableRows = () => [...document.querySelectorAll('table[aria-label="同步与传输记录当前页"] tbody tr')];
  const directions = () => tableRows().filter(row => row.children.length === 7).map(row => row.children[3].textContent);
  async function synchronized(bindingId) {
    try { return (await invoke("agent_get_status")).bindings?.find(item => item.binding_id === bindingId)?.synchronized === true; }
    catch (error) { report.lastStatusError = JSON.stringify(error); return false; }
  }
  try {
    await wait("React initial snapshot", () => document.querySelector("[data-agent-feedback]")?.textContent === "本机 agent 状态已读取。");
    await invoke("agent_set_connection", { input: { serverId: fixture.serverId, serverUrl: fixture.url, caFile: fixture.caFile } });
    check(true, "confirmed identity and private CA configured");
    await invoke("agent_login", { serverId: fixture.serverId, username: fixture.login, password: fixture.password });
    check(true, "native agent authenticated with real server");
    const bindingId = crypto.randomUUID().replaceAll("-", "");
    await invoke("agent_create_binding", { input: { bindingId, serverId: fixture.serverId, vaultId: fixture.vault, localPath: fixture.localPath, mode: "full" } });
    check(true, "native binding created through real IPC");
    await wait("actual first merge", () => synchronized(bindingId));
    check(true, "full first merge uses actual Windows NTFS upload and download");
    const emptyBinding = crypto.randomUUID().replaceAll("-", "");
    await invoke("agent_create_binding", { input: { bindingId: emptyBinding, serverId: fixture.serverId, vaultId: fixture.emptyVault, localPath: fixture.emptyPath, mode: "full" } });
    await wait("empty merge", () => synchronized(emptyBinding));
    const page = await invoke("agent_list_activity", { bindingId });
    check(Array.isArray(page.activity) && page.activity.length > 0 && !!page.next_cursor, "real IPC forwards authenticated activity and pagination");
    check(page.activity.some(row => row.client_hostname === fixture.hostname), "native client's identity is recorded");
    check(page.activity.some(row => row.client_hostname !== fixture.hostname), "authorized Vault includes another client's records");
    check(page.activity.some(row => row.action === "sync_commit" && row.status === "committed" && row.path === "local-only.txt"), "upload sync committed with original file path");
    check(page.activity.some(row => row.action === "upload_finalize" && row.status === "ready"), "upload validation is separate from sync commit");
    const served = page.activity.find(row => row.action === "download" && row.status === "served");
    check(!!served?.version_id && served.offset !== undefined && served.bytes !== undefined, "fixed-version served range carries offset and bytes");
    check(page.activity.every(row => /^[1-9][0-9]*$/.test(row.id) && /^[0-9]+$/.test(row.occurred_at)), "record identities and observed time are decimal strings");
    const second = await invoke("agent_list_activity", { bindingId, cursor: page.next_cursor });
    check(second.activity.length > 0 && BigInt(second.activity[0].id) < BigInt(page.activity.at(-1).id), "descending cursor pagination has no overlap");
    const downloads = await invoke("agent_list_activity", { bindingId, direction: "download" });
    const uploads = await invoke("agent_list_activity", { bindingId, direction: "upload" });
    check(downloads.activity.length > 0 && downloads.activity.every(row => row.direction === "download"), "download filter reaches actual API");
    check(uploads.activity.length > 0 && uploads.activity.every(row => row.direction === "upload"), "upload filter reaches actual API");
    check(uploads.activity.every(row => row.client_hostname === fixture.hostname), "activity hostname comes from the running Windows client");
    check(uploads.activity.every(row => row.client_os?.startsWith("Windows 11 (10.0.") && row.client_ip === "127.0.0.1"), "operating system and observed login IP survive HTTPS and IPC");
    report.clientProperties = uploads.activity.map(({client_hostname, client_os, client_ip}) => ({client_hostname, client_os, client_ip}));
    await rejected("IPC rejects noncanonical activity cursor", { bindingId, cursor: "01" }, "输入无效");
    await rejected("IPC rejects unknown activity direction", { bindingId, direction: "both" }, "输入无效");
    await rejected("unknown local binding cannot query another Vault", { bindingId: "00".repeat(16) }, "NOT_FOUND");
    location.hash = "/activity";
    await wait("activity route", () => document.querySelector('main h1')?.textContent === "同步与传输记录");
    await wait("binding snapshot", () => !!document.querySelector(`[name="activity_binding"] option[value="${bindingId}"]`));
    select("activity_binding", bindingId);
    await wait("real activity rows", () => directions().length > 0 && text().includes("local-only.txt"));
    check(text().includes("服务端记录") && text().includes("所有客户端"), "desktop identifies server-observed scope");
    await click("下一页");
    await wait("second UI page", () => directions().length > 0 && !text().includes("local-only.txt"));
    await click("第一页");
    await wait("first UI page", () => text().includes("local-only.txt") && directions().length > 0);
    select("activity_direction", "download");
    await wait("UI download filter", () => directions().length > 0 && directions().every(value => value === "下载"));
    check(text().includes("已提供下载内容") && !text().includes("下载完成"), "provided content is never called client download completion");
    await click("查看详情");
    await wait("served details", () => text().includes("客户端接收、校验与应用结果未上报"));
    check(text().includes("区间起点（字节）") && text().includes("版本"), "details expose fixed version and range");
    await click("关闭");
    // TLS proxy delays actual download-list responses. Switching filters must discard that reply.
    await click("刷新记录");
    await sleep(200);
    select("activity_direction", "upload");
    await wait("upload after in-flight download", () => directions().length > 0 && directions().every(value => value === "上传"));
    await sleep(2200);
    check(directions().every(value => value === "上传"), "late download reply cannot replace upload filter");
    select("activity_direction", "download");
    await sleep(200);
    select("activity_binding", emptyBinding);
    await wait("empty binding response", () => text().includes("当前筛选没有记录。"));
    await sleep(2200);
    check(directions().length === 0 && text().includes("当前筛选没有记录。"), "late reply from another binding cannot populate empty Vault");
    select("activity_binding", bindingId);
    select("activity_direction", "upload");
    await wait("upload rows restored", () => directions().length > 0 && directions().every(value => value === "上传"));
    await invoke("agent_logout", { serverId: fixture.serverId });
    await rejected("authenticated API rejects query after logout", { bindingId }, "AUTH");
    await wait("logout observed by desktop", () => text().includes("请先登录此绑定的服务器。"));
    check(directions().length === 0 && text().includes("尚未登录此绑定的服务器。"), "logout clears prior records and invalidates pending requests");
    await invoke("agent_login", { serverId: fixture.serverId, username: fixture.login, password: fixture.password });
    await wait("login context reset", () => !text().includes("请先登录此绑定的服务器。"));
    select("activity_binding", bindingId);
    await wait("records after login", () => directions().length > 0);
    check(text().includes(fixture.hostname), "client column shows hostname");
    await click("查看详情");
    await wait("client properties details", () => text().includes("登录来源 IP") && text().includes("Windows 11 (10.0."));
    check(text().includes("操作系统"), "operating system is a client property");
    await click("关闭");
    check(!document.querySelector('[role="alert"]'), "page recovers after reauthentication");
    check(document.documentElement.scrollWidth <= innerWidth, "native desktop page fits window");
    check(cspViolations.length === 0, "native activity page has no CSP violations");
    report.bindingId = bindingId;
    report.sample = page.activity.slice(0, 6);
    report.cspViolations = cspViolations;
    report.ok = true;
  } catch (error) { report.error = error instanceof Error ? String(error) : JSON.stringify(error); }
  await invoke("acceptance_report", { report });
});
