-- S063: 显式释放先持久意图；平台成功、驻留CAS后才移除意图。旧迁移校验和不变。
CREATE TABLE controlled_releases (
    op_id BLOB PRIMARY KEY CHECK(length(op_id)=16),
    binding_id BLOB NOT NULL REFERENCES bindings(id) ON DELETE CASCADE,
    target_path TEXT NOT NULL CHECK(length(target_path)>0),
    intent_id INTEGER UNIQUE REFERENCES local_intents(intent_id) ON DELETE RESTRICT,
    journal TEXT NOT NULL CHECK(json_valid(journal)),
    completed INTEGER NOT NULL DEFAULT 0 CHECK(completed IN (0,1))
) STRICT;
CREATE UNIQUE INDEX controlled_releases_active_target
    ON controlled_releases(binding_id,target_path) WHERE completed=0;
