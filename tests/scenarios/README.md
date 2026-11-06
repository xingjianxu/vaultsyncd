# 数据驱动场景描述（S074 起）

`tests/scenarios/*.json` 只描述“谁在什么时候写入哪份内容”，由明确的跨进程 runner 加载：

- **加载器**：`tests/e2e`（包 `vaultsyncd-e2e`）的 `src/scenario.rs` 在 Linux 宿主与 Windows 客体上做同一套形状/引用/路径/内容校验；`src/run.rs` 在 Windows 客体执行 block 并断言结果。
- **执行入口**：`tests/platform/windows/run-native-e2e.sh`（宿主真实服务端 + 一台 Windows 客体上的三个隔离身份）。
- **数据与判据分离**：JSON 提供客户端、服务端种子内容、路径与每块输入；每类 block 的验收判据固定在 runner 里，避免把断言写成场景文件中的自由文本。

## 文件形状（schema 1）

```json
{
  "schema": 1,
  "id": "s074-three-clients",
  "title": "…",
  "requirements": ["T01", "T02", "T03", "T09", "T11"],
  "clients": [
    { "id": "alpha", "label": "client-alpha", "mode": "full" },
    { "id": "bravo", "label": "client-bravo", "mode": "full" },
    { "id": "charlie", "label": "client-charlie", "mode": "on_demand" }
  ],
  "server_seed": [
    { "path": "server-note.txt", "content": "…" }
  ],
  "blocks": [
    { "id": "T01-import", "kind": "import", "requirements": ["T01"],
      "local": { "alpha": [{ "path": "alpha-only.txt", "content": "…" }] } }
  ]
}
```

约束（加载时强制，未知字段直接拒绝）：

- `schema` 必须为 `1`；`id`/客户端 id 只能是小写字母、数字与 `-`；客户端 id 唯一。
- 路径是相对同步根、以 `/` 分隔的路径：不允许绝对路径、`\`、`:`、空分量、`.`/`..` 或控制字符；文件内容非空且不超过 64 KiB。
- `import`：每个客户端绑定前的本地内容；`shared-import.txt` 必须与服务端种子同文，用于验证“同内容归并”。
- `offline_edit`：至少两个客户端在同一路径的离线修改内容。
- `rename`：`renamer` 与 `placeholder_client` 必须不同；源路径必须来自服务端初始内容，按需端在首次导入后只持有占位。
- `delete_modify`：`deleter` 与 `modifier` 必须不同；路径来自服务端初始内容。
- `lost_response`：put 与 delete 使用不同路径；put 给出初始内容与丢响应那次提交的新内容。

新增同类场景只需增加/复制 JSON 并在 `blocks` 中引用已声明客户端；若需要新动作语义，先扩展 `src/scenario.rs` 与 `src/run.rs` 的判据，再更新本文件。
