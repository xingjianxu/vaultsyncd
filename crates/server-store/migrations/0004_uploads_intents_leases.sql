-- 0004_uploads_intents_leases.sql — 上传会话与片段、工作目录发布意图、对象保护租约（S029）。
--
-- 落 design §7.2 的 uploads/upload_parts/fs_intents/backup_leases 四张表，加一张对象保护租约
-- （design §19.4 的“上传/下载租约”、§11 “短期内容租约或等价机制”）。五张表一次建出，因为它们
-- 共同支撑同一条恢复与 GC 边界：未完成的意图、未齐的上传片段和有效租约都是**持久化事实**，重启后
-- 必须能从数据库列出并按证据推进，而不是靠重放普通日志推测（design §11.3）。
--
-- 表与设计字段的对应：
--
--   * `uploads`：id、root_id、session_id、expected_size、hash、expires_at、state。会话绑定一个
--     不可变的本地内容快照（总大小 + 总 SHA-256，design §13.1）；`chunk_size` 是会话的块几何：
--     `POST /v1/roots/{root}/uploads` 由客户端提出、服务端可选定块大小（design §15.2），缺块列表
--     与每块期望长度都由它算出，否则“已收/缺哪些块”无法给出确定答案。
--   * `upload_parts`：upload_id、part_index、length、hash、path；主键 `(upload_id, part_index)` 就是
--     design §7.2 的“索引唯一”——重复上传同一索引永远不会产生第二行，只有内容一致才算幂等。
--   * `fs_intents`：intent_id、root_id、expected_before、desired_after、phase，加恢复所需的目标
--     路径、暂存路径与操作种类（design §11.2 步骤 5：意图含“前后内容引用及恢复所需路径”）。
--   * `backup_leases`：id、snapshot_seq_map、expires_at、manifest_ref；被备份引用的对象在租约
--     失效前不得被 GC（design §19.1、§19.4）。
--   * `object_leases`：下载与提交期间对**已登记内容对象**的短期保护（design §19.4“上传/下载租约”、
--     §11“跨越 30 天历史到期窗口的下载、恢复和释放检查通过短期内容租约或等价机制保护其版本”）。
--     它不引入第二套引用模型：租约只是“这个对象在此刻之前不许被 GC”。
--
-- 身份列 16 字节 BLOB、内容哈希 32 字节 BLOB（SHA-256），与 0002/0003 一致，不存十六进制文本。

CREATE TABLE uploads (
    id            BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    root_id       BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    -- 会话身份：设计里归属 sessions 表（S043 建立），本迁移不建该表，因此这里只校验长度；
    -- 追加外键需要重建表，由会话卡按当时的 schema 决定（与 0002 的 entries.content_version_id 同理）。
    session_id    BLOB NOT NULL CHECK (length(session_id) = 16),
    -- 快照总大小与总 SHA-256：上传完成后要按它们校验整份内容（design §13.1、§11.2 步骤 2）。
    expected_size INTEGER NOT NULL CHECK (expected_size >= 0),
    hash          BLOB NOT NULL CHECK (length(hash) = 32),
    -- 块几何：块大小属会话契约（design §13.1、§15.2），它决定块数与每块期望长度。
    chunk_size    INTEGER NOT NULL CHECK (chunk_size > 0),
    expires_at    INTEGER NOT NULL CHECK (expires_at >= 0),
    -- open：接收块中；complete：全部块齐备且逐块核对通过，等待 finalize 组装校验；
    -- finalized：已发布为内容对象（终态）；aborted：已放弃/会话过期（终态）。
    -- 不给“半完成的内容”留状态：块未齐不可能进入 complete（由 `uploads::mark_upload_complete` 保证）。
    state         TEXT NOT NULL CHECK (state IN ('open', 'complete', 'finalized', 'aborted'))
) STRICT;

-- 续传与过期清理都只关心未结束的上传（design §13.2 续传、§19.4 不完整上传可按会话过期清理）。
CREATE INDEX uploads_open_by_root ON uploads (root_id, expires_at, id)
    WHERE state IN ('open', 'complete');

CREATE TABLE upload_parts (
    upload_id  BLOB NOT NULL REFERENCES uploads (id) ON DELETE CASCADE,
    part_index INTEGER NOT NULL CHECK (part_index >= 0),
    -- 每块非空：空内容没有块（0 字节文件以 expected_size = 0、0 块表示），块长由会话几何决定。
    length     INTEGER NOT NULL CHECK (length > 0),
    hash       BLOB NOT NULL CHECK (length(hash) = 32),
    -- 暂存片段文件。全库唯一：两行指向同一文件时，按会话清理会删掉别人的字节。
    path       TEXT NOT NULL UNIQUE CHECK (length(path) > 0),
    -- 索引唯一（design §7.2）：同一会话的同一块只有一行。
    PRIMARY KEY (upload_id, part_index)
) STRICT;

