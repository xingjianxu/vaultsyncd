import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { createServer } from "vite";

const server = await createServer({ root: fileURLToPath(new URL("../../..", import.meta.url)), configFile: false,
  server: { middlewareMode: true, hmr: false } });
try {
  const { restoreStartupBindings } = await server.ssrLoadModule("/apps/desktop/src/services/startup-bindings.ts");
  const { AgentRequestError } = await server.ssrLoadModule("/apps/desktop/src/services/agent.ts");
  const binding = { binding_id: "1".repeat(32), local_path: "C:\\S231", paused: true, detached: false };
  for (const outcome of ["transient", "acknowledgement-lost", "detached", "permanent", "exhausted"]) {
    const current = { ...binding }; let resumes = 0; let reads = 0;
    const failures = await restoreStartupBindings({
      getStatus: async () => { reads++; return { management: { bindings: [current] } }; },
      resumeBinding: async () => {
        resumes++;
        if (outcome === "permanent") throw new AgentRequestError("binding blocked", false);
        if (outcome === "acknowledgement-lost") current.paused = false;
        if (outcome === "detached") current.detached = true;
        if (resumes === 1 || outcome === "exhausted") throw new AgentRequestError("temporary IPC loss", true);
        current.paused = false; return {};
      },
    }, [binding]);
    assert.equal(resumes, outcome === "transient" ? 2 : outcome === "exhausted" ? 3 : 1, outcome);
    assert.equal(failures.length, ["permanent", "exhausted"].includes(outcome) ? 1 : 0, outcome);
    if (outcome === "permanent") assert.equal(reads, 0);
  }
  let calls = 0;
  assert.deepEqual(await restoreStartupBindings({ resumeBinding: async () => { calls++; }, getStatus: async () => { calls++; } },
    [{ ...binding, detached: true }, { ...binding, paused: false }]), []);
  assert.equal(calls, 0);
  console.log("startup bindings: transient retry, ambiguous acknowledgement, concurrent detach, permanent rejection, bounded retries and inactive guards passed");
} finally { await server.close(); }
