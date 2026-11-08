# 任务交接与历史记录

当前状态与 ready 入口见 [进度](../progress.md)；任务目标、依赖和验收标准见 [实施手册](../implementation-plan.md)。按任务读取 `Sxxx.md`，接口和运维规则继续在 `docs/` 维护。

2026-11-08（S126）删除了 126 份已完成任务的拆卡、排障及批次过程文件，保留全部父卡的结果、限制和证据入口。5 张 blocked 父卡及其恢复条件、`S101-a.md`、`S101-b.md`、`S104-S108.md` 保持原文；当前恢复顺序见进度页。

归档固定版本：`1fb58955152c4f8f75ae4e8c8933f1d3d81db76a`。已删除文件的完整原文均在此提交，可先列文件名，再按原路径读取，例如：

```bash
git ls-tree -r --name-only 1fb58955152c4f8f75ae4e8c8933f1d3d81db76a -- agent/tasks/
git show 1fb58955152c4f8f75ae4e8c8933f1d3d81db76a:agent/tasks/S125-batch.md
```

旧父卡自身已有的“完整交接”固定提交继续有效。历史的环境、下一 ready、部署状态和“未部署”描述只代表当时结论；当前部署及最近一次生产浏览器与资产验收见 S128（generation253）。

已完成任务的过程文件只有在结论、验证、限制、生命周期都已有父卡或规范入口，且原文已提交可恢复时才可删除；同步修正引用。未完成任务保留续接点和恢复条件，不能只留 Git 索引。日志、截图、发行产物及 VM 等 Git 外证据须独立保存，本次没有删除或重新验证这些证据。

S128 批次：项目实现 `f7609ad`，主机 pin `6300214`，generation253；交接 `9d58fd7745c7a052ed0a44663fa4828dff9c0c55` 已 ff-only 合 master（整合点同该提交）；确认全部任务提交已合入、工作树干净且非当前目录后，worktree remove 与 branch -d 均成功。永久 `artifacts/S128/` 已保留日志、脚本和生产截图；生命周期详情见 S128。

S129 批次：Explorer 集成实现 `3357d89`、交接 `bbddb133c2046d4d4c08b71510e473eec58c0065` 已 ff-only 合 master，整合点同交接提交；检查祖先关系及任务树干净后，已从主树成功 remove `/wrd/prj/vaultsyncd/.worktrees/S129-explorer-status` 并 branch -d `fix/S129-explorer-status`。永久 `artifacts/S129/` 保留五阶段真实 Explorer 截图/属性、Windows74项与管理回归、失败修正及清理记录。未打包或部署用户客户端；生产服务仍为 S128 generation253，下一 ready 无。

S136 批次：对象隔离修复 `f0e0103`、Web 文件操作 `0346ec1` 已 ff-only 合 master（无额外 merge commit）；确认任务树干净、提交已合入且非当前工作树后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S136-web-file-actions` 与 `git branch -d feat/S136-web-file-actions` 均成功。永久 `artifacts/S136/` 保留 Rust/Web/浏览器日志与证据；真实浏览器运行目录在 `/data/vs-s136-*`。S136 未做生产部署，下一次部署会迁移 schema 14→15。S134 任务树仍按该卡交接保留。

S136-d 批次：合并后审查修复 `c2d06b0` 已 ff-only 合 master；确认任务树干净、提交已合入且非当前工作树后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S136-followups` 与 `git branch -d fix/S136-object-layout-followups` 均成功。1 GiB btrfs loop 卷（`/data/vaultsyncd-lab/runs`）用于实跑 ignored 的离线恢复原生测试，结束后已卸载、删除镜像，`/data/vaultsyncd-lab/runs` 不再存在。证据：`artifacts/S136-followups/` 与当时任务树 `artifacts/S136-followups/`（已随工作树移除，摘要见 S136-d）。

