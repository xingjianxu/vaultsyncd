-- 0007_snapshots.sql — 一致性快照的短期元数据清单（S048，design §9.3）。
--
-- 快照分页不能每页读不断变化的 live `entries`（否则边写边翻页会漏条目或重复条目），所以创建
-- 快照时在同一个写事务里：读 `roots.seq`（high_water_seq）、把当时的活跃条目复制成一份只读清单。
-- 之后的翻页只读这份清单，与翻页期间发生的新增/删除/改名无关。
--
-- 清单是短期数据：`expires_at` 之后不再提供翻页（端点答 410，客户端重启快照），行在下一次创建
-- 快照时被回收。它不是历史：条目的权威来源仍是 `entries`/`versions`，下载内容仍按 VersionId。
--
-- 条目清单列都是创建时的值拷贝，不引用 `entries`（没有外键）：被软删除或改名的条目不能让
-- 快照行变化，也不能因为外键阻止条目后续的物理清理。

CREATE TABLE snapshots (
    id             BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    root_id        BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    owner_id       BLOB NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    -- 创建快照那一刻 Root 的 RootSeq：快照内容恰好是“应用到该 seq 为止”的状态，
    -- 客户端据此从 `changes?after=high_water_seq` 继续追赶。
    high_water_seq INTEGER NOT NULL CHECK (high_water_seq >= 0),
    -- 创建时的 Root epoch，原样回给客户端（epoch 变化时旧快照不应被继续使用）。
    epoch          INTEGER NOT NULL CHECK (epoch >= 0),
    entry_count    INTEGER NOT NULL CHECK (entry_count >= 0),
    created_at     INTEGER NOT NULL CHECK (created_at >= 0),
    expires_at     INTEGER NOT NULL CHECK (expires_at > created_at)
) STRICT;

CREATE INDEX snapshots_by_root ON snapshots (root_id, created_at, id);
CREATE INDEX snapshots_by_expiry ON snapshots (expires_at);

CREATE TABLE snapshot_entries (
    snapshot_id        BLOB NOT NULL REFERENCES snapshots (id) ON DELETE CASCADE,
    -- 稳定顺序：创建时按 EntryId 排序后的 0 起序号；翻页游标就是下一个序号。
    ordinal            INTEGER NOT NULL CHECK (ordinal >= 0),
    entry_id           BLOB NOT NULL CHECK (length(entry_id) = 16),
    -- 合成根条目没有父目录；其余条目必有。
    parent_id          BLOB CHECK (parent_id IS NULL OR length(parent_id) = 16),
    name               TEXT NOT NULL,
    kind               TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    revision           INTEGER NOT NULL CHECK (revision >= 0),
    content_version_id BLOB CHECK (content_version_id IS NULL OR length(content_version_id) = 16),
    -- 当前版本的内容长度与哈希（目录为 NULL）。
    content_size       INTEGER CHECK (content_size IS NULL OR content_size >= 0),
    content_hash       BLOB CHECK (content_hash IS NULL OR length(content_hash) = 32),
    PRIMARY KEY (snapshot_id, ordinal)
) STRICT, WITHOUT ROWID;

-- 事件保留窗口（design §9.4）按批次时间判定：窗口下沿 = 早于截止时间的最新批次的 seq。
-- 这条索引让“找到下沿”是一次索引寻址，不随事件总量扫描。
CREATE INDEX changes_by_time ON changes (root_id, created_at, seq);
