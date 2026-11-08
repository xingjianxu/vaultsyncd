# S120 Mantine 版最新服务端本机部署生命周期

项目基线 `b5e96f2`，原生工具创建后按仓库约定放在 `.worktrees/S120-local-deploy`，分支 `chore/S120-local-deploy`；主工作区 `skills-lock.json` 与未跟踪 Mantine skills 未动。主机 `/etc/nixos` main 部署前后干净，基线 `b0ad622`（S118 的 pin）。

项目侧不改产品代码，只有任务卡、进度、实施计划与部署文档；实现源 `b5e96f23268ef9dd0a34574e81bcb87718d9a552`（S119 实现提交 `b16d9a1`）。主机侧脚本验收后自动生成 pin 提交 `0073fd638392658eb56ef01cf30ceadc9229631a` 并 ff-only 合入 main；临时主机树 `/data/vaultsyncd-deploy/local-20261108T142445-2602881/host` 与分支 `chore/S113-deploy-local-20261108T142445-2602881` 已由脚本删除（`result.json` 中 `cleaned=true`）。无推送、无依赖锁变更。

运行与开机默认 generation248，system `i1ydyql7wcb0gs8q5dr1xxygm4zj18h6-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `9h1r3jvn1r1nsnrdal2h169n4h5w9hdm-vaultsyncd-0.1.1`；上一版本 generation247（`cwrpmxw805bg9nrn08a54jmny2lxcg4w`）保留为回退入口。

最小验证、证据与限制见 [S120](S120.md)。证据 `/data/vaultsyncd-deploy/local-20261108T142445-2602881/`（result.json、旧 generation、旧 system/boot GC roots、原锁、pin.diff）与 `/data/vaultsyncd-deploy/S120-20261108-local/`（部署日志、基线/结果、浏览器脚本与 JSON、截图、响应头与静态文件副本）均留在 Git 外，不随清理删除。

项目侧交接提交在验收后 ff-only 合入 master；确认工作树干净且提交全部在 master 后清理 `.worktrees/S120-local-deploy` 与 `chore/S120-local-deploy`。下一 ready 无，S101、S106、S107、S108、S011 保持原恢复条件。
