-- S225：元数据操作与传输分开，保留既有记录身份、全部快照和分页高水位。
CREATE TEMP TABLE activity_sequence_before AS
    SELECT seq FROM sqlite_sequence WHERE name='file_activity';
CREATE TABLE file_activity_next (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    vault_id BLOB NOT NULL REFERENCES vaults(id) ON DELETE RESTRICT,
    occurred_at INTEGER NOT NULL CHECK(occurred_at>=0),
    client_id BLOB CHECK(client_id IS NULL OR length(client_id)=16),
    client_label TEXT NOT NULL,
    client_kind TEXT NOT NULL CHECK(client_kind IN ('client','browser','unknown')),
    device_id BLOB CHECK(device_id IS NULL OR length(device_id)=16),
    direction TEXT NOT NULL CHECK(direction IN ('upload','download','operation')),
    action TEXT NOT NULL CHECK(action IN ('upload_finalize','sync_commit','download','create_directory','move','delete','restore_version','restore_deleted')),
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
       OR (action='download' AND direction='download' AND status IN ('served','failed'))
       OR (action IN ('create_directory','move','delete','restore_version','restore_deleted')
           AND direction='operation' AND status='committed'))
) STRICT;
INSERT INTO file_activity_next SELECT * FROM file_activity;
DROP TABLE file_activity;
ALTER TABLE file_activity_next RENAME TO file_activity;
UPDATE sqlite_sequence SET seq=max(seq,coalesce((SELECT seq FROM activity_sequence_before),0))
    WHERE name='file_activity';
DROP TABLE activity_sequence_before;
CREATE INDEX file_activity_page ON file_activity(vault_id,id DESC);
CREATE INDEX file_activity_direction_page ON file_activity(vault_id,direction,id DESC);
CREATE UNIQUE INDEX file_activity_finalized ON file_activity(vault_id,upload_id)
    WHERE action='upload_finalize' AND status='ready';
CREATE UNIQUE INDEX file_activity_committed ON file_activity(vault_id,device_id,operation_id,entry_id)
    WHERE status='committed';
