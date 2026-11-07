# S112 集成与清理

本轮只执行用户新增的S112，未执行其他任务卡。

- 项目基线684b8d3，任务树 `/wrd/prj/vaultsyncd/.worktrees/S112`，分支 `feat/S112-nixos-public-deploy`；原生工具最初创建在home，按用户指定目录移动并repair，未嵌套/重复创建。
- 项目实现4856738已构建并固定进主机配置；验收脚本/文档提交 **c94aca4191bb8392475fc4d099e30e49645b4896**。
- 主机基线93dfcae，实现9c2133f及挂载修正56bd93b已ff-only合入 `/etc/nixos` 的main，两个合并均fast-forward，未生成额外merge commit。准确构建源kdr0v41d…-source与不变主机锁在证据目录，可独立重建。
- 主机任务树 `/wrd/prj/vaultsyncd/.worktrees/S112-host` 确认干净、main包含全部提交后已由非当前树执行git worktree remove，再git branch -d，成功。
- 当前system/default generation244，后端0.1.1，完整实际验证与限制见 [S112](S112.md) 和 [部署文档](../../docs/operations/nixos.md)。证据 `/data/vaultsyncd-deploy/S112-20261107T133454/` 不随worktree清理删除。
- 项目master从684b8d3 **ff-only到c94aca4191bb8392475fc4d099e30e49645b4896**，该提交即本次整合结果（无额外merge commit），已包含4856738与全部验收变更。确认任务树干净、master包含全部任务提交且操作位置在主仓库后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S112`、`git branch -d feat/S112-nixos-public-deploy`均成功；两仓库仅剩主工作区。清理未删除正式数据、旧试用卷、证据或GCroot。
