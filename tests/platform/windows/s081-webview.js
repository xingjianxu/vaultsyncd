// Real WebView + real React forms + real named pipe. Test feature only.
const cspViolations = [];
document.addEventListener("securitypolicyviolation", event => cspViolations.push({ directive: event.effectiveDirective, blocked: event.blockedURI }));
window.addEventListener("DOMContentLoaded", async () => {
  const report = { ok: false };
  const fixture = window.__S081_FIXTURE__;
  const delay = () => new Promise(resolve => setTimeout(resolve, 150));
  async function wait(label, predicate, timeout = 120000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) { if (await predicate()) return; await delay(); }
    throw new Error(`timeout ${label}`);
  }
  const text = () => document.body.innerText;
  function field(name, value) {
    const element = document.querySelector(`[name="${name}"]`);
    if (!element) throw new Error(`missing field ${name}`);
    const proto = element instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value").set.call(element, value);
    element.dispatchEvent(new Event(element instanceof HTMLSelectElement ? "change" : "input", { bubbles: true }));
  }
  async function click(label) {
    await wait(label, () => [...document.querySelectorAll("button")].some(b => b.textContent === label && !b.disabled));
    [...document.querySelectorAll("button")].find(b => b.textContent === label && !b.disabled).click();
    await delay();
  }
  const confirmed = () => document.querySelector("[data-agent-feedback]")?.textContent === "操作已由 agent 确认。";
  try {
    const invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args);
    const currentConnection = async () => (await invoke("agent_get_status")).management.connection;
    const saveButton = () => [...document.querySelectorAll("button")].find(b => b.textContent === "登录");
    const candidate = () => document.querySelector('[name="server_id"]');
    await wait("React agent snapshot", () => !!document.querySelector('[name="server_url"]'));
    const original = await currentConnection();
    field("server_url", fixture.url); field("ca_file", "");
    await wait("certificate rejection", () => text().includes("CERTIFICATE_INVALID"));
    report.certificateRejectedAndVisible = saveButton().disabled && !candidate();
    field("ca_file", fixture.caFile);
    await wait("automatic discovery", () => candidate()?.value === fixture.serverId);
    report.identityReadOnly = candidate().readOnly;
    report.discoveryDoesNotSave = JSON.stringify(await currentConnection()) === JSON.stringify(original);
    await click("取消");
    report.cancelDoesNotSave = !candidate() && saveButton().disabled && JSON.stringify(await currentConnection()) === JSON.stringify(original);
    await click("重新读取");
    await wait("retry discovery", () => candidate()?.value === fixture.serverId);
    field("server_url", fixture.url + "/delayed");
    report.editedAddressInvalidatesIdentity = !candidate() && saveButton().disabled;
    // Let a real discovery start, then edit the input. Its late reply cannot restore a candidate.
    await new Promise(resolve => setTimeout(resolve, 900));
    field("server_url", "");
    await new Promise(resolve => setTimeout(resolve, 3000));
    report.lateDiscoveryIgnored = !candidate() && saveButton().disabled;
    field("server_url", fixture.url);
    await wait("identity after edit", () => candidate()?.value === fixture.serverId);
    try {
      await invoke("agent_set_connection", { input: { serverId: "00".repeat(16), serverUrl: fixture.url, caFile: fixture.caFile } });
      throw new Error("mismatched identity accepted");
    } catch (cause) {
      report.saveRechecksIdentity = String(cause.message ?? cause).includes("BINDING_BLOCKED") && JSON.stringify(await currentConnection()) === JSON.stringify(original);
      if (!report.saveRechecksIdentity) throw cause;
    }
    async function timeoutDoesNotSave(command, input) {
      const started = Date.now();
      try { await invoke(command, { input }); throw new Error("slow identity request accepted"); }
      catch (cause) {
        if (cause.code !== "agent_rejected" || !String(cause.message ?? cause).includes("UNAVAILABLE")) throw cause;
      }
      if (Date.now() - started >= 5000) throw new Error("identity deadline exceeded IPC budget");
      await new Promise(resolve => setTimeout(resolve, 3500));
      if (JSON.stringify(await currentConnection()) !== JSON.stringify(original)) throw new Error("timeout changed durable connection");
    }
    await timeoutDoesNotSave("agent_discover_connection", { serverUrl: fixture.url + "/timeout", caFile: fixture.caFile });
    await timeoutDoesNotSave("agent_set_connection", { serverId: fixture.serverId, serverUrl: fixture.url + "/timeout", caFile: fixture.caFile });
    report.identityTimeoutDoesNotSave = true;
    field("username", fixture.login); field("password", fixture.password);
    await click("登录"); await wait("authenticated", () => text().includes("已登录（持有凭据"));
    report.confirmSavesIdentity = (await currentConnection()).server_id === fixture.serverId && (await currentConnection()).ca_file === fixture.caFile;
    report.passwordCleared = !document.querySelector('[name="password"]');
    location.hash = "/bindings";
    await wait("binding page", () => !!document.querySelector('[name="vault_id"]'));
    await click("读取授权 Vault"); await wait("authorized vaults", () => document.querySelector(`option[value="${fixture.vault}"]`));
    const option = document.querySelector(`option[value="${fixture.vault}"]`);
    report.vaultNameDisplayed = option.textContent === fixture.vaultName && option.textContent !== fixture.vault;
    const pathInput = () => document.querySelector('[name="local_path"]');
    report.pathReadOnly = pathInput().readOnly;
    report.noSelectionCannotSubmit = [...document.querySelectorAll("button")].find(b => b.textContent === "创建绑定并开始合并").disabled;
    field("vault_id", fixture.vault);
    await click("选择目录…");
    await wait("native selection", () => pathInput().value === fixture.localPath);
    report.nativeDirectorySelected = true;
    await click("选择目录…");
    await wait("native cancellation", () => ![...document.querySelectorAll("button")].find(b => b.textContent === "选择目录…").disabled);
    report.cancelKeepsSelection = pathInput().value === fixture.localPath;
    try {
      await window.__TAURI_INTERNALS__.invoke("agent_create_binding", {input:{bindingId:crypto.randomUUID().replaceAll("-",""),vaultId:fixture.vault,localPath:"Z:\\vaultsyncd-S081-missing",mode:"full"}});
      throw new Error("missing path was accepted");
    } catch (error) {
      report.invalidPathVisible = String(error.message ?? error).includes("LOCAL_PATH_UNAVAILABLE");
      if (!report.invalidPathVisible) throw error;
    }
    field("mode", "full");
    await click("创建绑定并开始合并"); await wait("binding created", () => text().includes("绑定已建立"));
    await wait("first merge", async () => {
      let status;
      try { status = await window.__TAURI_INTERNALS__.invoke("agent_get_status"); }
      catch (error) { report.statusReadError = JSON.stringify(error); return false; }
      report.snapshotVersion = status.snapshot_version;
      const info = status.management?.bindings?.find(b => b.local_path === fixture.localPath);
      if (!info) return false;
      report.bindingId = info.binding_id;
      report.tokenFreeResult = !/access_token|refresh_token|password/.test(JSON.stringify(status));
      return status.bindings.find(b => b.binding_id === info.binding_id)?.synchronized === true;
    });
    report.mergeCompleted = true;
    const bound = JSON.stringify(await currentConnection());
    try {
      await invoke("agent_set_connection", { input: { serverId: "00".repeat(16), serverUrl: fixture.url, caFile: fixture.caFile } });
      throw new Error("existing binding switched server");
    } catch (cause) {
      report.existingBindingProtectsIdentity = String(cause.message ?? cause).includes("BINDING_BLOCKED") && JSON.stringify(await currentConnection()) === bound;
      if (!report.existingBindingProtectsIdentity) throw cause;
    }

    await click("暂停"); await wait("pause visible", () => text().includes("已暂停")); report.pauseVisible = true;
    await click("恢复"); await wait("resume visible", () => !text().includes("已暂停")); report.resumeVisible = true;
    report.noBrowserCredentialStorage = Object.keys(localStorage).every(key => key === "mantine-color-scheme-value" && ["auto", "light", "dark"].includes(localStorage.getItem(key))) && sessionStorage.length === 0;
    const nonce = document.querySelector('meta[name="csp-nonce"]')?.content;
    report.nonceStyles = /^[0-9a-f]{32}$/.test(nonce ?? "") && [...document.querySelectorAll("style")].every(style => style.nonce === nonce);
    report.cspViolations = cspViolations;
    report.ok = ["identityReadOnly","discoveryDoesNotSave","cancelDoesNotSave","editedAddressInvalidatesIdentity","lateDiscoveryIgnored","saveRechecksIdentity","confirmSavesIdentity","existingBindingProtectsIdentity","identityTimeoutDoesNotSave"].every(key => report[key] === true) && report.nonceStyles && report.cspViolations.length === 0 && report.vaultNameDisplayed && report.pathReadOnly && report.noSelectionCannotSubmit && report.nativeDirectorySelected && report.cancelKeepsSelection && report.passwordCleared && report.tokenFreeResult && report.noBrowserCredentialStorage && report.certificateRejectedAndVisible && report.invalidPathVisible && report.mergeCompleted && report.pauseVisible && report.resumeVisible;
  } catch (error) { report.error = String(error?.message ?? error); report.page = text(); }
  await window.__TAURI_INTERNALS__.invoke("acceptance_report", { report });
});
