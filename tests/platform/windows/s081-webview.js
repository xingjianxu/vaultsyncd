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
    await wait("React agent snapshot", () => document.querySelector('[name="server_id"]')?.value?.length === 32);
    field("server_id", fixture.serverId); field("server_url", fixture.url); field("ca_file", "");
    await click("保存连接"); await wait("save endpoint", confirmed);
    field("username", fixture.login); field("password", fixture.password);
    await click("登录"); await wait("certificate rejection", () => text().includes("CERTIFICATE_INVALID"));
    report.certificateRejectedAndVisible = true;
    field("ca_file", fixture.caFile); await click("保存连接"); await wait("save trust", confirmed);
    field("username", fixture.login); field("password", fixture.password);
    await click("登录"); await wait("authenticated", () => text().includes("已登录（持有凭据"));
    report.passwordCleared = document.querySelector('[name="password"]').value === "";
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
    await click("暂停"); await wait("pause visible", () => text().includes("已暂停")); report.pauseVisible = true;
    await click("恢复"); await wait("resume visible", () => !text().includes("已暂停")); report.resumeVisible = true;
    report.noBrowserCredentialStorage = Object.keys(localStorage).every(key => key === "mantine-color-scheme-value" && ["auto", "light", "dark"].includes(localStorage.getItem(key))) && sessionStorage.length === 0;
    const nonce = document.querySelector('meta[name="csp-nonce"]')?.content;
    report.nonceStyles = /^[0-9a-f]{32}$/.test(nonce ?? "") && [...document.querySelectorAll("style")].every(style => style.nonce === nonce);
    report.cspViolations = cspViolations;
    report.ok = report.nonceStyles && report.cspViolations.length === 0 && report.vaultNameDisplayed && report.pathReadOnly && report.noSelectionCannotSubmit && report.nativeDirectorySelected && report.cancelKeepsSelection && report.passwordCleared && report.tokenFreeResult && report.noBrowserCredentialStorage && report.certificateRejectedAndVisible && report.invalidPathVisible && report.mergeCompleted && report.pauseVisible && report.resumeVisible;
  } catch (error) { report.error = String(error?.message ?? error); report.page = text(); }
  await window.__TAURI_INTERNALS__.invoke("acceptance_report", { report });
});
