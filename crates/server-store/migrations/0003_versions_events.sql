-- 0003_versions_events.sql — 内容对象、版本历史、RootSeq 事件批次与幂等操作记录（S028）。
--
-- 落 design §7.2 的 objects/versions/changes/operations 四张表。它们互相咬合，必须一起建出：
--
--   * `versions(root_id, object_hash) → objects(root_id, hash)`：版本只能引用**已持久化并校验过**的
--     内容对象（design §11.2 步骤 3—7、I05），不能凭一个哈希就声称内容存在。
--   * `versions(root_id, entry_id) → entries(root_id, id)`：版本与条目同 Root（design §7.2）。
--   * `versions(root_id, created_seq) → changes(root_id, seq)`：版本只能出现在**已记录的同一事务批次**
--     里（design §11.2 步骤 7「变更事件与最终逻辑状态同一数据库事务提交」）。事务回滚时批次与版本
--     一起消失，不存在“有版本却没有事件”的中间状态。
--   * `operations(root_id, root_seq) → changes(root_id, seq)`：committed 操作记录的批次序号必须真实
--     存在；`pending` 与 `rejected` 的该列为 NULL，复合外键对含 NULL 的行不生效。
--
-- `changes` 一行就是一条不可拆分的事务批次（design §7.2：`(root_id, seq)` 唯一，一条记录是一批），
-- 因此分页按行切分永远不会把目录级事务拆成一半。`roots.seq` 仍是 RootSeq 的唯一权威：
-- 追加批次时先在同一事务里推进它，再写批次行。
--
-- STRICT 表；身份列 16 字节 BLOB、内容哈希 32 字节 BLOB（SHA-256），不存十六进制文本，
-- 避免同一值有两种字节表示（crates/domain 的不透明身份，见 ids.rs）。

CREATE TABLE objects (
    root_id      BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    -- 内容地址：SHA-256 原始 32 字节。按 Root 分行，不提供“按哈希问全库是否存在”的接口
    -- （design §7.2：不通过哈希全局泄露存在性）。
    hash         BLOB NOT NULL CHECK (length(hash) = 32),
    size         INTEGER NOT NULL CHECK (size >= 0),
    -- 内部对象存储里的位置。全库唯一：两行指向同一个物理文件时，按 Root 的 GC 会删掉别人的字节。
    storage_path TEXT NOT NULL UNIQUE CHECK (length(storage_path) > 0),
    -- 只有校验通过的对象才会被记录（design §11.2 步骤 3），因此不为空；无需“未校验”状态。
    verified_at  INTEGER NOT NULL CHECK (verified_at >= 0),
    gc_state     TEXT NOT NULL CHECK (gc_state IN ('live', 'collectable')),
    PRIMARY KEY (root_id, hash)
) STRICT;

-- 内容地址证据不可修改：改哈希/长度/路径就是换了一份内容，必须写新对象行。
CREATE TRIGGER objects_content_reference_is_immutable
BEFORE UPDATE OF root_id, hash, size, storage_path, verified_at ON objects
BEGIN
    SELECT RAISE(ABORT, 'objects 的内容引用不可修改；新内容必须写新对象行');
END;

CREATE TABLE changes (
    root_id     BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    -- RootSeq 从 0 起算，批次序号从 1 开始：0 表示“还没有任何批次”。
    seq         INTEGER NOT NULL CHECK (seq > 0),
    tx_id       BLOB NOT NULL CHECK (length(tx_id) = 16),
    -- 一批事件是一个 JSON 数组；空批次没有意义（会白占一个 seq），因此长度大于 "[]"。
    events_json TEXT NOT NULL CHECK (
        json_valid(events_json)
        AND json_type(events_json) = 'array'
        AND length(events_json) > 2
    ),
    created_at  INTEGER NOT NULL CHECK (created_at >= 0),
    PRIMARY KEY (root_id, seq)
) STRICT;

