import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";

// Resolve through HeroUI's own dependencies, without relying on pnpm's layout.
const require = createRequire(import.meta.url);
const fromHero = createRequire(require.resolve("@heroui/react/button"));
const aria = dirname(dirname(fromHero.resolve("react-aria")));
const policy = readFileSync(new URL("../../server/src/web.rs", import.meta.url), "utf8")
  .match(/CONTENT_SECURITY_POLICY,HeaderValue::from_static\("([^"\n]+)"\)/)?.[1];
assert.ok(policy, "production CSP must be readable");
assert.ok(!policy.includes("unsafe-inline"), "do not grant arbitrary inline styles/scripts");
for (const path of ["interactions/usePress.mjs", "overlays/usePreventScroll.mjs"]) {
  const source = readFileSync(resolve(aria, "private", path), "utf8");
  const template = source.match(/style\.textContent = `([\s\S]*?)`\.trim\(\)/)?.[1];
  assert.ok(template, `review React Aria's style injection after upgrading: ${path}`);
  const css = template.replace(/\$\{[^}]*PRESSABLE_ATTRIBUTE\}/g, "data-react-aria-pressable").trim();
  assert.ok(!css.includes("${"), "review new style interpolation before adding CSP hashes");
  const hash = createHash("sha256").update(css).digest("base64");
  assert.ok(policy.includes(`'sha256-${hash}'`), `production CSP must permit exact React Aria style: ${path} ${hash}`);
}
console.log("CSP: locked React Aria press/scroll styles match exact production hashes; no unsafe-inline");
