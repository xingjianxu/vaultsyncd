# S118 最新服务端本机部署生命周期

项目基线 `9da5ee8`，原生工具创建后按仓库约定放在 `.worktrees/S118-local-deploy`，分支 `chore/S118-local-deploy`；主工作区 `skills-lock.json` 与未跟踪 HeroUI skills 未动。主机 `/etc/nixos` 部署前 main 干净，基线 `9cd3956`（其中 `42d99c5` 为用户要求并确认随本次部署激活的 dae 分流规则调整）。

项目侧不改产品代码，只有任务卡、进度、实施计划与部署文档；实现源为 `9da5ee8f3fe3f64cf517ab6f52d6f1969b1dc353`。主机侧脚本验收后自动生成 pin 提交 `b0ad622809158369075b0065d8dd6e1a4c8e4838` 并 ff-only 合入 main；临时主机树 `/data/vaultsyncd-deploy/local-20261108T133746-2533091/host` 与分支 `chore/S113-deploy-local-20261108T133746-2533091` 已由脚本删除（`result.json` 中 `cleaned=true`）。无推送、无依赖锁变更。

运行与开机默认 generation247，system `cwrpmxw805bg9nrn08a54jmny2lxcg4w-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `2vqjynvrjjxcldl9svlka49mrn3rzwk6-vaultsyncd-0.1.1`；上一版本 generation246（`22bvy0jasjkb02vd8j2wm6lcmgn5aci9`）保留为回退入口。

最小验证、记录偏差与限制见 [S118](S118.md)。证据 `/data/vaultsyncd-deploy/local-20261108T133746-2533091/`（result.json、旧 generation、旧 system/boot GC roots、原锁、pin.diff）与 `/data/vaultsyncd-deploy/S118-20261108-local/`（部署日志、基线/结果、浏览器脚本与 JSON、截图、静态文件副本）均留在 Git 外，不随清理删除。

项目侧交接提交在验收后 ff-only 合入 master；确认工作树干净且提交全部在 master 后清理 `.worktrees/S118-local-deploy` 与 `chore/S118-local-deploy`。下一 ready 无，S101、S106、S107、S108、S011 保持原恢复条件。