CREATE TABLE versions (
    id            BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    root_id       BLOB NOT NULL,
    entry_id      BLOB NOT NULL,
    object_hash   BLOB NOT NULL CHECK (length(object_hash) = 32),
    size          INTEGER NOT NULL CHECK (size >= 0),
    -- 本版本被记录进哪个 RootSeq 批次；它必须真实存在（下面的外键）。
    created_seq   INTEGER NOT NULL CHECK (created_seq > 0),
    -- NULL 表示这是条目的当前版本；被新版本取代后写入时间，历史行从此只读。
    superseded_at INTEGER CHECK (superseded_at IS NULL OR superseded_at >= 0),
    -- client：客户端提交或首次同步导入；external：服务端观察到的外部变更（design I14 同一条流程）。
    origin        TEXT NOT NULL CHECK (origin IN ('client', 'external')),
    FOREIGN KEY (root_id, entry_id) REFERENCES entries (root_id, id) ON DELETE RESTRICT,
    FOREIGN KEY (root_id, object_hash) REFERENCES objects (root_id, hash) ON DELETE RESTRICT,
    FOREIGN KEY (root_id, created_seq) REFERENCES changes (root_id, seq) ON DELETE RESTRICT
) STRICT;

-- 每个条目至多一个当前版本（superseded_at IS NULL）；历史行可以有任意多条。
CREATE UNIQUE INDEX versions_one_current_per_entry
    ON versions (root_id, entry_id)
    WHERE superseded_at IS NULL;

-- 历史不可变（design I12）：已提交版本的字节引用不再改变，恢复历史要写新版本而不是改旧行。
-- 允许更新的只有 superseded_at —— 它是“是否仍是当前版本”的唯一可变标记。
CREATE TRIGGER versions_history_is_immutable
BEFORE UPDATE OF id, root_id, entry_id, object_hash, size, created_seq, origin ON versions
BEGIN
    SELECT RAISE(ABORT, 'versions 的已提交身份与内容引用不可修改；恢复历史请写新版本');
END;

CREATE TABLE operations (
    root_id        BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    device_id      BLOB NOT NULL CHECK (length(device_id) = 16),
    op_id          BLOB NOT NULL CHECK (length(op_id) = 16),
    -- 规范化请求语义的 SHA-256：同一语义只有一种编码，JSON 键顺序/空白不参与。
    request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
    state          TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rejected')),
    -- 紧凑结果：必须是 JSON object 且不超过 4 KiB，防止把事件或条目清单塞进结果。
    result_json    TEXT CHECK (
        result_json IS NULL
        OR (
            length(result_json) <= 4096
            AND json_valid(result_json)
            AND json_type(result_json) = 'object'
        )
    ),
    -- committed 操作推进过的 RootSeq；用于重试时返回同一个 root_seq（design §15.3）。
    root_seq       INTEGER CHECK (root_seq IS NULL OR root_seq > 0),
    -- 工作目录发布的意图记录位置（design §11.2 步骤 5）；pending 期间写入。
    intent_path    TEXT CHECK (intent_path IS NULL OR length(intent_path) > 0),
    -- 幂等键：同一设备在同一 Root 内的 OpId 唯一（design §7.2）。
    PRIMARY KEY (root_id, device_id, op_id),
    FOREIGN KEY (root_id, root_seq) REFERENCES changes (root_id, seq) ON DELETE RESTRICT,
    -- pending 不带结果，也不占 seq（不把 pending 当 committed）；committed 必须有批次序号；
    -- rejected 的稳定拒绝结果不推进 RootSeq。
    CHECK (
        (state = 'pending' AND result_json IS NULL AND root_seq IS NULL)
        OR (state = 'committed' AND result_json IS NOT NULL AND root_seq IS NOT NULL)
        OR (state = 'rejected' AND result_json IS NOT NULL AND root_seq IS NULL)
    )
) STRICT;
