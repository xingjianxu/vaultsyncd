-- S147: 每次登录的连接身份跨刷新保持；备份去除凭据时保留历史记录。
CREATE TABLE client_connections (
    id BLOB NOT NULL PRIMARY KEY CHECK(length(id)=16),
    user_id BLOB NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    session_id BLOB UNIQUE REFERENCES sessions(id) ON DELETE SET NULL,
    device_label TEXT NOT NULL,
    client_kind TEXT NOT NULL CHECK(client_kind IN ('unknown','client','browser')),
    created_at INTEGER NOT NULL CHECK(created_at>=0),
    last_seen_at INTEGER CHECK(last_seen_at IS NULL OR last_seen_at>=created_at)
) STRICT;
CREATE INDEX client_connections_user_page ON client_connections(user_id,created_at DESC,id DESC);
-- 已有刷新链合并成原始登录；历史活动未知，不以迁移时间冒充。
WITH RECURSIVE chain(root_id,session_id,user_id,device_label,created_at,replaced_by) AS (
    SELECT s.id,s.id,s.user_id,s.device_label,s.created_at,s.replaced_by FROM sessions s
    WHERE NOT EXISTS(SELECT 1 FROM sessions p WHERE p.replaced_by=s.id)
    UNION ALL
    SELECT c.root_id,s.id,c.user_id,c.device_label,c.created_at,s.replaced_by
    FROM chain c JOIN sessions s ON s.id=c.replaced_by
)
INSERT INTO client_connections(id,user_id,session_id,device_label,client_kind,created_at,last_seen_at)
SELECT root_id,user_id,session_id,device_label,'unknown',created_at,NULL FROM chain WHERE replaced_by IS NULL;
