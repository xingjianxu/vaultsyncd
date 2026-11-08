# S124 整合与清理

- 任务：文件浏览器左右分栏与 URL 目录定位，仅执行 S124。
- 工作树：`/wrd/prj/vaultsyncd/.worktrees/S124-file-layout`；分支 `feat/S124-file-layout`；基线 `1e2d778`。
- 验收：Web test/build、根 typecheck、Nix Web；真实 btrfs Chromium 20 类流程和两组各10个主题/宽度场景；原五路由50组回归通过。证据与限制见 [交接](S124.md)。
- 产出提交：本轮提交后补记实际 hash。目标 master；整合与清理结果在执行后补记，不提前宣称完成。
- 主树既有 `skills-lock.json` 与未跟踪 Mantine skills 不属于本卡，保留。未部署、未推送，不改变原首发门禁。
