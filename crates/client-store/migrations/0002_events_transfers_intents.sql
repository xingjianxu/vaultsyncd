-- 0002_events_transfers_intents.sql — 事件接收、传输进度、本地意图与通知（S031）。
--
-- 落 design §7.3 的 received_changes、transfers、local_intents、notifications 与 §9.3 的
-- 接收/应用游标：客户端在**同一个事务**里写批次、远端状态、应用任务和接收游标；应用游标
-- 只在文件落盘完成后另行推进（I09）。
--
-- 0001_initial.sql 已发布，不得改写；本文件只追加新表与游标列。


-- 接收/应用游标（design §9.3、I09）。两个游标放在绑定行上：游标属于某个绑定，服务器 epoch
-- 改变时旧游标失效（§9.4），从不跨绑定复用。
--
-- `received_seq` = 已持久接收到 `received_changes` 的最大 RootSeq；`applied_seq` = 这些批次里
-- 连续应用完成的最大 RootSeq。0 表示“还没有任何批次”（服务端批次序号从 1 起，见
-- `crates/server-store/migrations/0003_versions_events.sql` 的 `changes.seq > 0`）。
-- **接收游标只由写批次的事务推进**（不能先推进再写任务）；应用游标只由 `events` 模块在批次
-- 标记应用完成后推进。两者不同步是正常状态，没有“批次没到就应用”的方向。
ALTER TABLE bindings ADD COLUMN received_seq INTEGER NOT NULL DEFAULT 0 CHECK (received_seq >= 0);
ALTER TABLE bindings ADD COLUMN applied_seq INTEGER NOT NULL DEFAULT 0 CHECK (applied_seq >= 0);


-- 已持久收到的事件批次及应用结果（design §7.3、§9.3）。
--
-- 服务端一条 `changes` 行就是一个不可拆分事务批次，`(root_id, seq)` 唯一（design §7.2），
-- 因此这里以 `(binding_id, seq)` 作主键：一个 RootSeq 就是客户端能整体接收的最小单位，目录级
-- 事务不会被拆成两半。`tx_id`/`events_json` 与服务器批次同形：客户端保存收到的原始事件列表，
-- 重启后能核对“收到了什么、是否同一批”，可应用的状态另由 `remote_entries` 与 `pending_ops`
-- 承载（design §9.3：三者同一事务写入）。
--
-- `state` 是应用结果：`received` = 已持久、尚未全部应用；`applied` = 批次内条目都已落盘并更新
-- 基线；`blocked` = 至少一个条目停止自动推进（`last_error_code` 给出原因，design §15.4）。
-- 某文件阻塞不阻止接收后续批次，但应用游标不会越过它。
CREATE TABLE received_changes (
    binding_id      BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    -- 批次序号从 1 起；0 是“没有批次”的游标值，不能当批次。
    seq             INTEGER NOT NULL CHECK (seq > 0),
    tx_id           BLOB NOT NULL CHECK (length(tx_id) = 16),
    -- 收到的原始事件列表（JSON 数组文本，与服务端 `changes.events_json` 同形）；空批次没有意义。
    events_json     TEXT NOT NULL CHECK (
                        json_valid(events_json)
                        AND json_type(events_json) = 'array'
                        AND length(events_json) > 2
                    ),
    state           TEXT NOT NULL CHECK (state IN ('received', 'applied', 'blocked')),
    last_error_code TEXT CHECK (last_error_code IS NULL OR length(last_error_code) > 0),
    PRIMARY KEY (binding_id, seq),
    -- 只有 blocked 带原因：applied/received 带错误码会让“应用结果”出现两种读法。
    CHECK ((state = 'blocked') = (last_error_code IS NOT NULL))
) STRICT;


