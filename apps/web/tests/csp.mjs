import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { AppShell, MantineProvider, SimpleGrid } from "@mantine/core";

const policy = readFileSync(new URL("../../server/src/web.rs", import.meta.url), "utf8")
  .match(/pub const WEB_CSP: &str = "([^"\n]+)";/)?.[1];
assert.ok(policy, "production CSP template must be readable");
assert.ok(!policy.includes("unsafe-inline"), "do not grant arbitrary inline styles/scripts");
assert.ok(policy.includes("script-src 'self';"), "style nonce must never grant inline scripts");
assert.ok(policy.includes("style-src 'self' 'nonce-__STYLE_NONCE__';"));
assert.ok(!policy.includes("sha256-"), "removed React Aria style exceptions must not survive");

assert.ok(policy.includes("frame-src blob:;"), "only verified local PDF blobs may be framed");
assert.ok(policy.includes("object-src 'none';"), "do not allow general plugin embeds");

const nonce = "test-response-specific-nonce";
const markup = renderToStaticMarkup(createElement(MantineProvider, { getStyleNonce: () => nonce },
  createElement(AppShell, { header: { height: 60 }, navbar: { width: 240, breakpoint: "sm", collapsed: { mobile: true } }, padding: "md" },
    createElement(AppShell.Main, null, createElement(SimpleGrid, { cols: { base: 1, sm: 2 } }, "content")))));
const styles = [...markup.matchAll(/<style\b[^>]*>/g)].map(match => match[0]);
assert.ok(styles.length >= 2, "exercise Mantine global and responsive style injection");
for (const style of styles) assert.ok(style.includes(`nonce="${nonce}"`), `every generated style needs the response nonce: ${style}`);
assert.ok(!markup.includes("<script"), "Mantine layout must not inject scripts");
console.log("CSP: Mantine global/responsive styles carry the response nonce; scripts stay self-only");
