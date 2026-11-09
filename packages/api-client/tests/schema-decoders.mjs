// 生成代码的运行时行为检查（`pnpm --filter @vaultsyncd/api-client test`）。
//
// TypeScript 只做类型检查；这里的检查证明**运行期**也不会把非法 wire 数据当成功：未知枚举、
// 非法十进制文本、缺失的必填字段都抛错，2^53 以上的 epoch 以字符串原样保留。样本直接取
// `api/examples/`，与 Rust 侧 `crates/protocol/tests/contract.rs` 用同一份数据。
//
// 直接以 .mjs 导入 .ts 源码：Node 24 会剥离类型注解，生成文件只用可擦除语法（interface、
// type、as const），因此不需要额外的构建步骤。

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  decodeCapabilities,
  decodeClientConnectionPage,
  decodeErrorCode,
  decodeErrorEnvelope,
  decodeHealthStatus,
} from "../src/generated/types.ts";

const examples = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "api", "examples");

/** 读取一个契约样本。 */
function example(name) {
  return JSON.parse(readFileSync(join(examples, name), "utf8"));
}

let checks = 0;
const failures = [];

/** 记录一项通过/失败的检查，保持与 tests/platform 脚本一致的输出形态。 */
function check(name, body) {
  checks += 1;
  try {
    body();
    console.log(`check=pass ${name}`);
  } catch (error) {
    failures.push(name);
    console.log(`check=fail ${name}: ${error.message}`);
  }
}

/** 断言某个调用抛出错误。 */
function expectThrow(body) {
  try {
    body();
  } catch (error) {
    return error;
  }
  throw new Error("期望抛错，实际成功返回");
}

check("health_ok", () => {
  assert.equal(decodeHealthStatus("ok"), "ok");
  assert.equal(decodeHealthStatus(example("health.response.json").status), "ok");
});

check("health_unknown_status_rejected", () => {
  const error = expectThrow(() => decodeHealthStatus("needs_migration"));
  assert.match(error.message, /未知的 HealthStatus/);
});

check("error_code_closed_set", () => {
  assert.equal(decodeErrorCode("VAULT_UNAVAILABLE"), "VAULT_UNAVAILABLE");
  const error = expectThrow(() => decodeErrorCode("LOCAL_CHANGED"));
  assert.match(error.message, /未知的 ErrorCode/);
});

check("error_envelope_example_decodes", () => {
  const envelope = decodeErrorEnvelope(example("error.response.json"));
  assert.equal(envelope.error.code, "VAULT_UNAVAILABLE");
  assert.equal(envelope.error.retryable, true);
  assert.equal(envelope.error.vault_id, "fedcba9876543210fedcba9876543210");
  assert.equal(envelope.error.details.reason, "mount_identity_changed");
});

check("capabilities_example_decodes", () => {
  const capabilities = decodeCapabilities(example("capabilities.response.json"));
  assert.equal(capabilities.protocol.major, 2);
  assert.equal(capabilities.server_id, "0123456789abcdef0123456789abcdef");
  assert.equal(capabilities.epoch, "9007199254740993");
  assert.ok(capabilities.features.includes("feature_from_a_newer_server"));
  assert.equal(capabilities.limits.max_page_size, 500);
  assert.equal(capabilities.limits.max_upload_part_bytes, "67108864");
});

check("epoch_as_number_rejected", () => {
  const sample = example("capabilities.response.json");
  const error = expectThrow(() => decodeCapabilities({ ...sample, epoch: 9007199254740993 }));
  assert.match(error.message, /Capabilities\.epoch/);
});

check("non_canonical_epoch_rejected", () => {
  const sample = example("capabilities.response.json");
  for (const text of ["01", "-1", "1.0", "9007199254740993 "]) {
    const error = expectThrow(() => decodeCapabilities({ ...sample, epoch: text }));
    assert.match(error.message, /Capabilities\.epoch/);
  }
});

check("missing_required_field_rejected", () => {
  const sample = example("capabilities.response.json");
  const without_server_id = { ...sample };
  delete without_server_id.server_id;
  const error = expectThrow(() => decodeCapabilities(without_server_id));
  assert.match(error.message, /Capabilities\.server_id/);
});

check("unknown_optional_field_allowed", () => {
  const sample = example("error.response.json");
  const envelope = decodeErrorEnvelope({
    error: { ...sample.error, vault_id: undefined, details: undefined },
  });
  assert.equal(envelope.error.vault_id, undefined);
});

check("client_connection_page_preserves_unknown_history_and_rejects_false_online", () => {
  const row = { connection_id: "ab".repeat(16), device_label: "<script>设备</script>", client_kind: "unknown", created_at: "1000", state: "unknown", current: false };
  const sample = { observed_at: "2000", connections: [row] };
  const result = decodeClientConnectionPage(sample);
  assert.equal(result.connections[0].last_seen_at, undefined);
  assert.equal(result.connections[0].device_label, row.device_label);
  assert.equal(result.next_cursor, undefined);
  for (const invalid of [{ state: "online" }, { client_kind: "guessed-device" }, { last_seen_at: 2000 }, { created_at: "-1" }, { current: "true" }]) {
    assert.throws(() => decodeClientConnectionPage({ ...sample, connections: [{ ...row, ...invalid }] }));
  }
});

if (failures.length > 0) {
  console.log(`${failures.length}/${checks} 项失败`);
  process.exit(1);
}
console.log(`全部 ${checks} 项通过`);
