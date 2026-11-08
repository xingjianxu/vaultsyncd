-- 0014_entry_modified_at.sql — 条目的服务端最后修改时间（S130，design §7.2、§9.3）。
--
-- Web 文件浏览器要显示“修改日期”。服务端此前不记录任何按条目的时间：`revision` 只是单调计数，
-- `changes.created_at` 是批次时间且没有“哪一批最后动过这个条目”的反向索引，`versions.created_seq`
-- 只覆盖带内容的文件。因此这里给 `entries` 加一列由写路径维护的 `modified_at`：
--
--   * 创建、移动/改名、内容提交（revision 推进）、回收站恢复都在同一个写事务里写入该值，取值就是
--     该批次的服务端时间；软删除同时写 `modified_at`（行不再活跃，值只作证据保留）。
--   * 它不是文件系统 mtime：服务端不以 mtime 决定冲突或保留（design §6、§9.4），这里只记录
--     “服务端在哪个时刻最后一次接受了对该条目的改动”。
--   * 旧库没有历史可回填，已有行保持 NULL（界面显示“—”），首次改动后才有值；不用 migration 时间
--     冒充修改时间。
--
-- 快照条目是创建时的值拷贝，所以 `snapshot_entries` 同样加一列并随清单一起复制：翻页期间 live 行
-- 的后续改动不会改变已发出的快照值（design §9.3 的一致性口径）。

ALTER TABLE entries
    ADD COLUMN modified_at INTEGER CHECK (modified_at IS NULL OR modified_at >= 0);

ALTER TABLE snapshot_entries
    ADD COLUMN modified_at INTEGER CHECK (modified_at IS NULL OR modified_at >= 0);
