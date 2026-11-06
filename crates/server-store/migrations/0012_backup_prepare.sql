CREATE TABLE backup_preparations (
    id BLOB NOT NULL PRIMARY KEY CHECK(length(id)=16),
    owner_id BLOB NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    phase TEXT NOT NULL CHECK(phase IN ('preparing','ready','failed','releasing','released')),
    created_at INTEGER NOT NULL CHECK(created_at>=0),
    expires_at INTEGER NOT NULL CHECK(expires_at>created_at),
    snapshot_seq_map TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(snapshot_seq_map) AND json_type(snapshot_seq_map)='object'),
    manifest_hash BLOB CHECK(manifest_hash IS NULL OR length(manifest_hash)=32),
    detail TEXT,
    cleaned_at INTEGER CHECK(cleaned_at IS NULL OR cleaned_at>=0),
    CHECK(phase<>'ready' OR manifest_hash IS NOT NULL)
) STRICT;
