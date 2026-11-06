-- 0008_conflicts.sql — 冲突副本映射（S052 续接；design §7.4、§10.1—10.2）。
--
-- 过期基线上的迟到内容不会被丢弃，也不会覆盖服务器 head：核心把它提交成一个**独立的冲突条目**
-- （`sibling_entry_id`），这里记录“哪个源条目在哪个 OpId 下产生了哪个副本、路径与原因”。
--
--   * 主键 `(root_id, device_id, op_id)`：同一设备/会话的同一 OpId 只产生一个副本；重试拿到同一身份；
--   * `sibling_entry_id` 在 Root 内唯一：一个条目只能是一份冲突副本；
--   * 源条目外键指向 entries 的复合键，副本与源必须同 Root。
--
-- 副本的路径、内容版本与 revision 都在 `entries`/`versions`/`changes` 里，本表只存映射与解释信息，
-- 不复制内容。旧设备据此知道“我的迟到分支去了哪里”（design §9.4）。

CREATE TABLE conflicts (
    root_id          BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    device_id        BLOB NOT NULL CHECK (length(device_id) = 16),
    op_id            BLOB NOT NULL CHECK (length(op_id) = 16),
    source_entry_id  BLOB NOT NULL CHECK (length(source_entry_id) = 16),
    sibling_entry_id BLOB NOT NULL CHECK (length(sibling_entry_id) = 16),
    path             TEXT NOT NULL CHECK (length(path) > 0),
    reason           TEXT NOT NULL CHECK (length(reason) > 0),
    created_at       INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (root_id, device_id, op_id),
    FOREIGN KEY (root_id, device_id, op_id) REFERENCES operations (root_id, device_id, op_id) ON DELETE RESTRICT,
    UNIQUE (root_id, sibling_entry_id),
    FOREIGN KEY (root_id, source_entry_id) REFERENCES entries (root_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (root_id, sibling_entry_id) REFERENCES entries (root_id, id) ON DELETE RESTRICT
) STRICT;

CREATE INDEX conflicts_by_source ON conflicts (root_id, source_entry_id, created_at);
