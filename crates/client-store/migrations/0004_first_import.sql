-- S058：首次绑定的固定快照、导入清单、执行检查点与临时身份映射。
-- 已发布迁移不改写。JSON 为 agent-core 内部日志，不是 wire DTO；原始快照使用生成 DTO。
CREATE TABLE binding_imports (
    binding_id BLOB NOT NULL PRIMARY KEY REFERENCES bindings(id) ON DELETE CASCADE,
    journal TEXT NOT NULL CHECK (json_valid(journal)),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    complete INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0, 1))
) STRICT;

CREATE TABLE import_id_map (
    binding_id BLOB NOT NULL REFERENCES binding_imports(binding_id) ON DELETE CASCADE,
    temporary_id BLOB NOT NULL CHECK (length(temporary_id) = 16),
    entry_id BLOB NOT NULL CHECK (length(entry_id) = 16),
    op_id BLOB NOT NULL CHECK (length(op_id) = 16),
    -- 保留完整 ACK（含冲突身份）；不以 pending/finalize 充当 committed。
    result_json TEXT NOT NULL CHECK (json_valid(result_json)),
    PRIMARY KEY (binding_id, temporary_id),
    UNIQUE (binding_id, op_id)
) STRICT;
