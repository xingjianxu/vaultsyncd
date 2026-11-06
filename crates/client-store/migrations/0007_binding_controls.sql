-- S061: binding lifecycle is retained separately; unbind never cascades user state.
CREATE TABLE binding_controls (
    binding_id BLOB NOT NULL PRIMARY KEY REFERENCES bindings(id) ON DELETE CASCADE,
    paused INTEGER NOT NULL DEFAULT 0 CHECK(paused IN (0,1)),
    detached INTEGER NOT NULL DEFAULT 0 CHECK(detached IN (0,1)),
    rules TEXT NOT NULL DEFAULT '',
    revision INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0),
    mode_pending INTEGER NOT NULL DEFAULT 0 CHECK(mode_pending IN (0,1))
) STRICT;
CREATE TABLE binding_participation (
    binding_id BLOB NOT NULL REFERENCES bindings(id) ON DELETE CASCADE,
    entry_id BLOB NOT NULL CHECK(length(entry_id)=16),
    state TEXT NOT NULL CHECK(state IN ('excluded','reconcile_required')),
    PRIMARY KEY(binding_id,entry_id)
) STRICT;
-- A certificate only covers the exact remote version and observed local generation/identity.
CREATE TABLE binding_full_proofs (
    binding_id BLOB NOT NULL REFERENCES bindings(id) ON DELETE CASCADE,
    entry_id BLOB NOT NULL CHECK(length(entry_id)=16),
    stamp TEXT NOT NULL,
    PRIMARY KEY(binding_id,entry_id)
) STRICT;
-- Never reuse a rules version: delayed reconciliation acknowledgements cannot unlock a new policy.
CREATE TABLE binding_rules_history (
    binding_id BLOB NOT NULL REFERENCES bindings(id) ON DELETE CASCADE,
    version TEXT NOT NULL CHECK(length(version)>0),
    PRIMARY KEY(binding_id,version)
) STRICT;
