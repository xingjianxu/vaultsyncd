-- 0002_namespace.sql — 服务端命名空间：账号归属、Root 与条目树（S027）。
--
-- 落 design §7.2 的 users/roots/entries 与 §7.4 的元数据约束。三张表一次建出，是因为
-- “owner scope”与“父子同 Root”只能用外键表达：
--
--   * `roots.owner_id → users(id)`：每个 Root 都属于一个账号（design §17.1），数据访问必须带
--     owner scope，不能“登录成功即可访问任意 Root”。首发只有一位 owner，但表里不省略归属。
--   * `entries(root_id, parent_id) → entries(root_id, id)`：父项与子项必须同一个 Root。跨 Root 的
--     父项在 SQLite 里直接违反外键，而不是只靠调用方自觉；这条外键也顺带保证父项存在。
--
-- 身份列是 16 字节 BLOB（crates/domain 的不透明身份，见 ids.rs），不存十六进制文本，避免同一
-- 身份有两种字节表示。表用 STRICT：列类型按声明严格检查。
--
-- 本迁移不建 versions（S028）：`entries.content_version_id` 只做长度与“目录无内容版本”检查，
-- 指向已持久化版本的外键属于 versions 卡（SQLite 追加外键需要重建表，由该卡决定）。

CREATE TABLE users (
    id            BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    login         TEXT NOT NULL UNIQUE,
    -- 只有首次 owner 初始化流程写入密码散列（design §17.1：没有内置默认密码，也不允许公开注册），
    -- 因此不存在“没有密码的账号”这种半初始化状态。
    password_hash TEXT NOT NULL,
    -- 取值集合由账号卡（S042）定义；本层只要求它必须有值。
    status        TEXT NOT NULL
) STRICT;

CREATE TABLE roots (
    id                 BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    owner_id           BLOB NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    -- 注册时的绝对路径文本（服务端一侧的规范形式，规则见 roots.rs）；同一路径只能有一个 Root。
    absolute_path      TEXT NOT NULL UNIQUE,
    -- 卷身份与根目录身份是平台层给出的不透明字节（S032 的 RootSnapshot 编码），本层不解释。
    -- 身份不匹配时停止同步（design §7.2），比较由平台层做。
    volume_identity    BLOB NOT NULL CHECK (length(volume_identity) > 0),
    root_file_identity BLOB NOT NULL CHECK (length(root_file_identity) > 0),
    health             TEXT NOT NULL CHECK (health IN ('healthy', 'unavailable')),
    seq                INTEGER NOT NULL DEFAULT 0 CHECK (seq >= 0),
    epoch              INTEGER NOT NULL CHECK (epoch >= 0)
) STRICT;

CREATE TABLE entries (
    id                 BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    root_id            BLOB NOT NULL REFERENCES roots (id) ON DELETE RESTRICT,
    -- 根目录有合成条目（design §7.1）：它是每个 Root 的唯一一行 parent_id IS NULL。
    parent_id          BLOB,
    name               TEXT NOT NULL,
    name_key           TEXT NOT NULL,
    kind               TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    revision           INTEGER NOT NULL CHECK (revision >= 0),
    content_version_id BLOB CHECK (content_version_id IS NULL OR length(content_version_id) = 16),
    -- 软删除时间（UTC unix 秒）；NULL 表示活跃条目。删除即释放同父名称的唯一性。
    deleted_at         INTEGER CHECK (deleted_at IS NULL OR deleted_at >= 0),
    -- 复合外键的引用目标必须唯一；它同时服务“按 id 取同 Root 条目”。
    UNIQUE (root_id, id),
    FOREIGN KEY (root_id, parent_id) REFERENCES entries (root_id, id) ON DELETE RESTRICT,
    -- 根条目没有文件系统名称（它不是某个目录里的分量），其余条目必须有名称与比较键。
    CHECK (
        (parent_id IS NULL AND name = '' AND name_key = '' AND kind = 'directory')
        OR (parent_id IS NOT NULL AND length(name) > 0 AND length(name_key) > 0)
    ),
    -- 目录没有内容版本（design §7.1、I12）。
    CHECK (kind <> 'directory' OR content_version_id IS NULL)
) STRICT;

-- 活跃同父名称唯一（design §7.2）：`name_key` 是平台等价规则派生的比较键，`name` 是原始名称。
-- 两个索引分别防两种错误：键相同但名字不同（平台等价）、名字相同却给了不同的键（调用方不一致）。
-- 根条目的 parent_id 为 NULL，被这两个部分索引排除。
CREATE UNIQUE INDEX entries_active_sibling_name_key
    ON entries (root_id, parent_id, name_key)
    WHERE deleted_at IS NULL AND parent_id IS NOT NULL;

CREATE UNIQUE INDEX entries_active_sibling_name
    ON entries (root_id, parent_id, name)
    WHERE deleted_at IS NULL AND parent_id IS NOT NULL;

-- 每个 Root 至多一个活跃的合成根条目。
CREATE UNIQUE INDEX entries_one_active_root
    ON entries (root_id)
    WHERE deleted_at IS NULL AND parent_id IS NULL;
