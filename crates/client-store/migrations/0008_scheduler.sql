-- S062: 调度状态与队列同寿命。取消/退避不删除原操作、依赖、续传或固定版本。
CREATE TABLE operation_schedule (
    op_id BLOB PRIMARY KEY REFERENCES pending_ops(op_id) ON DELETE CASCADE,
    priority INTEGER NOT NULL CHECK(priority BETWEEN 0 AND 3),
    due_at INTEGER NOT NULL DEFAULT 0 CHECK(due_at >= 0),
    cancelled INTEGER NOT NULL DEFAULT 0 CHECK(cancelled IN (0,1)),
    halted INTEGER NOT NULL DEFAULT 0 CHECK(halted IN (0,1)),
    offline INTEGER NOT NULL DEFAULT 0 CHECK(offline IN (0,1))
);
