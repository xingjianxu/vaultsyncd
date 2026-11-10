-- S172：只记录启用后的证据，不从版本/运行日志反推传输。
CREATE TABLE operation_activity_sources (
    vault_id BLOB NOT NULL REFERENCES vaults(id) ON DELETE RESTRICT,
    device_id BLOB NOT NULL CHECK(length(device_id)=16),
    op_id BLOB NOT NULL CHECK(length(op_id)=16),
    request_digest BLOB NOT NULL CHECK(length(request_digest)=32),
    client_id BLOB CHECK(client_id IS NULL OR length(client_id)=16),
    client_label TEXT NOT NULL,
    client_kind TEXT NOT NULL CHECK(client_kind IN ('client','browser','unknown')),
    upload_id BLOB CHECK(upload_id IS NULL OR length(upload_id)=16),
    PRIMARY KEY(vault_id,device_id,op_id)
) STRICT;
CREATE TABLE file_activity (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    vault_id BLOB NOT NULL REFERENCES vaults(id) ON DELETE RESTRICT,
    occurred_at INTEGER NOT NULL CHECK(occurred_at>=0),
    client_id BLOB CHECK(client_id IS NULL OR length(client_id)=16),
    client_label TEXT NOT NULL,
    client_kind TEXT NOT NULL CHECK(client_kind IN ('client','browser','unknown')),
    device_id BLOB CHECK(device_id IS NULL OR length(device_id)=16),
    direction TEXT NOT NULL CHECK(direction IN ('upload','download')),
    action TEXT NOT NULL CHECK(action IN ('upload_finalize','sync_commit','download')),
    status TEXT NOT NULL CHECK(status IN ('ready','committed','served','failed')),
    entry_id TEXT,
    version_id TEXT,
    path TEXT,
    size INTEGER CHECK(size IS NULL OR size>=0),
    bytes INTEGER CHECK(bytes IS NULL OR bytes>=0),
    offset INTEGER CHECK(offset IS NULL OR offset>=0),
    operation_id BLOB CHECK(operation_id IS NULL OR length(operation_id)=16),
    upload_id BLOB CHECK(upload_id IS NULL OR length(upload_id)=16),
    error_code TEXT,
    CHECK((action='upload_finalize' AND direction='upload' AND status IN ('ready','failed'))
       OR (action='sync_commit' AND direction='upload' AND status='committed')
       OR (action='download' AND direction='download' AND status IN ('served','failed')))
) STRICT;
CREATE INDEX file_activity_page ON file_activity(vault_id,id DESC);
CREATE INDEX file_activity_direction_page ON file_activity(vault_id,direction,id DESC);
CREATE UNIQUE INDEX file_activity_finalized ON file_activity(vault_id,upload_id)
    WHERE action='upload_finalize' AND status='ready';
CREATE UNIQUE INDEX file_activity_committed ON file_activity(vault_id,device_id,operation_id,entry_id)
    WHERE action='sync_commit';
