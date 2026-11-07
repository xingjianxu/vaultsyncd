# S111 单卡批次

范围：用户确认全面移除 Linux ARM64，仅执行 S111-a/b 两个顺序分段，其他 blocked 卡不重启。

主树上一轮两处文档修改先以 `b585fc4` 单独保存。原生工具建立 detached worktree，跨设备迁移（git worktree move 报 EXDEV 后用 mv 与 repair）到 `/wrd/prj/vaultsyncd/.worktrees/s111`，分支 `build/S111-drop-linux-arm64`，基线 b585fc4。实现提交 `9e62b03`；验收见 [S111](S111.md)。验证摘要已保存到主树忽略的 artifacts/S111，不把控制器 mock、Nix 求值或 plan 当作原生运行/完整发行证据。

验收交接提交 `ecb21dc` 已 ff-only 合入 master（集成后 HEAD 即 `ecb21dc`）；`git merge-base --is-ancestor` 确认实现 `9e62b03` 与交接均已包含。确认任务树 porcelain 为空且从主树执行后，`git worktree remove /wrd/prj/vaultsyncd/.worktrees/s111` 与 `git branch -d build/S111-drop-linux-arm64` 均返回0，未强制清理；只余 master 主工作树。验证摘要在主树 artifacts/S111，未删除历史发行归档或外部卷。下一 ready 无，S101 hosted 资源恢复后再续原链。未推送远端。
