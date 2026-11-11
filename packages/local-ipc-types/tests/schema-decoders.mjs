// 生成代码的运行时检查（`pnpm --filter @vaultsyncd/local-ipc-types test`）。
//
// TypeScript 只做类型检查；这里的检查证明**运行期**也不会把非法 wire 数据当成功：未知命令、
// 未知应答种类、缺失的必填字段与不带宽度的协议版本都抛错。样本直接取 `api/examples/`，
// 与 Rust 侧 `crates/local-ipc/tests/contract.rs` 用同一份数据：两侧解码结果都必须等于样本
// 本身，所以“Rust 与 TS 样本一致”不是各写一份断言，而是同一份文件通过两套生成代码。
//
// 直接以 .mjs 导入 .ts 源码：Node 24 会剥离类型注解，生成文件只用可擦除语法（interface、
// type、as const），因此不需要额外的构建步骤。

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  FRAME_HEADER_BYTES,
  MAX_FRAME_BYTES,
  PROTOCOL_MAJOR,
  PROTOCOL_MINOR,
  decodeCommandName,
  decodeManagementResult,
  decodeRequest,
  decodeResponse,
  decodeResponseKind,
} from "../src/generated/types.ts";

const examples = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "api", "examples");

/** 读取一个契约样本。 */
function example(name) {
  return JSON.parse(readFileSync(join(examples, name), "utf8"));
}

/** 解码后再编码：`undefined` 的可选字段不会出现在 JSON 里，与 Rust 侧 `to_value` 比对同一意图。 */
function reencode(decoded) {
  return JSON.parse(JSON.stringify(decoded));
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

check("frame_limits_come_from_contract", () => {
  assert.equal(FRAME_HEADER_BYTES, 4);
  assert.ok(MAX_FRAME_BYTES >= 64 && MAX_FRAME_BYTES <= 64 * 1024 * 1024);
  const contract = JSON.parse(readFileSync(join(examples, "..", "local-ipc.schema.json"), "utf8"));
  assert.equal(PROTOCOL_MAJOR, contract["x-vaultsyncd"].protocol.major);
  assert.equal(PROTOCOL_MINOR, contract["x-vaultsyncd"].protocol.minor);
});

check("file_projection_preserves_pending_and_rejects_lossy_flags_or_missing_dimensions", () => {
  const sample=example("local-ipc.files.json");
  assert.deepEqual(reencode(decodeResponse(sample)),sample);
  const row=sample.result.management.files[0];
  assert.ok(row.pending && row.dirty && !row.synchronized && !row.release_allowed);
  row.busy="false"; expectThrow(()=>decodeResponse(sample));
  row.busy=false; delete row.availability; expectThrow(()=>decodeResponse(sample));
});

check("request_example_decodes_to_sample", () => {
  const sample = example("local-ipc.request.json");
  assert.deepEqual(reencode(decodeRequest(sample)), sample);
});

check("all_management_examples_decode_without_losing_parameters", () => {
  const samples = example("local-ipc.management.json");
  const commands = new Set();
  for (const sample of samples) {
    const decoded = decodeRequest(sample);
    assert.deepEqual(reencode(decoded), sample);
    commands.add(decoded.command.name);
  }
  assert.equal(commands.size, 29);
  assert.ok(commands.has("list_activity"));
});

check("multiple_connections_and_binding_server_identity_round_trip", () => {
  const server = index => ({ server_id: String(index).repeat(32), server_url: `https://server${index}.invalid`, authenticated: index === 1 });
  const sample = { connections: [server(1), server(2)], bindings: [{
    binding_id: "a".repeat(32), server_id: server(2).server_id, vault_id: "b".repeat(32),
    local_path: "C:/sync", cache_path: "C:/cache", mode: "on_demand", paused: false, detached: false,
  }] };
  assert.deepEqual(reencode(decodeManagementResult(sample)), sample);
  assert.deepEqual(reencode(decodeManagementResult({ connections: [] })), { connections: [] });
  expectThrow(() => decodeManagementResult({ connections: [server(1), { ...server(2), authenticated: "no" }] }));
});

check("notice_server_scope_is_optional_and_validated", () => {
  const notice = { key: "legacy", cause: "CONFLICT_COPY", code: "NEEDS_RECOVERY", state: "active", generation: "3", critical: true, claimed: true };
  for (const value of [notice, { ...notice, server_id: "1".repeat(32), vault_id: "2".repeat(32) }]) {
    assert.deepEqual(reencode(decodeManagementResult({ notices: [value] })), { notices: [value] });
  }
  expectThrow(() => decodeManagementResult({ notices: [{ ...notice, server_id: 12 }] }));
});

check("response_ok_example_decodes_to_sample", () => {
  const sample = example("local-ipc.response.json");
  assert.deepEqual(reencode(decodeResponse(sample)), sample);
});

check("real_status_preserves_decimal_counts_and_legacy_response", () => {
  const sample = example("local-ipc.status.json");
  const decoded = decodeResponse(sample);
  assert.deepEqual(reencode(decoded), sample);
  assert.equal(decoded.result.bindings[0].dirty, "9007199254740993");
  assert.equal(decoded.result.bindings[0].synchronized, false);
  assert.equal(decodeResponse(example("local-ipc.response.json")).result.bindings, undefined);
  sample.result.bindings[0].dirty = 9007199254740993;
  expectThrow(() => decodeResponse(sample));
});

check("response_error_example_decodes_to_sample", () => {
  const sample = example("local-ipc.error.json");
  assert.deepEqual(reencode(decodeResponse(sample)), sample);
});

check("unknown_command_is_rejected", () => {
  const request = example("local-ipc.request.json");
  request.command.name = "delete_everything_unknown";
  expectThrow(() => decodeRequest(request));
  expectThrow(() => decodeCommandName("delete_everything_unknown"));
});

check("unknown_response_kind_is_rejected", () => {
  const response = example("local-ipc.response.json");
  response.kind = "partial";
  expectThrow(() => decodeResponse(response));
  expectThrow(() => decodeResponseKind("partial"));
});

check("missing_required_field_is_rejected", () => {
  const request = example("local-ipc.request.json");
  delete request.request_id;
  expectThrow(() => decodeRequest(request));
});

check("version_numbers_must_be_integers", () => {
  const request = example("local-ipc.request.json");
  request.ipc_version.major = "1";
  expectThrow(() => decodeRequest(request));
});


check("connection_username_is_optional_non_secret_metadata", () => {
  const sample = example("local-ipc.response.json");
  sample.result.management = { connection: { server_id: "76".repeat(16), server_url: "https://sync.example", authenticated: false, username: "用户甲" } };
  assert.equal(decodeResponse(sample).result.management.connection.username, "用户甲");
  delete sample.result.management.connection.username;
  assert.equal(decodeResponse(sample).result.management.connection.username, undefined);
  sample.result.management.connection.username = 42;
  expectThrow(() => decodeResponse(sample));
});

if (failures.length > 0) {
  console.log(`${failures.length}/${checks} 项失败`);
  process.exit(1);
}
console.log(`${checks} 项全部通过`);
