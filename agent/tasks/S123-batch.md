# S123 部署生命周期

任务树 `/wrd/prj/vaultsyncd/.worktrees/S123-local-deploy`，分支 `chore/S123-local-deploy`，基线 `6395dfa`（开卡提交；部署源同此提交）。主树既有 `skills-lock.json` 与未跟踪 Mantine skills 未动。

主机侧：一键脚本在 `/data/vaultsyncd-deploy/local-20261108T153312-2697471/host` 临时树只改 pin，提交 `207099c690eedf4d991efe9bd519a63d2e4b25c3` 已 ff 合 `/etc/nixos` main；`result.json` 记录 `passed=true`、`cleaned=true`，临时主机树与分支已删除，main 干净、锁未变。

运行/开机 generation250，恢复入口 generation249；包内 Rust 组件重建 67/67 测试通过，Web 组件复用在 S122 已验收的同一 store path。部署契约 6/6、pure 构建、manifest/锁、dry/test/HTTPS/IPC/归属/switch 与切换后一致性全部通过。生产 Chromium 七项覆盖六路由（含新增 `#/files`）、登录安全 cookie、Root 健康、文件浏览器只读浏览与属性弹窗、SSE 与敏感路径 404；10 项服务、3 应用、SSH 与主机锁保持。文件浏览器请求审计无内容/上传/操作写入，仅生成一条元数据快照。

证据：`/data/vaultsyncd-deploy/local-20261108T153312-2697471/`、`/data/vaultsyncd-deploy/S123-20261108-local/`，脚本与结果另复制到 `artifacts/S123/`（Git 外）。未实际激活旧 generation 演练回退。原 S101/S106/S107/S108/T02 阻塞与首发门禁不变，下一 ready 无。

整合与清理：交接提交 `e780f39` 已 ff 合 master（整合点同 SHA，无额外 merge 提交），开卡提交 `6395dfa` 与部署源一致。确认任务树无未提交修改且全部提交已包含后，从主树执行 `git worktree remove /wrd/prj/vaultsyncd/.worktrees/S123-local-deploy` 与 `git branch -d chore/S123-local-deploy`，均 exit0；`git worktree list` 仅余主树，主树既有 skills 修改完整保留。无推送、无改写历史。
