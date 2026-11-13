import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const read = path => readFileSync(new URL(path, import.meta.url), "utf8");
const sorted = commands => [...commands].sort();
const manifests = [...read("../src-tauri/build.rs").matchAll(/&\[\s*((?:"[^"]+",?\s*)+)\]/g)]
  .map(match => [...match[1].matchAll(/"([^"]+)"/g)].map(command => command[1]));
const handlers = [...read("../src-tauri/src/shell.rs").matchAll(/tauri::generate_handler!\[([\s\S]*?)\]/g)]
  .map(match => match[1].split(/\W+/).filter(Boolean));
assert.equal(manifests.length, 2, "check both acceptance and production command manifests");
assert.equal(handlers.length, 2, "check both production and acceptance handlers");
for (const manifest of manifests) {
  assert(manifest.includes("open_binding_directory"));
  assert(handlers.some(handler => JSON.stringify(sorted(handler)) === JSON.stringify(sorted(manifest))),
    "every command manifest must match its invoke handler, including native-acceptance");
}
const main = JSON.parse(read("../src-tauri/capabilities/main.json"));
const file = JSON.parse(read("../src-tauri/capabilities/shell-file.json"));
assert(main.local && main.windows.length === 1 && main.windows[0] === "main");
assert(main.permissions.includes("allow-open-binding-directory"), "main window must be allowed to open a binding");
assert(!file.permissions.includes("allow-open-binding-directory"), "file window must not gain binding-management authority");
const production = manifests.find(manifest => !manifest.includes("acceptance_report"));
const allowed = new Set([...main.permissions, ...file.permissions].map(permission => permission.replace(/^allow-/, "").replaceAll("-", "_")));
assert.deepEqual(sorted(allowed), sorted(production), "production capabilities must explicitly cover exactly the registered app commands");
console.log("native commands: production/acceptance manifest-handler parity, explicit main permission and file-window isolation passed");
