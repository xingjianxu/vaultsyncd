-- 0001_initial.sql — 客户端数据库基线（S030）。
--
-- 落 design §7.3 的客户端表与 §8.4 的持重要求，只建本卡范围的前五张表：
-- bindings、baseline_entries、remote_entries、local_entries、pending_ops。
-- received_changes、transfers、local_intents、notifications 属 S031，由后续迁移追加。
--
-- 本目录只服务客户端库（crates/client-store）；服务端 schema 在
-- crates/server-store/migrations/，两份迁移互不复用，也不允许指向同一个数据库文件
-- （库种类由 src/db.rs 的 `application_id` 门禁保证）。
--
-- 三张条目表（baseline/remote/local）之间**没有外键**，这是有意的：同一条目必须能同时
-- 保存“旧的已确认基线 + 更新的远端状态 + 本地 dirty”（design §7.3、§8.4），任何一方的
-- 写入或删除都不得约束另一方。三张表各自只依赖 bindings。
--
-- 身份列一律是 16 字节 BLOB（crates/domain 的不透明身份），不存十六进制文本，避免同一身份
-- 出现两种字节表示。表用 STRICT：列类型按声明严格检查。


CREATE TABLE bindings (
    id            BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    server_id     BLOB NOT NULL CHECK (length(server_id) = 16),
    device_id     BLOB NOT NULL CHECK (length(device_id) = 16),
    root_id       BLOB NOT NULL CHECK (length(root_id) = 16),
    -- 设备本地路径与缓存位置（design §7.3）。客户端路径可能是 Windows 的 `C:\dir` 或
    -- Linux 的 `/mnt/dir`，本层只要求非空；绝对性、是否重叠等绑定规则由绑定卡（S058 起）判。
    local_path    TEXT NOT NULL CHECK (length(local_path) > 0),
    cache_path    TEXT NOT NULL CHECK (length(cache_path) > 0),
    -- 同步模式集合来自 design C34：完整双向 `full` 与按需双向 `on_demand`。
    mode          TEXT NOT NULL CHECK (mode IN ('full', 'on_demand')),
    -- 排除规则的版本标识（design §7.3 “规则版本”）：内容由规则卡定义，本层只存标识。
    rules_version TEXT NOT NULL CHECK (length(rules_version) > 0),
    -- 绑定时看到的服务器 epoch；epoch 改变意味着旧游标失效（design §7.1）。
    server_epoch  INTEGER NOT NULL CHECK (server_epoch >= 0)
) STRICT;


-- 最近已确认的共同基线（design §7.3）。`revision` 是服务器分配的修订号；`content_version_id`
-- 是文件内容版本，目录没有（I12）。`deleted` 是已确认的服务器删除标记，不是本地推断。
CREATE TABLE baseline_entries (
    binding_id         BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    entry_id           BLOB NOT NULL CHECK (length(entry_id) = 16),
    -- 位置 = 父条目身份 + 名称。根目录的合成条目没有父项也没有名称。
    parent_entry_id    BLOB CHECK (parent_entry_id IS NULL OR length(parent_entry_id) = 16),
    name               TEXT NOT NULL,
    kind               TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    revision           INTEGER NOT NULL CHECK (revision >= 0),
    content_version_id BLOB CHECK (content_version_id IS NULL OR length(content_version_id) = 16),
    deleted            INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
    PRIMARY KEY (binding_id, entry_id),
    -- 目录没有内容版本（design §7.1、I12）。
    CHECK (kind <> 'directory' OR content_version_id IS NULL),
    -- 根条目不是某个目录里的分量：没有父项、没有名称、只能是目录。
    CHECK (parent_entry_id IS NOT NULL OR (name = '' AND kind = 'directory'))
) STRICT;


