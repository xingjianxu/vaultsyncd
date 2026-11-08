# S124 整合与清理

- 任务：文件浏览器左右分栏与 URL 目录定位，仅执行 S124。
- 工作树：`/wrd/prj/vaultsyncd/.worktrees/S124-file-layout`；分支 `feat/S124-file-layout`；基线 `1e2d778`。
- 验收：Web test/build、根 typecheck、Nix Web；真实 btrfs Chromium 20 类流程和两组各10个主题/宽度场景；原五路由50组回归通过。证据与限制见 [交接](S124.md)。
- 产出/整合提交：`986c35c7d7a1c001d3ca98e708ddefc27f4deeaf`（`feat(web): 重构文件浏览器分栏与目录深链`），已从 `1e2d778` ff-only 合入 master，没有额外 merge commit。
- 清理：确认 master 已包含全部任务提交、任务树状态为空且当前目录为主树后，执行 `git worktree remove /wrd/prj/vaultsyncd/.worktrees/S124-file-layout`、`git branch -d feat/S124-file-layout`，均 exit0；最终 worktree 列表只保留主树。
- 主树既有 `skills-lock.json` 与未跟踪 Mantine skills 不属于本卡，保留。未部署、未推送，不改变原首发门禁。
