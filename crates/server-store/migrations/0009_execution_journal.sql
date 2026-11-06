-- S052：发布前持久化业务恢复上下文；旧的未关联意图仍失败关闭，不猜关联。
CREATE TABLE execution_journal (
    root_id BLOB NOT NULL,
    device_id BLOB NOT NULL,
    op_id BLOB NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('put', 'namespace', 'delete', 'version_restore', 'conflict', 'trash_restore')),
    context_json TEXT NOT NULL CHECK (json_valid(context_json)),
    PRIMARY KEY (root_id, device_id, op_id),
    FOREIGN KEY (root_id, device_id, op_id) REFERENCES operations(root_id, device_id, op_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE intent_operations (
    intent_id BLOB PRIMARY KEY REFERENCES fs_intents(intent_id) ON DELETE RESTRICT,
    root_id BLOB NOT NULL,
    device_id BLOB NOT NULL,
    op_id BLOB NOT NULL,
    FOREIGN KEY (root_id, device_id, op_id) REFERENCES execution_journal(root_id, device_id, op_id) ON DELETE RESTRICT
) STRICT;
-- 不以路径前缀猜测哪次删除的后代属于一次目录恢复。
CREATE TABLE trash_membership (
    entry_id BLOB PRIMARY KEY REFERENCES trash_entries(entry_id) ON DELETE CASCADE,
    deletion_root BLOB NOT NULL CHECK(length(deletion_root) = 16),
    deletion_id BLOB NOT NULL CHECK(length(deletion_id) = 16)
) STRICT;
