-- 用户移除的服务器身份。agent_connections 没有该行时仍以本表作为持久证据：
-- 否则 load_connections 会把 agent.json 里的旧单连接当作后备入口重新暴露出来。
CREATE TABLE agent_connection_removals (
    server_id TEXT PRIMARY KEY CHECK (
        length(CAST(server_id AS BLOB)) = 32 AND server_id NOT GLOB '*[^0-9a-f]*'
    )
) STRICT;
