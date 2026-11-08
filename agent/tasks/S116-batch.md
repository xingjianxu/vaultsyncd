# S116 本机当前版本部署生命周期

项目基线 `f3b8979`，原生工具创建后按仓库约定移到 `.worktrees/S116-local-deploy` 并 repair，分支 `chore/S116-local-deploy`。主工作区 `skills-lock.json` 与未跟踪 HeroUI skills 未动。主机 `/etc/nixos` main 部署前干净，基线 `32bfeb5`；部署脚本自行在 Git 外创建并清理临时主机树。

项目侧不产生实现提交（不改产品代码/协议/数据库），只有交接与文档。主机侧脚本在验收后自动生成 pin 提交 `15cfc075031ba6d9fab4bf040e1adbe5d68be939` 并 ff-only 合入 `/etc/nixos` main；临时主机树 `/data/vaultsyncd-deploy/local-20261108T124809-2457084/host` 与分支 `chore/S113-deploy-local-20261108T124809-2457084` 已由脚本删除（`result.json` 中 `cleaned=true`）。无推送、无依赖锁变更。

运行与开机默认 generation246，system `22bvy0jasjkb02vd8j2wm6lcmgn5aci9-nixos-system-wks-lab-26.11.20261102.7356cc7`，包 `yd63mh9prpasa8f8647g076mx2m2wjdv-vaultsyncd-0.1.1`，固定源 `f3b897983087af1b6d075d89000c0a5ebc235aeb`。

最小验证、证据与恢复入口见 [S116](S116.md)。证据 `/data/vaultsyncd-deploy/local-20261108T124809-2457084/`（result.json、旧 generation、旧 system/boot GC roots、原锁、pin.diff）与 `/data/vaultsyncd-deploy/S116-20261108-local/`（部署日志、健康前后、浏览器脚本/日志/截图）均留在 Git 外，不随清理删除。

项目侧交接提交在验收后 ff-only 合入 master；确认工作树干净且提交全部在 master 后清理 `.worktrees/S116-local-deploy` 与 `chore/S116-local-deploy`。下一 ready 无，S101、S106、S107、S108、S011 保持原恢复条件。
