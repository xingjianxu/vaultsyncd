// Test feature only: real WebView forms -> Tauri -> named pipe -> HTTPS -> Windows credentials.
const violations = [];
document.addEventListener("securitypolicyviolation", event => violations.push(event.effectiveDirective));
window.addEventListener("DOMContentLoaded", async () => {
  const fixture = window.__S176_FIXTURE__;
  const report = { ok: false, phase: fixture.phase };
  const invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args);
  const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
  const wait = async (label, predicate) => {
    const end = Date.now() + 60000;
    while (Date.now() < end) { if (await predicate()) return; await pause(100); }
    throw new Error(`timeout ${label}`);
  };
  const status = async () => (await invoke("agent_get_status")).management.connection;
  const main = () => !!document.querySelector(".mantine-AppShell-root");
  const dialog = () => document.querySelector('[role="dialog"]');
  function assert(value, label) { if (!value) throw new Error(label); report[label] = true; }
  function field(name, value) {
    const node = document.querySelector(`[name="${name}"]`);
    if (!node) throw new Error(`missing ${name}`);
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(node, value);
    node.dispatchEvent(new Event("input", { bubbles: true }));
  }
  async function click(label) {
    await wait(label, () => [...document.querySelectorAll("button")].some(b => b.textContent === label && !b.disabled));
    [...document.querySelectorAll("button")].find(b => b.textContent === label && !b.disabled).click();
    await pause(150);
  }
  try {
    if (fixture.phase === "fresh") {
      location.hash = "/bindings";
      await wait("login form", () => !!document.querySelector('[name="username"]'));
      assert(!main(), "unauthenticatedHasNoMain");
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      await pause(200);
      assert(!!dialog() && !main(), "loginCannotBeDismissed");
      field("server_url", fixture.url);
      await wait("certificate error", () => document.body.innerText.includes("CERTIFICATE_INVALID"));
      assert(!(await status()).authenticated && !main(), "certificateFailureStaysInLogin");
      field("ca_file", fixture.caFile);
      await wait("server identity", () => document.querySelector('[name="server_id"]')?.value === fixture.serverId);
      assert(document.querySelector('[name="server_id"]').readOnly, "serverIdentityReadOnly");
      field("username", fixture.login); field("password", fixture.password + "wrong");
      await click("登录");
      await wait("failed login", () => document.querySelector('[role="alert"]') && !document.querySelector('[name="password"]').value);
      assert(!main() && !(await status()).authenticated, "wrongPasswordStaysInLogin");
      assert(document.querySelector('[name="username"]').value === fixture.login, "failedLoginKeepsUsername");
      field("password", fixture.password);
      await click("登录"); await wait("main after login", main);
      assert(location.hash === "#/bindings", "loginPreservesRoute");
      assert(!document.querySelector('[name="password"]'), "passwordFormUnmounted");
      assert((await status()).username === fixture.login && (await status()).authenticated, "loginMetadataReturned");
    } else if (fixture.phase === "restored") {
      await wait("restored main", main);
      const connection = await status();
      assert(connection.authenticated && connection.username === fixture.login && connection.server_url === fixture.url && connection.server_id === fixture.serverId, "restartRestoresSessionAndMetadata");
      assert(!dialog() && !document.querySelector('[name="password"]'), "restartSkipsLogin");
      await click("退出登录");
      await wait("logout login form", () => !!document.querySelector('[name="username"]'));
      assert(!main() && !(await status()).authenticated, "logoutRemovesMain");
      assert(document.querySelector('[name="username"]').value === fixture.login && document.querySelector('[name="server_url"]').value === fixture.url, "logoutRetainsMetadata");
      assert(document.querySelector('[name="password"]').value === "", "logoutPasswordEmpty");
    } else {
      await wait("logged out restart", () => !!document.querySelector('[name="username"]'));
      assert(!main() && !(await status()).authenticated, "logoutSurvivesRestart");
      assert(document.querySelector('[name="username"]').value === fixture.login && document.querySelector('[name="server_url"]').value === fixture.url, "metadataSurvivesLogoutAndRestart");
      assert(document.querySelector('[name="password"]').value === "", "restartPasswordEmpty");
    }
    const serialized = JSON.stringify(await invoke("agent_get_status"));
    assert(!/access_token|refresh_token|password/.test(serialized), "statusContainsNoCredentials");
    assert(Object.keys(localStorage).every(key => key === "mantine-color-scheme-value") && sessionStorage.length === 0, "noBrowserCredentialStorage");
    assert(violations.length === 0, "strictCspPreserved");
    report.ok = true;
  } catch (cause) { report.error = String(cause?.message ?? cause); report.page = document.body.innerText; }
  await invoke("acceptance_report", { report });
});
