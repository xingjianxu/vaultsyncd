-- 标记仍可被新增引用取消；deleting 与 collectable 同事务授权且不可逆。
CREATE TABLE gc_candidates (
    root_id BLOB NOT NULL,
    object_hash BLOB NOT NULL CHECK(length(object_hash)=32),
    marked_at INTEGER NOT NULL CHECK(marked_at>=0),
    phase TEXT NOT NULL CHECK(phase IN ('marked','deleting')),
    PRIMARY KEY(root_id,object_hash),
    FOREIGN KEY(root_id,object_hash) REFERENCES objects(root_id,hash) ON DELETE CASCADE
) STRICT;

CREATE TRIGGER versions_require_live_object BEFORE INSERT ON versions
WHEN EXISTS(SELECT 1 FROM objects WHERE root_id=NEW.root_id AND hash=NEW.object_hash AND gc_state='collectable')
BEGIN SELECT RAISE(ABORT,'GC object is not live'); END;

CREATE TRIGGER leases_require_live_object BEFORE INSERT ON object_leases
WHEN EXISTS(SELECT 1 FROM objects WHERE root_id=NEW.root_id AND hash=NEW.object_hash AND gc_state='collectable')
BEGIN SELECT RAISE(ABORT,'GC object is not live'); END;

CREATE TRIGGER leases_update_require_live_object BEFORE UPDATE ON object_leases
WHEN EXISTS(SELECT 1 FROM objects WHERE root_id=NEW.root_id AND hash=NEW.object_hash AND gc_state='collectable')
BEGIN SELECT RAISE(ABORT,'GC object is not live'); END;

CREATE TRIGGER deleting_object_cannot_revive BEFORE UPDATE OF gc_state ON objects
WHEN NEW.gc_state='live' AND EXISTS(SELECT 1 FROM gc_candidates WHERE root_id=OLD.root_id AND object_hash=OLD.hash AND phase='deleting')
BEGIN SELECT RAISE(ABORT,'GC deleting object cannot revive'); END;

CREATE TRIGGER backup_lease_cannot_follow_gc BEFORE INSERT ON backup_leases
WHEN EXISTS(SELECT 1 FROM objects o, json_each(NEW.snapshot_seq_map) j WHERE lower(hex(o.root_id))=j.key AND o.gc_state='collectable')
BEGIN SELECT RAISE(ABORT,'GC deletion already authorized'); END;

CREATE TRIGGER backup_lease_update_cannot_follow_gc BEFORE UPDATE ON backup_leases
WHEN EXISTS(SELECT 1 FROM objects o, json_each(NEW.snapshot_seq_map) j WHERE lower(hex(o.root_id))=j.key AND o.gc_state='collectable')
BEGIN SELECT RAISE(ABORT,'GC deletion already authorized'); END;

CREATE TRIGGER uploads_cannot_follow_gc BEFORE INSERT ON uploads
WHEN EXISTS(SELECT 1 FROM objects WHERE root_id=NEW.root_id AND hash=NEW.hash AND gc_state='collectable')
BEGIN SELECT RAISE(ABORT,'GC deletion already authorized'); END;

CREATE TRIGGER uploads_update_cannot_follow_gc BEFORE UPDATE ON uploads
WHEN NEW.state<>'aborted' AND EXISTS(SELECT 1 FROM objects WHERE root_id=NEW.root_id AND hash=NEW.hash AND gc_state='collectable')
BEGIN SELECT RAISE(ABORT,'GC deletion already authorized'); END;
