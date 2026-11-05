-- 0006_sessions.sql — 登录会话与刷新凭据（S043）。
--
-- 落 design §7.2 的 `sessions` 与 §17.1“访问凭据短期、刷新凭据可轮换，服务端只保存必要摘要”。
-- 与 design 的字段清单有一处有意收窄和一处有意扩展，两者都在 `sessions.rs` 的模块文档里说明：
--
--   * design 写作 `device_id`；首发没有设备注册表（design §17.1 明确“不额外添加设备审批”），
--     `uploads.session_id` 也只要会话身份，因此这里存登录时提交的 `device_label` 文本，不做设备身份。
--   * design 写作一个 `token_digest`；一次登录同时签发访问凭据与刷新凭据，两者必须一起轮换，
--     所以一张行里放两个摘要（各 32 字节 SHA-256），避免出现“刷新凭据已经换掉、访问凭据还是旧的”
--     这种跨行不一致。
--
-- 令牌本身（明文）不落库：只有摘要进库，服务端无法从库里还原可用的凭据。
--
-- `replaced_by` 指向轮换出的后继会话：旧刷新凭据被再次提交时，这一列说明它已经被谁取代，
-- 供日志与诊断关联（不用于放行）。自引用外键不需要重建表。
--
-- 到期与撤销都是可检的列，判定的权威是实现（`sessions.rs` 只在同一写事务里比较），
-- 但库层面也要求时间自洽：创建 ≤ 访问到期 ≤ 刷新到期。

CREATE TABLE sessions (
    id                 BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    user_id            BLOB NOT NULL REFERENCES users (id) ON DELETE RESTRICT,
    device_label       TEXT NOT NULL CHECK (length(device_label) > 0),
    access_digest      BLOB NOT NULL CHECK (length(access_digest) = 32),
    refresh_digest     BLOB NOT NULL CHECK (length(refresh_digest) = 32),
    created_at         INTEGER NOT NULL CHECK (created_at >= 0),
    access_expires_at  INTEGER NOT NULL CHECK (access_expires_at >= created_at),
    refresh_expires_at INTEGER NOT NULL CHECK (refresh_expires_at >= access_expires_at),
    revoked_at         INTEGER CHECK (revoked_at IS NULL OR revoked_at >= created_at),
    replaced_by        BLOB CHECK (replaced_by IS NULL OR length(replaced_by) = 16),
    FOREIGN KEY (replaced_by) REFERENCES sessions (id) ON DELETE RESTRICT
) STRICT;

-- 一个摘要只对应一行：按摘要查会话必须唯一，否则“用哪个会话”取决于行顺序。
CREATE UNIQUE INDEX sessions_access_digest ON sessions (access_digest);
CREATE UNIQUE INDEX sessions_refresh_digest ON sessions (refresh_digest);

-- 账号维度的查询（后续“密码重置后失效全部会话”与运维诊断都要按账号取）。
CREATE INDEX sessions_user_id ON sessions (user_id);
