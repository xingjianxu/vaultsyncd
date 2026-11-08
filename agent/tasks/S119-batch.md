# S119 Mantine 迁移生命周期

项目基线 `e5981066faa3cf38be99dcabb59006712d668013`，原生工具创建后跨设备移动并 repair 至 `/wrd/prj/vaultsyncd/.worktrees/S119-mantine`，分支 `feat/S119-mantine`。三个 subagent 依次完成基础界面、业务页面和验证子任务，主 agent 负责计划、ADR-018/接口、集成与最终交接；只执行父卡 S119。主工作区 skills-lock.json 及未跟踪 Mantine skills 不纳入本任务。

基线 frozen pnpm install、Web test/build通过；最终验证、四业务 run_id、CSP设计、一次Modal焦点问题及复验结果见 [父卡](S119.md) 和三个子卡。日志与截图保留在永久主仓库 `artifacts/S119/`、`artifacts/S119-*.log` 及对应 `/data/vs-s0xx-...`，不随源码树清理删除。不修改主机配置或部署版本，线上 generation247 保持。

提交及清理：待验收交接完成后使用 git-commit skill 做中文 summary/body 本地提交，随后 fast-forward 合入 master；确认任务树干净且提交均已包含后 remove 工作树、删除本地任务分支。实际hash与执行结果将在完成后回填；不推送远程、不改写历史。
