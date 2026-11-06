ALTER TABLE backup_preparations ADD COLUMN integrity TEXT NOT NULL DEFAULT 'unverified' CHECK(integrity IN ('unverified','verified','incomplete'));
ALTER TABLE backup_preparations ADD COLUMN verified_at INTEGER CHECK(verified_at IS NULL OR verified_at>=0);
CREATE TABLE backup_jobs (
    backup_id BLOB NOT NULL CHECK(length(backup_id)=16),
    kind TEXT NOT NULL CHECK(kind IN ('prepare','verify','release')),
    owner_id BLOB NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    phase TEXT NOT NULL CHECK(phase IN ('queued','running','succeeded','failed')),
    created_at INTEGER NOT NULL CHECK(created_at>=0),
    detail TEXT,
    PRIMARY KEY(backup_id,kind)
) STRICT;
