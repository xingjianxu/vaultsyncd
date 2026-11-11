-- S188: endpoint configuration is keyed by stable server identity. Bindings already store that
-- identity independently. Login profiles and Windows Credential Manager entries keep their keys.
CREATE TABLE agent_connections (
    server_id TEXT PRIMARY KEY CHECK (
        length(CAST(server_id AS BLOB)) = 32 AND server_id NOT GLOB '*[^0-9a-f]*'
    ),
    server_url TEXT NOT NULL CHECK (length(server_url) > 0 AND length(server_url) <= 4096),
    ca_file TEXT,
    development_http INTEGER NOT NULL DEFAULT 0 CHECK (development_http IN (0, 1))
) STRICT;

-- The old singleton could store HTTP only when the agent explicitly allowed development HTTP.
-- The runtime still validates that any such endpoint is loopback.
INSERT INTO agent_connections (server_id, server_url, ca_file, development_http)
SELECT server_id, server_url, ca_file,
       lower(ltrim(server_url, char(9, 10, 13, 32))) LIKE 'http:%'
FROM agent_connection;

-- An old vault-level notification came from the singleton server. Preserve its complete
-- lifecycle and delivery claim while adding the server scope. A binding is stronger evidence
-- when present. Without a singleton, use a Vault only when exactly one server owns it.
-- Unattributable orphan keys stay intact and visible for recovery.
WITH scopes AS MATERIALIZED (
    SELECT n.dedupe_key AS old_key,
           coalesce(
               (SELECT lower(hex(b.server_id)) FROM bindings b WHERE b.id = n.binding_id),
               (SELECT server_id FROM agent_connection WHERE singleton = 1),
               (SELECT CASE WHEN count(DISTINCT hex(b.server_id)) = 1
                            THEN lower(hex(min(b.server_id))) END
                FROM bindings b WHERE lower(hex(b.vault_id)) = substr(n.dedupe_key, 7, 32))
           ) AS server_id
    FROM notifications n
    WHERE substr(n.dedupe_key, 1, 6) = 'vault:'
      AND substr(n.dedupe_key, 39, 1) = ':'
      AND length(substr(n.dedupe_key, 7, 32)) = 32
      AND substr(n.dedupe_key, 7, 32) NOT GLOB '*[^0-9a-f]*'
)
UPDATE notifications AS original
SET dedupe_key = (
    SELECT 'server:' || scopes.server_id || ':' || scopes.old_key
    FROM scopes WHERE scopes.old_key = original.dedupe_key
)
WHERE original.dedupe_key IN (
    SELECT scopes.old_key FROM scopes
    WHERE scopes.server_id IS NOT NULL
      AND NOT EXISTS (
          SELECT 1 FROM notifications existing
          WHERE existing.dedupe_key = 'server:' || scopes.server_id || ':' || scopes.old_key
      )
);

DROP TABLE agent_connection;
