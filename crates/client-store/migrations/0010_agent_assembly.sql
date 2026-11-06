-- Immutable missing platform/transport evidence and restartable application action specifications.
-- Existing baseline/local/remote/pending_ops remain the only current synchronization state.
CREATE TABLE agent_evidence (
 binding_id BLOB NOT NULL REFERENCES bindings(id),
 path TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation>=0),
 evidence TEXT NOT NULL CHECK(json_valid(evidence)),
 PRIMARY KEY(binding_id,path,generation)
) STRICT;
CREATE TABLE agent_versions (
 binding_id BLOB NOT NULL REFERENCES bindings(id),
 entry_id BLOB NOT NULL CHECK(length(entry_id)=16),
 version_id BLOB NOT NULL CHECK(length(version_id)=16),
 digest BLOB NOT NULL CHECK(length(digest)=32),
 size INTEGER NOT NULL CHECK(size>=0),
 PRIMARY KEY(binding_id,version_id)
) STRICT;
CREATE TABLE agent_actions (
 op_id BLOB NOT NULL PRIMARY KEY CHECK(length(op_id)=16),
 binding_id BLOB NOT NULL REFERENCES bindings(id),
 specification TEXT NOT NULL CHECK(json_valid(specification)),
 result TEXT CHECK(result IS NULL OR json_valid(result))
) STRICT;
