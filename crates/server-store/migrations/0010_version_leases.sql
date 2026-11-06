-- Public fixed-version lease request identities survive expiry/release for safe replay.
-- GC protection remains solely in object_leases; this table is not a second GC list.
CREATE TABLE version_lease_requests (
    id BLOB NOT NULL PRIMARY KEY CHECK(length(id)=16),
    root_id BLOB NOT NULL REFERENCES roots(id) ON DELETE RESTRICT,
    entry_id BLOB NOT NULL CHECK(length(entry_id)=16),
    version_id BLOB NOT NULL CHECK(length(version_id)=16),
    object_hash BLOB NOT NULL CHECK(length(object_hash)=32),
    size INTEGER NOT NULL CHECK(size>=0),
    closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1))
) STRICT;

CREATE TRIGGER version_lease_request_identity_is_immutable
BEFORE UPDATE OF id,root_id,entry_id,version_id,object_hash,size ON version_lease_requests
BEGIN
    SELECT RAISE(ABORT, 'version lease request identity is immutable');
END;

-- Acquisition and GC marking serialize on SQLite's writer transaction. The existing
-- object_leases FK also blocks physical metadata deletion until expired rows are pruned.
CREATE TRIGGER objects_active_lease_blocks_gc_mark
BEFORE UPDATE OF gc_state ON objects
WHEN NEW.gc_state='collectable' AND EXISTS (
    SELECT 1 FROM object_leases l WHERE l.root_id=OLD.root_id AND l.object_hash=OLD.hash
      AND l.expires_at>CAST(strftime('%s','now') AS INTEGER)
)
BEGIN
    SELECT RAISE(ABORT, 'active content lease protects object from GC');
END;
