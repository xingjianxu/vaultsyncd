-- 0005_trash.sql — 回收站清单与墓碑（S039）。
--
-- 落 design §10.3：删除先把条目路径与内容引用记进回收站清单（30 天内可恢复），同时写一条墓碑，
-- 让旧设备基线知道“这个条目已经没了”。两张表分开是因为**生命周期不同**：
--
--   * `trash_entries` 是恢复用的清单：原父目录、原路径、删除时的 revision 与当前内容版本都在这里，
--     内容对象在 `expires_at` 之前不得回收；
--   * `tombstones` 是同步协议用的事实：条目身份、路径、删除时间。内容过期被回收后墓碑仍然存在
--     （design §10.3 末条：不能因为内容过期就把旧客户端当首次加入），压缩由同步协议卡另立规则。
--
-- 外键 `(root_id, entry_id) → entries(root_id, id)` 让“墓碑/清单指向别的 Root 的条目”直接失败。条目
-- 只做软删除（`entries.deleted_at`），不硬删行，因此 RESTRICT 不会挡住正常路径。
--
-- 身份列 16 字节 BLOB，与 0002—0004 一致；时间列是 UTC unix 秒。

CREATE TABLE trash_entries (
    entry_id           BLOB NOT NULL PRIMARY KEY CHECK (length(entry_id) = 16),
    root_id            BLOB NOT NULL,
    -- 删除时的父目录：恢复要放回原位，冲突时另名并回报实际路径（S040）。
    parent_id          BLOB NOT NULL CHECK (length(parent_id) = 16),
    -- 删除时的 Root 内相对路径（整棵子树逐条记录，目录也在其中）。
    path               TEXT NOT NULL CHECK (length(path) > 0),
    kind               TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    -- 删除时的条目 revision：证明删的是哪一版。
    revision           INTEGER NOT NULL CHECK (revision >= 0),
    -- 删除时的当前内容版本；目录没有内容版本（与 entries 的同类检查一致）。
    content_version_id BLOB CHECK (
        content_version_id IS NULL OR length(content_version_id) = 16
    ),
    deleted_at         INTEGER NOT NULL CHECK (deleted_at >= 0),
    -- 内容最早可被回收的时间；删除条目在保留期内必须保持可恢复（design §10.3）。
    expires_at         INTEGER NOT NULL CHECK (expires_at >= deleted_at),
    FOREIGN KEY (root_id, entry_id) REFERENCES entries (root_id, id) ON DELETE RESTRICT,
    CHECK (kind <> 'directory' OR content_version_id IS NULL)
) STRICT;

-- 过期扫描与“这个 Root 现在有什么可恢复”的稳定顺序（路径相同不可能，再由身份兜底）。
CREATE INDEX trash_entries_by_expiry ON trash_entries (root_id, expires_at, path, entry_id);

CREATE TABLE tombstones (
    root_id    BLOB NOT NULL,
    entry_id   BLOB NOT NULL CHECK (length(entry_id) = 16),
    path       TEXT NOT NULL CHECK (length(path) > 0),
    kind       TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    deleted_at INTEGER NOT NULL CHECK (deleted_at >= 0),
    -- 与清单同一个保留期；内容回收后墓碑仍要留着（design §10.3 末条）。
    expires_at INTEGER NOT NULL CHECK (expires_at >= deleted_at),
    PRIMARY KEY (root_id, entry_id),
    FOREIGN KEY (root_id, entry_id) REFERENCES entries (root_id, id) ON DELETE RESTRICT
) STRICT;

-- 旧设备按 Root + 时间取“这段时间哪些条目被删了”。
CREATE INDEX tombstones_by_time ON tombstones (root_id, deleted_at, entry_id);
