-- 0001—0004 保持校验和；与 local_intents 同事务保存完整重放上下文。
-- 完成记录保留，避免首次导入/事件在平台成功后重放时再覆盖用户新字节。
CREATE TABLE controlled_applies (
    op_id BLOB PRIMARY KEY CHECK (length(op_id) = 16),
    binding_id BLOB NOT NULL REFERENCES bindings(id) ON DELETE CASCADE,
    target_path TEXT NOT NULL CHECK (length(target_path) > 0),
    intent_id INTEGER UNIQUE REFERENCES local_intents(intent_id) ON DELETE RESTRICT,
    journal TEXT NOT NULL CHECK (json_valid(journal)),
    completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
    UNIQUE (binding_id, target_path, op_id)
) STRICT;
CREATE UNIQUE INDEX controlled_applies_active_target
    ON controlled_applies(binding_id, target_path) WHERE completed = 0;