-- 最新收到但可能尚未落盘的远端状态（design §7.3）。与 baseline 分开，因此“服务器有新版本”
-- 不需要先改写已确认基线；远端行也不必存在对应基线（首次同步 I07）。
CREATE TABLE remote_entries (
    binding_id         BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    entry_id           BLOB NOT NULL CHECK (length(entry_id) = 16),
    parent_entry_id    BLOB CHECK (parent_entry_id IS NULL OR length(parent_entry_id) = 16),
    name               TEXT NOT NULL,
    kind               TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    revision           INTEGER NOT NULL CHECK (revision >= 0),
    content_version_id BLOB CHECK (content_version_id IS NULL OR length(content_version_id) = 16),
    deleted            INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
    PRIMARY KEY (binding_id, entry_id),
    CHECK (kind <> 'directory' OR content_version_id IS NULL),
    CHECK (parent_entry_id IS NOT NULL OR (name = '' AND kind = 'directory'))
) STRICT;


-- 本地观察与本地状态（design §7.3、§8.4）。`dirty` 的 clean 只是上次观察结果，不是“文件
-- 当前未变”的证明；`generation` 用来判断上传确认是否仍对应当前本地内容（旧 ACK 不得清除新
-- generation）。没有指向 baseline_entries 的外键：本地新文件可以先有 local 行而没有基线。
CREATE TABLE local_entries (
    binding_id    BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    entry_id      BLOB NOT NULL CHECK (length(entry_id) = 16),
    generation    INTEGER NOT NULL CHECK (generation >= 0),
    dirty         TEXT NOT NULL
                  CHECK (dirty IN ('clean', 'dirty', 'snapshotting', 'awaiting_commit')),
    residency     TEXT NOT NULL CHECK (residency IN ('metadata_only', 'partial', 'materialized')),
    -- 平台编码的当前指纹（大小、时间、文件身份等由平台层组合），本层不解释；
    -- NULL 表示尚未观察，不能当成“与基线一致”。
    fingerprint   BLOB CHECK (fingerprint IS NULL OR length(fingerprint) > 0),
    -- 平台 file identity（S014 的 16 字节 RootId/EntryId 之外的平台身份），
    -- 用于重启后核对“这个路径还是不是同一条目”；本层不解释其编码。
    file_identity BLOB CHECK (file_identity IS NULL OR length(file_identity) > 0),
    PRIMARY KEY (binding_id, entry_id)
) STRICT;


-- 待执行的本地操作（design §7.3、§8.4）。队列按插入顺序（rowid）取用，不使用客户端时钟：
-- 时间戳只用于展示与检测候选变化（I04）。
CREATE TABLE pending_ops (
    op_id               BLOB NOT NULL PRIMARY KEY CHECK (length(op_id) = 16),
    binding_id          BLOB NOT NULL REFERENCES bindings (id) ON DELETE CASCADE,
    -- 操作类型。取值集合由执行器卡（S055）定义；本层只要求非空，不提前冻结一组字符串。
    kind                TEXT NOT NULL CHECK (length(kind) > 0),
    -- 操作目标条目；本地新建在提交前还没有服务器 EntryId，因此可为 NULL。
    entry_id            BLOB CHECK (entry_id IS NULL OR length(entry_id) = 16),
    -- 规划时使用的基线：修订号 + 内容版本（design §7.3 “本地操作、基线”）。
    baseline_revision   INTEGER CHECK (baseline_revision IS NULL OR baseline_revision >= 0),
    baseline_version_id BLOB CHECK (baseline_version_id IS NULL OR length(baseline_version_id) = 16),
    -- 依赖：同一队列里的前置操作。自引用必须指向不同的操作。
    depends_on          BLOB REFERENCES pending_ops (op_id) ON DELETE RESTRICT,
    -- 固定内容版本：租约/重试期间不得被 GC 的版本（design §8.4）。
    pinned_version_id   BLOB CHECK (pinned_version_id IS NULL OR length(pinned_version_id) = 16),
    attempts            INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- 最近一次尝试的失败原因（design §15.4 错误码文本）；NULL 表示尚未失败或已成功重试。
    last_error_code     TEXT CHECK (last_error_code IS NULL OR length(last_error_code) > 0),
    CHECK (depends_on IS NULL OR depends_on <> op_id)
) STRICT;

-- 按绑定取队列：rowid 是插入顺序，索引带 binding_id 后仍按 rowid 递增取用。
CREATE INDEX pending_ops_by_binding ON pending_ops (binding_id);
