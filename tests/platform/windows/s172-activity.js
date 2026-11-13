// Test-only script in the actual Windows WebView, real named pipe and authenticated server.
const cspViolations = [];
document.addEventListener("securitypolicyviolation", event => cspViolations.push(event.effectiveDirective));
window.addEventListener("DOMContentLoaded", async () => {
  const report = { ok: false, checks: [] };
  const fixture = window.__S172_FIXTURE__;
  // Observe, never replace, real WebView -> named pipe -> TLS activity queries.
  // Store only activity arguments/timings: login arguments contain credentials.
  // Tauri defines invoke as non-writable/non-configurable. Observe its actual
  // custom-protocol fetch instead of silently failing to replace that property.
  const nativeFetch = window.fetch.bind(window);
  const activityUrl = window.__TAURI_INTERNALS__.convertFileSrc("agent_list_activity", "ipc");
  const activityCalls = [];
  window.fetch = async (input, options) => {
    if (String(input) !== activityUrl) return nativeFetch(input, options);
    const args = JSON.parse(options.body);
    const call = { bindingId: args.bindingId, cursor: args.cursor, direction: args.direction, started: performance.now() };
    activityCalls.push(call);
    try { return await nativeFetch(input, options); }
    finally { call.finished = performance.now(); }
  };
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
  const directions = () => tableRows().filter(row => row.children.length === 7).map(row => row.children[3].querySelector('[role="img"]')?.getAttribute("aria-label") ?? row.children[3].textContent);
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
    const operations = await invoke("agent_list_activity", { bindingId, direction: "operation" });
    check(downloads.activity.length > 0 && downloads.activity.every(row => row.direction === "download"), "download filter reaches actual API");
    check(uploads.activity.length > 0 && uploads.activity.every(row => row.direction === "upload"), "upload filter reaches actual API");
    check(uploads.activity.every(row => row.client_hostname === fixture.hostname), "activity hostname comes from the running Windows client");
    check(uploads.activity.every(row => row.client_os?.startsWith("Windows 11 (10.0.") && row.client_ip === "127.0.0.1"), "operating system and observed login IP survive HTTPS and IPC");
    check(operations.activity.length === fixture.operations.length && operations.activity.every(row => row.direction === "operation" && row.status === "committed" && row.bytes === undefined && row.offset === undefined), "operation filter reaches actual API without invented transfer bytes");
    for (const expected of fixture.operations) {
      const matches = operations.activity.filter(row => row.operation_id === expected.operation_id);
      check(matches.length === 1 && matches[0].action === expected.action && matches[0].path === expected.path, `real ${expected.action} operation keeps original path and is idempotent`);
    }
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
    const secondPageCall = activityCalls.at(-1);
    check(!!secondPageCall.cursor, "native UI second page uses an actual cursor");
    const secondPageRows = tableRows().map(row => row.innerText);
    await click("查看详情");
    await wait("second-page record details", () => !!document.querySelector('[role="dialog"]'));
    const details = document.querySelector('[role="dialog"]').innerText;
    let flashedEmpty = false;
    const observer = new MutationObserver(() => { if (!directions().length) flashedEmpty = true; });
    observer.observe(document.querySelector("main"), { childList: true, subtree: true });
    const baseline = activityCalls.length;
    await wait("native automatic second-page poll completes", () => activityCalls.length > baseline && activityCalls.at(-1).finished !== undefined, 15000);
    const automatic = activityCalls.slice(baseline);
    observer.disconnect();
    check(automatic.length > 0 && automatic.every(call => call.cursor === secondPageCall.cursor && call.bindingId === bindingId), "native automatic refresh remains on current page");
    check(automatic[0].started - secondPageCall.finished >= 1800, "native polling waits 2 seconds after previous response completes");
    check(automatic.every((call, index) => index === 0 || call.started - automatic[index - 1].finished >= 1800), "native automatic requests are serial and completion-spaced");
    check(document.querySelector('[role="dialog"]')?.innerText === details, "native details stay open across an actual polling cycle");
    check(JSON.stringify(tableRows().map(row => row.innerText)) === JSON.stringify(secondPageRows) && !flashedEmpty, "native automatic refresh never clears or resets the loaded page");
    await click("关闭");
    check([...document.querySelectorAll("button")].some(button => button.textContent === "第一页" && !button.disabled), "native second page remains selected after closing details");
    // Manual refresh intentionally differs from the background cursor-preserving poll.
    await click("刷新记录");
    await wait("manual refresh returns first UI page", () => text().includes("local-only.txt") && directions().length > 0);
    check(activityCalls.at(-1).cursor === undefined, "native manual refresh resets to first page");
    select("activity_direction", "operation");
    const operationLabels = ["新建目录", "移动/重命名", "删除", "恢复历史版本", "从回收站恢复"];
    await wait("UI operation filter", () => directions().length === fixture.operations.length && operationLabels.every(label => directions().includes(label)));
    check(directions().every(label => operationLabels.includes(label)), "native operation filter displays all five actions separately from upload and download");
    check(text().includes("S225-new-directory") && text().includes("S225-moved-directory") && text().includes("删除已提交"), "native operation table preserves historical paths and committed results");
    const deleteRow = tableRows().find(row => row.children[3].querySelector('[role="img"]')?.getAttribute("aria-label") === "删除");
    deleteRow.querySelector("button").click();
    await wait("deleted record details", () => document.querySelector('[role="dialog"]')?.textContent.includes("删除已提交"));
    check(document.querySelector('[role="dialog"]').textContent.includes("server-seed.txt") && !document.querySelector('[role="dialog"]').textContent.includes("本次提供（字节）"), "native delete details show the actual path without transfer evidence");
    await click("关闭");
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
    // A real in-flight query is abandoned on route leave; the page must not keep
    // a background timer alive after its UI has unmounted.
    select("activity_direction", "download");
    await sleep(200);
    location.hash = "/problems";
    await wait("leave activity route", () => document.querySelector('main h1')?.textContent === "问题与恢复");
    await sleep(2000); // let the TLS-delayed old reply arrive
    const leftRouteCalls = activityCalls.length;
    await sleep(2600);
    check(activityCalls.length === leftRouteCalls, "native route leave stops activity polling despite delayed reply");
    location.hash = "/activity";
    await wait("native activity remount binding snapshot", () => !!document.querySelector(`[name="activity_binding"] option[value="${bindingId}"]`));
    const bindingSelect = document.querySelector('[name="activity_binding"]');
    check(bindingSelect.value === bindingSelect.options[0].value, "native remount defaults to the first available binding");
    await wait("native remount default binding read completes", () => activityCalls.some(call => call.started > activityCalls[leftRouteCalls - 1].started && call.bindingId === bindingSelect.value && call.finished !== undefined));
    if (bindingSelect.value === emptyBinding) {
      await wait("native remount empty default Vault", () => text().includes("当前筛选没有记录。"));
      check(directions().length === 0, "native empty default binding is represented by its actual empty records");
    }
    select("activity_binding", bindingId);
    await wait("native selected binding loads automatically after remount", () => directions().length > 0);
    check(!document.querySelector('[role="alert"]'), "page recovers after reauthentication");
    check(document.documentElement.scrollWidth <= innerWidth, "native desktop page fits window");
    check(cspViolations.length === 0, "native activity page has no CSP violations");
    report.liveRefresh = { secondPageCursor: secondPageCall.cursor, automaticCycles: automatic.length, firstPollDelayMs: Math.round(automatic[0].started - secondPageCall.finished), rowsRetained: !flashedEmpty };
    report.bindingId = bindingId;
    report.sample = page.activity.slice(0, 6);
    report.operations = operations.activity;
    report.cspViolations = cspViolations;
    report.ok = true;
  } catch (error) { report.error = error instanceof Error ? String(error) : JSON.stringify(error); }
  await invoke("acceptance_report", { report });
});
