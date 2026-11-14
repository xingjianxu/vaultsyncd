// Loaded only by the native-acceptance WebView. Exercise the rendered app and real agent IPC.
document.addEventListener("DOMContentLoaded", async () => {
  const invoke = window.__TAURI_INTERNALS__.invoke;
  const report = { ok: false };
  const wait = async (predicate, label) => {
    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      const value = predicate();
      if (value) return value;
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    throw new Error(label);
  };
  try {
    const status = await invoke("agent_get_status");
    const connections = status.management?.connections;
    if (!Array.isArray(connections)) throw new Error("missing server list");
    await wait(() => document.querySelector('[aria-label="本地 Binding"]'), "binding table did not open on startup");
    if (document.querySelector('input[type="password"], [role="dialog"]')) {
      throw new Error("startup prompted for login");
    }
    report.mainWindowAtStartup = true;
    if (window.__S191_PHASE__ === 1) {
      if (connections.length !== 0) throw new Error("fresh profile unexpectedly has a connection");
      const add = await wait(
        () => Array.from(document.querySelectorAll("a,button")).find(node => node.textContent.trim() === "添加服务器"),
        "empty binding page has no add-server action",
      );
      add.click();
      await wait(
        () => document.querySelector('input[name="server_url"]'),
        "add-server form did not open",
      );
      report.emptyThenExplicitAdd = true;
    } else {
      if (connections.length !== 2 || connections.some(server => !server.authenticated)) {
        throw new Error("both configured servers must retain independent authenticated sessions");
      }
      const selector = await wait(() => {
        const selector = document.querySelector('select[name="server_id"]');
        return selector && connections.every(server => Array.from(selector.options).some(option => option.value === server.server_id)) && selector;
      }, "binding form does not offer both servers");
      const listedVaults = new Set();
      for (const server of connections) {
        const result = await invoke("agent_list_vaults", { serverId: server.server_id });
        if (result.items?.length !== 1) throw new Error("server-scoped Vault list did not reach its server");
        const vault = result.items[0].id;
        if (listedVaults.has(vault)) throw new Error("both test instances returned the same Vault");
        Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value").set.call(selector, server.server_id);
        selector.dispatchEvent(new Event("change", { bubbles: true }));
        const picker = await wait(() => document.querySelector('[role="combobox"]'), "missing Vault selector");
        picker.click();
        await wait(
          () => [...document.querySelectorAll('[role="option"]')].some(node => node.getAttribute("value") === vault),
          "desktop displayed the wrong server's Vault list",
        );
        if ([...document.querySelectorAll('[role="option"]')].some(node => listedVaults.has(node.getAttribute("value")))) {
          throw new Error("desktop retained the previous server's Vault");
        }
        listedVaults.add(vault);
        picker.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      }
      report.twoAuthenticatedServerChoices = true;
    }
    report.ok = true;
  } catch (error) {
    report.error = error instanceof Error ? error.message : String(error);
  }
  await invoke("acceptance_report", { report });
});
