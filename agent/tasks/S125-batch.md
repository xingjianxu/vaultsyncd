# S125 部署生命周期

任务树 `/wrd/prj/vaultsyncd/.worktrees/S125-local-deploy`，分支 `chore/S125-local-deploy`，基线 `30daa88`（开卡提交，同时是部署源）。主树既有 `skills-lock.json` 与未跟踪 Mantine skills 未动。

主机侧：一键脚本在 `/data/vaultsyncd-deploy/local-20261108T160744-2745069/host` 临时树只改 pin，提交 `dc7dd421dadbcf0cba11ab59f4801aefe7d4e3c5` 已 ff 合 `/etc/nixos` main；`result.json` 记录 `passed=true`、`cleaned=true`，临时主机树与分支已删除，main 干净、锁未变。

运行/开机 generation251，恢复入口 generation250。按用户要求未做部署后验证（无 Chromium、无资产哈希、无服务 PID/应用/SSH 基线复核），只有部署脚本自身门禁：契约 6/6、pure 构建、manifest/锁、dry/test/HTTPS/IPC/归属/switch 与切换后一致性。

证据：`/data/vaultsyncd-deploy/local-20261108T160744-2745069/`、`/data/vaultsyncd-deploy/S125-20261108-local/`，筛选副本在 `artifacts/S125/`（Git 外）。原 S101/S106/S107/S108/T02 阻塞与首发门禁不变，下一 ready 无。
