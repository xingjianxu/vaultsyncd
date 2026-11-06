-- 旧设备重建独立于首次导入。日志保留旧基线/事件及固定快照；不搬走或删除本地内容与队列。
CREATE TABLE binding_resyncs (
    resync_id INTEGER PRIMARY KEY AUTOINCREMENT,
    binding_id BLOB NOT NULL REFERENCES bindings(id) ON DELETE CASCADE,
    journal TEXT NOT NULL CHECK (json_valid(journal)),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    complete INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0, 1))
);
CREATE UNIQUE INDEX one_open_resync ON binding_resyncs(binding_id) WHERE complete = 0;

-- 不随 pending_ops 出队而丢失旧操作身份，尤其禁止旧 Delete 在新 epoch 重新解释。
CREATE TABLE resync_quarantined_ops (
    op_id BLOB PRIMARY KEY CHECK (length(op_id) = 16),
    resync_id INTEGER NOT NULL REFERENCES binding_resyncs(resync_id) ON DELETE CASCADE
);
