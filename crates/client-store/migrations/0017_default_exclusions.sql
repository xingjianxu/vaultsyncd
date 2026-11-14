-- S196: the effective policy always prepends the built-in defaults. Older binaries
-- must refuse this database rather than reinterpret the same stored additional text.
-- Core adopts each binding in a short transaction before dispatch: retain the old
-- rule version, mint a fresh fence, and persist excluded tracked entries. Zero also
-- covers newly created bindings and interrupted startup; no user data is removed.
ALTER TABLE binding_controls ADD COLUMN default_rules_version INTEGER NOT NULL DEFAULT 0
    CHECK(default_rules_version IN (0,1));