CREATE TABLE fs_intents (
    intent_id       BLOB NOT NULL PRIMARY KEY CHECK (length(intent_id) = 16),
    root_id         BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    -- 意图日志同样覆盖纯移动、创建目录与删除（design §11.2 末段），不只是内容替换。
    kind            TEXT NOT NULL CHECK (kind IN ('put_content', 'create', 'move', 'delete')),
    -- 发布目标在 Root 内的相对路径；崩溃恢复靠它在工作目录里找证据（design §11.3）。
    target_path     TEXT NOT NULL CHECK (length(target_path) > 0),
    -- 发布用的暂存文件（目标所在子卷内，design §11.2 步骤 6）；不需要暂存的动作（删除/移动）为空。
    staging_path    TEXT CHECK (staging_path IS NULL OR length(staging_path) > 0),
    -- 意图前提与目标：JSON object。**不用 NULL 表达“当时不存在”**——那是必须记录的事实
    -- （新建 vs 覆盖），因此两种状态都由对象里的字段表达；具体编码属提交状态机（S034）。
    expected_before TEXT NOT NULL CHECK (
        json_valid(expected_before) AND json_type(expected_before) = 'object'
    ),
    desired_after   TEXT NOT NULL CHECK (
        json_valid(desired_after) AND json_type(desired_after) = 'object'
    ),
    -- prepared：意图已持久化，工作文件未发布；published：工作文件已按 desired_after 发布，
    -- 元数据事务未完成；finalized：元数据事务已完成（终态）；needs_recovery：现场与意图都无法
    -- 解释，暂停破坏性写入等人工核对（design §11.3）。转换规则由 `intents::FsIntentPhase` 给出。
    phase           TEXT NOT NULL CHECK (
        phase IN ('prepared', 'published', 'finalized', 'needs_recovery')
    ),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at      INTEGER NOT NULL CHECK (updated_at >= 0),
    CHECK (updated_at >= created_at)
) STRICT;

-- 启动恢复按 Root 列出未完成意图，顺序必须稳定（同一时刻创建的意图靠身份兜底）。
CREATE INDEX fs_intents_unfinished_by_root ON fs_intents (root_id, created_at, intent_id)
    WHERE phase <> 'finalized';

CREATE TABLE backup_leases (
    id               BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    -- 各 Root 已确认的 seq（design §19.1 步骤 2）。JSON object：键是 Root 身份文本，值是十进制
    -- seq；至少一个 Root，空映射的租约保护不了任何对象。
    snapshot_seq_map TEXT NOT NULL CHECK (
        json_valid(snapshot_seq_map)
        AND json_type(snapshot_seq_map) = 'object'
        AND length(snapshot_seq_map) > 2
    ),
    expires_at       INTEGER NOT NULL CHECK (expires_at >= 0),
    -- 备份清单的位置；租约与清单互为证据（design §19.1 步骤 4—5）。
    manifest_ref     TEXT NOT NULL CHECK (length(manifest_ref) > 0)
) STRICT;

CREATE INDEX backup_leases_by_expiry ON backup_leases (expires_at);

CREATE TABLE object_leases (
    id          BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    root_id     BLOB NOT NULL,
    object_hash BLOB NOT NULL CHECK (length(object_hash) = 32),
    -- download：固定版本下载/恢复/释放检查期间保护其内容；commit：对象已落盘但尚未被版本引用的
    -- 发布窗口（design §11.2 步骤 3—7）。
    purpose     TEXT NOT NULL CHECK (purpose IN ('download', 'commit')),
    expires_at  INTEGER NOT NULL CHECK (expires_at >= 0),
    -- 租约只能保护**已登记**对象：文件系统里还没有的哈希没有可保护的内容，写租约没有意义；
    -- 反向的 ON DELETE RESTRICT 让“删除仍被租约保护的对象行”直接失败。
    FOREIGN KEY (root_id, object_hash) REFERENCES objects (root_id, hash) ON DELETE RESTRICT
) STRICT;

CREATE INDEX object_leases_by_object ON object_leases (root_id, object_hash);
CREATE INDEX object_leases_by_expiry ON object_leases (expires_at);
