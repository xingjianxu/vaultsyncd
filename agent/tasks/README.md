# 任务交接与历史记录

S150 批次：文件右键版本历史弹窗实现/验收`65d56ae14b142e2594167db1db91e993cf44c479`已ff-only合master，整合点同该提交，无额外merge commit。确认两树干净、全部任务提交为master祖先且当前位于主树后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S150-file-history`与`git branch -d feat/S150-file-history`均成功。S134树保持，永久证据`/data/vaultsyncd-temp/S150-history-20261109T181921-782538/evidence/`及原文件浏览器回归`/data/vaultsyncd-temp/vs-s134-20261109T181740-780344/`保留。未部署生产，下一ready无，原5张blocked首发门禁保持；详见[S150](S150.md)。

S148 批次：实现`3885ab4b4707bd43c1a5dd20428dd66c04a7baac`、交接`4e0b07423e5b42b29a2089338bf96d92aba9b006`已ff-only合master，整合点同交接提交。核对任务树干净、任务提交全属master且当前位于主树后，成功执行`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S148-web-auto-connect`及`git branch -d feat/S148-web-auto-connect`。S134/S147工作树保留；主树flake.lock既有改动未暂存、合并前后哈希一致。真实Chromium及独立btrfs服务验收证据在`/data/vaultsyncd-temp/S148-auth-20261109T154337-494556/`，详见[S148](S148.md)。本卡未部署，下一ready无，原5张blocked首发门禁保持。

当前状态与 ready 入口见 [进度](../progress.md)；任务目标、依赖和验收标准见 [实施手册](../implementation-plan.md)。按任务读取 `Sxxx.md`，接口和运维规则继续在 `docs/` 维护。

2026-11-08（S126）删除了 126 份已完成任务的拆卡、排障及批次过程文件，保留全部父卡的结果、限制和证据入口。5 张 blocked 父卡及其恢复条件、`S101-a.md`、`S101-b.md`、`S104-S108.md` 保持原文；当前恢复顺序见进度页。

归档固定版本：`1fb58955152c4f8f75ae4e8c8933f1d3d81db76a`。已删除文件的完整原文均在此提交，可先列文件名，再按原路径读取，例如：

```bash
git ls-tree -r --name-only 1fb58955152c4f8f75ae4e8c8933f1d3d81db76a -- agent/tasks/
git show 1fb58955152c4f8f75ae4e8c8933f1d3d81db76a:agent/tasks/S125-batch.md
```

旧父卡自身已有的“完整交接”固定提交继续有效。历史的环境、下一 ready、部署状态和“未部署”描述只代表当时结论；当前部署及最近一次生产浏览器与资产验收见 [S145](S145.md)（generation261）。

已完成任务的过程文件只有在结论、验证、限制、生命周期都已有父卡或规范入口，且原文已提交可恢复时才可删除；同步修正引用。未完成任务保留续接点和恢复条件，不能只留 Git 索引。日志、截图、发行产物及 VM 等 Git 外证据须独立保存，本次没有删除或重新验证这些证据。

S128 批次：项目实现 `f7609ad`，主机 pin `6300214`，generation253；交接 `9d58fd7745c7a052ed0a44663fa4828dff9c0c55` 已 ff-only 合 master（整合点同该提交）；确认全部任务提交已合入、工作树干净且非当前目录后，worktree remove 与 branch -d 均成功。永久 `artifacts/S128/` 已保留日志、脚本和生产截图；生命周期详情见 S128。

S129 批次：Explorer 集成实现 `3357d89`、交接 `bbddb133c2046d4d4c08b71510e473eec58c0065` 已 ff-only 合 master，整合点同交接提交；检查祖先关系及任务树干净后，已从主树成功 remove `/wrd/prj/vaultsyncd/.worktrees/S129-explorer-status` 并 branch -d `fix/S129-explorer-status`。永久 `artifacts/S129/` 保留五阶段真实 Explorer 截图/属性、Windows74项与管理回归、失败修正及清理记录。未打包或部署用户客户端；生产服务仍为 S128 generation253，下一 ready 无。

S136 批次：对象隔离修复 `f0e0103`、Web 文件操作 `0346ec1` 已 ff-only 合 master（无额外 merge commit）；确认任务树干净、提交已合入且非当前工作树后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S136-web-file-actions` 与 `git branch -d feat/S136-web-file-actions` 均成功。永久 `artifacts/S136/` 保留 Rust/Web/浏览器日志与证据；真实浏览器运行目录在 `/data/vs-s136-*`。S136 未做生产部署，下一次部署会迁移 schema 14→15。S134 任务树仍按该卡交接保留。

