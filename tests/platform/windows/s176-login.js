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
  const status = async () => (await invoke("agent_get_status")).management.connections.find(c => c.server_id === fixture.serverId);
  const main = () => !!document.querySelector(".mantine-AppShell-root");
  const dialog = () => document.querySelector('[role="dialog"]');
  const input = name => document.querySelector(`input[name="${name}"]`);
  const feedback = () => document.querySelector("[data-agent-feedback]")?.textContent;
  const ready = () => feedback() === "本机 agent 状态已读取。";
  function assert(value, label) { if (!value) throw new Error(label); report[label] = true; }
  function field(name, value) {
    const node = input(name);
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
    await wait("initial shell and IPC", () => main() && ready());
    assert(!dialog() && !input("password"), "startupDoesNotPromptForLogin");
    assert(/^vaultsyncd \d+\.\d+\.\d+/.test(document.querySelector("[data-client-version]")?.textContent ?? ""), "shellShowsClientVersion");
    if (fixture.phase === "fresh") {
      assert(document.querySelector("main h1")?.textContent === "同步目录", "freshStartupShowsBindings");
      await wait("add server action", () => document.querySelector('a[href="#/connections/add"]'));
      document.querySelector('a[href="#/connections/add"]').click();
      await wait("add form", () => input("server_url"));
      assert(!dialog() && !input("password"), "discoveryPrecedesCredentials");
      field("server_url", fixture.url);
      await wait("certificate error", () => document.body.innerText.includes("CERTIFICATE_INVALID"));
      assert(!(await status()) && main() && !input("password"), "certificateFailureLeavesConnectionUnsaved");
      field("ca_file", fixture.caFile);
      const identity = () => document.querySelector('textarea[name="server_id"]');
      await wait("server identity", () => identity()?.value === fixture.serverId);
      assert(identity().readOnly, "serverIdentityReadOnly");
      await click("确认并保存连接");
      await wait("explicit login form", () => input("password"));
      field("username", fixture.login); field("password", fixture.password + "wrong");
      await click("登录");
      await wait("failed login", () => feedback() && !["操作已由 agent 确认。", "操作正在执行，等待 agent 确认。"].includes(feedback()) && !input("password")?.value);
      assert(main() && !(await status())?.authenticated, "wrongPasswordPreservesMain");
      assert(input("username")?.value === fixture.login, "failedLoginKeepsUsername");
      field("password", fixture.password);
      await click("登录");
      await wait("login confirmed", async () => !input("password") && (await status())?.authenticated);
      assert(location.hash === "#/connections", "loginRemainsInConnectionSettings");
      assert((await status()).username === fixture.login && (await status()).authenticated, "loginMetadataReturned");
    } else if (fixture.phase === "restored") {
      const connection = await status();
      assert(connection?.authenticated && connection.username === fixture.login && connection.server_url === fixture.url, "restartRestoresSessionAndMetadata");
      location.hash = "/connections";
      await click("退出登录");
      await wait("logout status", async () => (await status())?.authenticated === false);
      assert(main() && !dialog() && !input("password"), "logoutPreservesMainWithoutLoginPrompt");
      assert((await status()).username === fixture.login && (await status()).server_url === fixture.url, "logoutRetainsMetadata");
    } else {
      const connection = await status();
      assert(main() && connection?.authenticated === false, "logoutSurvivesRestart");
      assert(connection.username === fixture.login && connection.server_url === fixture.url, "metadataSurvivesLogoutAndRestart");
      location.hash = "/connections";
      await click("登录");
      assert(input("username")?.value === fixture.login && input("password")?.value === "", "explicitReloginPrefillsUsernameOnly");
      await click("取消");
      assert(main() && !input("password"), "explicitLoginCanBeDismissed");
    }
    const serialized = JSON.stringify(await invoke("agent_get_status"));
    assert(!/access_token|refresh_token|password/.test(serialized), "statusContainsNoCredentials");
    assert(Object.keys(localStorage).every(key => key === "mantine-color-scheme-value") && sessionStorage.length === 0, "noBrowserCredentialStorage");
    assert(violations.length === 0, "strictCspPreserved");
    report.ok = true;
  } catch (cause) { report.error = String(cause?.message ?? cause); report.page = document.body.innerText; }
  await invoke("acceptance_report", { report });
});
