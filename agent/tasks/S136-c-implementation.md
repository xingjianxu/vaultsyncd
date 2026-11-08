# S136-c 实现说明（完成）

结论：Root-qualified 新写 + legacy 只读兼容 + schema 15 围栏已落地并通过审查。实现入口：`apps/server/src/object_layout.rs`、`crates/server-core/src/objects/mod.rs::{ObjectLayout::for_root,scoped_relative_object_path,valid_storage_path}`、`crates/server-store/migrations/0015_root_scoped_objects.sql`。完整验证与限制见 [S136-c](S136-c.md)。

- 根因：`objects.storage_path` 全局唯一且身份不可变，而服务当前全 Root 共用 `state_dir/objects/objects/<aa>/<hash>`；仅修改登记字符串无法保证 GC/备份和物理隔离。
- 新对象物理与登记统一为对象区下 `roots/<RootId>/objects/<aa>/<hash>`，仍一 Root 一对象行，不删 UNIQUE、不共享对象行。新增 `0015` 语义兼容围栏：虽不改旧对象行，持久对象位置格式已不兼容旧程序，必须推进 `user_version`/迁移账到 15，使 target14 旧二进制拒绝写入。沿用数据库现有迁移前备份、失败闭锁与恢复流程，不做不可恢复对象搬迁。
- 旧对象不做原地迁移。布局只按已授权 Root 的数据库对象行显式允许旧路径；不按磁盘存在性跨 Root fallback。已有旧对象同摘要的新写/重试复用该 Root 的不可变旧登记位置；未登记摘要一律写新命名空间。
- 上传、观察捕获、执行器、恢复 replay、下载与版本租约使用同一个按 Root 选择的布局。GC 校验记录路径必须是该 Root 的新规范路径或旧规范路径，仅删除被授权候选的实际记录位置。
- 备份导出/恢复同时接纳匹配对象 Root 的新规范路径与旧规范路径；继续拒绝穿越、错误 Root 前缀、重复物理登记等。
- 回归目标：两 Root 同摘要独立上传/finalize/create，鉴权隔离，一方 GC 不影响另一方历史/当前，旧对象读取/新写/重试，以及新旧备份恢复。