S136-d 批次：合并后审查修复 `c2d06b0` 已 ff-only 合 master；确认任务树干净、提交已合入且非当前工作树后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S136-followups` 与 `git branch -d fix/S136-object-layout-followups` 均成功。1 GiB btrfs loop 卷（`/data/vaultsyncd-lab/runs`）用于实跑 ignored 的离线恢复原生测试，结束后已卸载、删除镜像，`/data/vaultsyncd-lab/runs` 不再存在。证据：`artifacts/S136-followups/` 与当时任务树 `artifacts/S136-followups/`（已随工作树移除，摘要见 S136-d）。


S137 批次：实现与验收`999bbee2dc941ecf3070aef8b9663e25a7032bfe`已ff合master；确认任务树干净、祖先关系和非当前目录后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S137-web-rename`、`git branch -d feat/S137-web-rename`均成功。原生创建的跨盘临时副本经925个基线文件与无额外文件核对后也已清理。所有日志保留永久`artifacts/S137/`，真实最终浏览器`/data/vs-s137-20261108T223612-3435202`，主树旧S134任务树保持。下一卡S138部署本机。

S138 批次：固定源5689be16b8bc389ae3781e96c2ff734f7d6761ae已部署generation258，主机pin9930ec77046abed5744a35f1b672593270bc485e已ff main且临时主机树/分支清理。项目交接a89debb已ff master（整合点同提交）；确认干净与祖先关系后，从主树`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S138-deploy-verification`及`git branch -d chore/S138-deploy-verification`均成功。原生临时副本932基线文件核对后清理。永久`artifacts/S138/`已保留验证脚本/全部日志/生产截图/异盘verified完整归档与收据，/data/vaultsyncd-deploy/local-20261108T224057-3442489保留旧system/boot GC roots。当前无ready，5张blocked恢复条件保持。

S145 批次：统一登录实现/部署源 `6e58bc26fcef09faf675229889693b593f6e9c4f`、交接 `c9b092d34f650da9ccc869fca5ffcb349adefe71` 已ff-only合master，整合点同交接提交，无额外merge commit。生产generation261、主机pin `18a7df5e9ef087ba7bc65d83f1c98b919ebbc5eb`，部署主机临时树/分支自动清理。确认任务树干净且全部提交为master祖先后，从主树成功 `git worktree remove /wrd/prj/vaultsyncd/.worktrees/S145-unified-login`、`git branch -d feat/S145-unified-login`；永久 `artifacts/S145/` 已保存脚本、日志、基线/结果和生产/离线截图，独立btrfs验收目录与旧system GC roots保留；旧S134树未触碰。验收/回退入口见S145；无ready，原5张blocked门禁保持。


S146 批次：实现 `f67b8c1501d2eb928eabe8bf513f7ae75f7c3f67`、交接 `a29ee681f8ac1f7e44d88bd30e1bf4e50cd90320` 已ff-only合master，整合点同交接提交。主机 `13f0dfc` 已ff合main，运行/开机generation262，正式服务pin保持S145。确认两任务树均无未提交修改、全部任务提交属于目标分支后，已分别从主树执行 `git worktree remove /wrd/prj/vaultsyncd/.worktrees/S146-host`、`git branch -d chore/S146-retire-trial`，以及 `git worktree remove /home/xingjian/.codex/worktrees/49b3/vaultsyncd`、`git branch -d chore/S146-temp-cleanup`；本次项目树由原生工具创建，使用其返回路径。旧S134树不动。原始清单、脚本、容量、s075私密归档、验证和主机日志、旧generation261 GC roots独立保存在 `/data/vaultsyncd-temp/S146-cleanup/`，清树前已核对脚本副本一致。36项清理完成，原始du约10.3GiB，不等同df独占空间回收；正式数据、整个win-lab、历史证据/备份保留。无ready，原5张blocked首发门禁保持。


S147 批次：实现 `5403c3c4d08c9affb9cd387015ccfdca17109ca0`、整合S148 `1e9367f`、交接 `de1f96ec6f9cdcf7016ced835e4403aebd118e9d` 已ff-only合master（整合点同交接提交）。schema18、Web客户端记录及共同认证/自动连接浏览器复验通过。确认任务树干净、全部任务提交为master祖先且当前在主树后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/S147-client-connections` 与 `git branch -d feat/S147-client-connections` 均成功。S134树及主树flake.lock未提交修改保持，合并前后lock哈希一致；源码外证据、缓存与限制见S147，未部署，原5卡blocked保持。

S149 批次：部署源`d9cfee975ba2656feffd560b8cf0be46ba516442`（含S147/S148），登记`28860be`与验收交接`98de5a5ca134e9bc629b8f6cde804a11656522c4`已ff-only合master，整合点同98de5a5。运行/开机generation263、schema18；精确主机构建pin`f5153d8`由`fb293d5`合main，仅整合pin并保留main未激活的全量锁更新。确认两树干净、全部提交为目标分支祖先且当前在主树后，成功清理项目`/wrd/prj/vaultsyncd/.worktrees/S149-deploy`及`chore/S149-deploy`、主机`/wrd/prj/vaultsyncd/.worktrees/S149-host`及`chore/S149-host`。S134与原失败部署树保持。原始生产验收、截图、日志及旧system GC roots在`/data/vaultsyncd-temp/S149-deploy-20261109T095905Z/evidence/`，完整schema17备份在该run的backups及异盘`artifacts/S149/backups/`，均独立保留；隔离完整恢复已通过，未实际回退生产。详见S149与运维文档。无ready，原5卡blocked不变。
