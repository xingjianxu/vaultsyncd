# S115 整合与隔离生命周期

本轮仅执行父卡S115；a—d分别负责基础壳、账号/Root、恢复、备份/验收。最多3个subagent并行，公共组件/依赖锁由单一代理维护，主代理集成、CSP修复、文档与验证。

基线：`5c5c797`；任务树 `/wrd/prj/vaultsyncd/.worktrees/S115-web-heroui`，分支 `feat/S115-web-heroui`。基线与最终验收均通过，详细结果见[S115](S115.md)。UI截图及构建/浏览器日志已复制到主仓库忽略目录 `artifacts/S115`；真实服务验收在独立 `/data/vs-s0xx-*` run_id 内，未接触正式数据。

实现提交、master整合与自动清理将在完成后补记；用户主工作区skills安装变更保持，未暂存进入本卡。