-- 入站内容传输的进度（design §7.3、§9.3）：固定 VersionId、块位图、校验结果、临时路径与
-- 取消状态。出站上传的已收块以服务端为准（§9.3“不能只相信客户端位图”），不记在这张表里。
--
-- `bitmap` 第 i 块是第 `i / 8` 字节的第 `i % 8` 位（低位在前），未收到为 0；长度必须与
-- `total_size`/`chunk_size` 推出的块数一致，尾部多余 bit 必须为 0（Rust 层写入前与读取时都
-- 检查）。位图与临时路径一起持久化，重启后从已收块继续下载，不重下完整内容。
CREATE TABLE transfers (
    binding_id  BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    version_id  BLOB NOT NULL CHECK (length(version_id) = 16),
    chunk_size  INTEGER NOT NULL CHECK (chunk_size > 0),
    total_size  INTEGER NOT NULL CHECK (total_size >= 0),
    bitmap      BLOB NOT NULL
                CHECK (length(bitmap) = ((total_size + chunk_size - 1) / chunk_size + 7) / 8),
    -- 临时文件路径（缓存目录内）；安装/丢弃由调用方做，本层不做文件系统 I/O。
    temp_path   TEXT NOT NULL CHECK (length(temp_path) > 0),
    state       TEXT NOT NULL CHECK (state IN ('running', 'cancelled', 'completed', 'failed')),
    -- 内容校验结果：1 = 已通过校验。只有校验通过的传输才能是 completed（design §9.3）。
    verified    INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0, 1)),
    last_error_code TEXT CHECK (last_error_code IS NULL OR length(last_error_code) > 0),
    PRIMARY KEY (binding_id, version_id),
    CHECK ((state = 'failed') = (last_error_code IS NOT NULL)),
    CHECK (state <> 'completed' OR verified = 1)
) STRICT;


-- 可恢复的本地动作（design §7.3、§8.4）：替换、移动、释放、导入。写入顺序是“暂存内容 → 校验
-- → 持久意图 → 受控替换 → 更新 baseline”，因此意图先于磁盘改动落库；文件已换而数据库未更新时，
-- 靠 `content_hash` 与 `source_path` 辨认结果（§8.4）。
--
-- `intent_id` 只在本客户端库内有意义（不发给服务器），用整数插入序：恢复时按它升序重放，与
-- `pending_ops` 的 rowid 顺序同一类约定（I04：不使用客户端时钟决定先后）。
CREATE TABLE local_intents (
    intent_id    INTEGER PRIMARY KEY,
    binding_id   BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('replace', 'move', 'release', 'import')),
    -- 目标条目：替换/移动/释放有服务器身份；导入的本地新文件在提交前还没有，故可为 NULL。
    entry_id     BLOB CHECK (entry_id IS NULL OR length(entry_id) = 16),
    -- 动作作用的位置（相对绑定根）。同一绑定的同一路径同时最多一个意图。
    target_path  TEXT NOT NULL CHECK (length(target_path) > 0),
    -- 来源位置：替换的暂存文件、移动的旧路径、导入的来源文件；释放与无暂存的动作没有。
    source_path  TEXT CHECK (source_path IS NULL OR length(source_path) > 0),
    -- 期望内容哈希（SHA-256 原始字节）：文件已换但数据库未更新时用它辨认结果。
    content_hash BLOB CHECK (content_hash IS NULL OR length(content_hash) > 0),
    -- 意图开始前观察到的 generation：恢复时判断是否已有更新的本地修改（旧意图不得覆盖新内容）。
    generation   INTEGER CHECK (generation IS NULL OR generation >= 0),
    state        TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'failed')),
    last_error_code TEXT CHECK (last_error_code IS NULL OR length(last_error_code) > 0),
    UNIQUE (binding_id, target_path),
    CHECK ((state = 'failed') = (last_error_code IS NOT NULL))
) STRICT;

CREATE INDEX local_intents_by_binding ON local_intents (binding_id);


-- 需要关注的问题（design §7.3、§18.2）：去重键、已读状态、是否已恢复。
--
-- 去重键由调用方按“原因 + Root”组成：同一原因重复报告只累加次数、不刷屏（§18.2）；恢复后置为
-- resolved，再次出现时重新成为未读的 active（“不反复弹窗”不等于把复发问题藏起来）。
-- `binding_id` 可为 NULL：缓存盘满这类问题不属于某个具体绑定。
CREATE TABLE notifications (
    dedupe_key  TEXT NOT NULL PRIMARY KEY CHECK (length(dedupe_key) > 0),
    binding_id  BLOB REFERENCES bindings (id) ON DELETE CASCADE,
    entry_id    BLOB CHECK (entry_id IS NULL OR length(entry_id) = 16),
    -- 问题原因（design §15.4 错误码文本）；未知文本按坏行拒绝，不猜类别。
    code        TEXT NOT NULL CHECK (length(code) > 0),
    state       TEXT NOT NULL DEFAULT 'active' CHECK (state IN ('active', 'resolved')),
    is_read     INTEGER NOT NULL DEFAULT 0 CHECK (is_read IN (0, 1)),
    occurrences INTEGER NOT NULL DEFAULT 1 CHECK (occurrences >= 1)
) STRICT;

CREATE INDEX notifications_by_binding ON notifications (binding_id);
