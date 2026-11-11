//! First import must retain the authenticated snapshot while excluding actions, including replay.
use super::*;
use crate::bindings::control::{BindingControls, ExecutionGate, FullFile, FullMaterializer};

async fn fixture(base: &Path) -> (Arc<Database>, Arc<DiskRemote>, Arc<DiskLocal>) {
    let db = setup(base).await;
    fs::remove_dir_all(base.join("local")).unwrap();
    fs::create_dir(base.join("local")).unwrap();
    write_file(&base.join("local/local.xlsx"), b"local workbook");
    write_file(&base.join("local/~$local.xlsx"), b"local Office lock");
    let remote = Arc::new(DiskRemote {
        base: base.to_owned(),
        fault: String::new(),
    });
    let mut state = remote.state().unwrap();
    state.entries.truncate(1);
    for (marker, name, bytes) in [
        (2, "remote.xlsx", b"remote workbook".as_slice()),
        (3, "~$remote.xlsx", b"remote Office lock".as_slice()),
    ] {
        let version = EntryId::from_bytes([marker + 0x40; 16]).to_string();
        write_file(&base.join("content").join(&version), bytes);
        state.entries.push(SnapshotEntry {
            modified_at: None,
            entry_id: EntryId::from_bytes([marker; 16]).to_string(),
            hash: Some(hex(&digest_of(bytes))),
            kind: WireKind::File,
            name: name.to_owned(),
            parent_id: Some(VAULT_ENTRY.to_string()),
            revision: "1".to_owned(),
            size: Some(bytes.len().to_string()),
            version_id: Some(version),
        });
    }
    remote.save(&state).unwrap();
    let local = Arc::new(DiskLocal::new(base, ""));
    (db, remote, local)
}

#[tokio::test]
async fn defaults_and_additional_negations_filter_actions_not_authenticated_facts() {
    for (text, workbook, lock) in [
        ("", true, false),
        ("*.xlsx\n!~$local.xlsx\n!~$remote.xlsx", false, true),
        ("!~$*", true, true),
    ] {
        let dir = TestDir::new();
        let (db, remote, local) = fixture(&dir.0).await;
        if !text.is_empty() {
            BindingControls::new(db.clone())
                .change_rules(BINDING, "extra-1", text)
                .await
                .unwrap();
        }
        let fixed = remote.state().unwrap().entries;
        let import = runner(db.clone(), local, remote.clone());
        assert!(
            import
                .run(BINDING, &mut Ids(0), usize::MAX)
                .await
                .unwrap()
                .complete
        );
        let journal = import.store.load(BINDING).await.unwrap().unwrap();
        assert_eq!(
            journal.entries, fixed,
            "never filter snapshot pages or rewrite entry_count"
        );
        assert_eq!(journal.snapshot.entry_count, "3");
        let state = remote.state().unwrap();
        let uploaded: BTreeSet<_> = state
            .requests
            .iter()
            .map(|r| r.name.as_deref().unwrap())
            .collect();
        assert_eq!(uploaded.contains("local.xlsx"), workbook);
        assert_eq!(uploaded.contains("~$local.xlsx"), lock);
        assert_eq!(dir.0.join("local/remote.xlsx").exists(), workbook);
        assert_eq!(dir.0.join("local/~$remote.xlsx").exists(), lock);
        assert_eq!(
            fs::read(dir.0.join("local/~$local.xlsx")).unwrap(),
            b"local Office lock"
        );
        for row in &fixed {
            let mut conn = db.pool().acquire().await.unwrap();
            let id = entry_id(&row.entry_id).unwrap();
            assert!(
                entries::remote_entry(&mut conn, BINDING, id)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        let excluded: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT entry_id FROM binding_participation WHERE state='excluded'")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(excluded.contains(&vec![2; 16]), !workbook);
        assert_eq!(excluded.contains(&vec![3; 16]), !lock);
        assert!(
            fixed.iter().all(|row| state.entries.contains(row)),
            "no remote delete/overwrite"
        );
        drop(import);
        Arc::try_unwrap(db).unwrap().close().await;
    }
}

/// A disk-backed controlled-install test port, not a Windows/CFAPI acceptance substitute.
struct Materializer {
    db: Arc<Database>,
    local: Arc<DiskLocal>,
    remote: Arc<DiskRemote>,
}
impl FullMaterializer for Materializer {
    fn ensure_full<'a>(
        &'a self,
        file: &'a FullFile,
    ) -> PortFuture<'a, Result<LocalEntry, PortError>> {
        Box::pin(async move {
            let state = self.remote.state()?;
            let fixed = state
                .entries
                .iter()
                .find(|row| row.entry_id == file.entry.to_string())
                .unwrap();
            assert_eq!(
                fixed.version_id.as_deref(),
                file.version.map(|v| v.to_string()).as_deref()
            );
            if !self.local.vault().join(file.path.to_string()).exists() {
                self.local
                    .install(&ImportInstall {
                        op: OpId::from_bytes([0xfa; 16]),
                        binding: BINDING,
                        path: file.path.clone(),
                        remote: fixed.clone(),
                        mode: SyncMode::Full,
                    })
                    .await?;
            }
            let bytes = fs::read(self.local.vault().join(file.path.to_string())).map_err(io)?;
            assert_eq!(Some(hex(&digest_of(&bytes))), fixed.hash);
            let local = LocalEntry {
                entry: file.entry,
                generation: generation(1)?,
                dirty: Dirty::Clean,
                residency: Residency::Materialized,
                fingerprint: Some(digest_of(&bytes).to_vec()),
                file_identity: Some(vec![0xaa; 16]),
            };
            let mut tx = self.db.begin_write().await.map_err(io)?;
            entries::put_baseline(&mut tx, BINDING, &baseline(fixed)?)
                .await
                .map_err(io)?;
            entries::put_local(&mut tx, BINDING, &local)
                .await
                .map_err(io)?;
            tx.commit().await.map_err(io)?;
            Ok(local)
        })
    }
}

#[tokio::test]
async fn excluded_remote_survives_restart_and_reincludes_without_a_new_event() {
    let dir = TestDir::new();
    let (db, remote, local) = fixture(&dir.0).await;
    let import = runner(db.clone(), local.clone(), remote.clone());
    import.run(BINDING, &mut Ids(0), usize::MAX).await.unwrap();
    let before = remote.state().unwrap().entries;
    drop(import);
    Arc::try_unwrap(db).unwrap().close().await;
    let db = Arc::new(Database::open(dir.0.join("client.sqlite")).await.unwrap());
    let controls = BindingControls::new(db.clone());
    let id = EntryId::from_bytes([3; 16]);
    assert_eq!(
        controls.execution_gate(BINDING, Some(id)).await.unwrap(),
        ExecutionGate::Excluded
    );
    controls
        .change_rules(BINDING, "include-lock", "!~$*")
        .await
        .unwrap();
    assert_eq!(
        controls.execution_gate(BINDING, Some(id)).await.unwrap(),
        ExecutionGate::ReconcileRequired
    );
    assert!(
        controls
            .acknowledge_reconciled(BINDING, id, "old-version")
            .await
            .is_err()
    );
    let materializer = Materializer {
        db: db.clone(),
        local,
        remote: remote.clone(),
    };
    assert!(
        !controls
            .resume_mode(BINDING, &materializer, 10)
            .await
            .unwrap()
            .complete
    );
    assert!(
        !dir.0.join("local/~$remote.xlsx").exists(),
        "must reconcile before installing"
    );
    // No synthetic event: the complete retained remote namespace and current fixed snapshot agree.
    let mut conn = db.pool().acquire().await.unwrap();
    let retained = entries::remote_entry(&mut conn, BINDING, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retained.content_version.map(|v| v.to_string()),
        before
            .iter()
            .find(|r| r.entry_id == id.to_string())
            .unwrap()
            .version_id
    );
    assert!(
        entries::baseline_entry(&mut conn, BINDING, id)
            .await
            .unwrap()
            .is_none(),
        "excluded is not applied"
    );
    drop(conn);
    controls
        .acknowledge_reconciled(BINDING, id, "include-lock")
        .await
        .unwrap();
    assert!(
        controls
            .resume_mode(BINDING, &materializer, 10)
            .await
            .unwrap()
            .complete
    );
    assert_eq!(
        fs::read(dir.0.join("local/~$remote.xlsx")).unwrap(),
        b"remote Office lock"
    );
    assert_eq!(
        remote.state().unwrap().entries,
        before,
        "no new event or upload was needed"
    );
    let (received, applied): (i64, i64) =
        sqlx::query_as("SELECT received_seq,applied_seq FROM bindings")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!((received, applied), (7, 7));
    drop(materializer);
    drop(controls);
    Arc::try_unwrap(db).unwrap().close().await;
}

#[tokio::test]
async fn legacy_manifest_never_replays_excluded_upload_install_or_preserve_and_keeps_ack() {
    let dir = TestDir::new();
    let (db, remote, local) = fixture(&dir.0).await;
    write_file(
        &dir.0.join("local/~$remote.xlsx"),
        b"conflicting local lock",
    );
    let controls = BindingControls::new(db.clone());
    controls
        .change_rules(BINDING, "legacy-includes", "!~$*")
        .await
        .unwrap();
    let import = runner(db.clone(), local, remote.clone());
    import.run(BINDING, &mut Ids(0), 0).await.unwrap();
    let journal = import.store.load(BINDING).await.unwrap().unwrap();
    let acked = journal
        .items
        .as_ref()
        .unwrap()
        .iter()
        .find(|i| i.path == "~$local.xlsx")
        .unwrap();
    import
        .upload(&import.store.binding(BINDING).await.unwrap(), acked)
        .await
        .unwrap();
    let journal = import.store.load(BINDING).await.unwrap().unwrap();
    let receipt = journal
        .items
        .as_ref()
        .unwrap()
        .iter()
        .find(|i| i.path == "~$local.xlsx")
        .unwrap()
        .result
        .clone()
        .unwrap();
    let stable = entry_id(receipt.entry_id.as_ref().unwrap()).unwrap();
    let mut tx = db.begin_write().await.unwrap();
    let newer = LocalEntry {
        entry: stable,
        generation: generation(2).unwrap(),
        dirty: Dirty::AwaitingCommit,
        residency: Residency::Materialized,
        fingerprint: Some(vec![0x73; 32]),
        file_identity: Some(vec![0x45; 16]),
    };
    entries::put_local(&mut tx, BINDING, &newer).await.unwrap();
    tx.commit().await.unwrap();
    // Actual pre-S196 serialization had no exclusion field. Keep every OpId/temporary ID/ACK.
    let mut legacy = serde_json::to_value(&journal).unwrap();
    for item in legacy["items"].as_array_mut().unwrap() {
        item.as_object_mut().unwrap().remove("excluded");
    }
    sqlx::query("UPDATE binding_imports SET journal=?,revision=revision+1")
        .bind(legacy.to_string())
        .execute(db.pool())
        .await
        .unwrap();
    controls
        .change_rules(BINDING, "defaults-only", "")
        .await
        .unwrap();
    assert!(
        import
            .run(BINDING, &mut NoNewIds, usize::MAX)
            .await
            .unwrap()
            .complete
    );
    let recovered = import.store.load(BINDING).await.unwrap().unwrap();
    for original in journal
        .items
        .as_ref()
        .unwrap()
        .iter()
        .filter(|i| i.path.starts_with("~$"))
    {
        let item = recovered
            .items
            .as_ref()
            .unwrap()
            .iter()
            .find(|i| i.op == original.op)
            .unwrap();
        assert!(item.excluded && !item.complete);
        assert_eq!(item.temporary, original.temporary);
        assert_eq!(item.result, original.result);
        assert!(!item.preserved, "old conflicting lock must not be renamed");
        assert!(!dir.0.join("local").join(&item.target).exists() || item.target == item.source);
    }
    let mut conn = db.pool().acquire().await.unwrap();
    assert_eq!(
        entries::local_entry(&mut conn, BINDING, stable)
            .await
            .unwrap(),
        Some(newer)
    );
    drop(conn);
    let requests = remote.state().unwrap().requests;
    assert_eq!(
        requests.len(),
        2,
        "only the prior ACK and ordinary workbook upload"
    );
    assert_eq!(
        fs::read(dir.0.join("local/~$remote.xlsx")).unwrap(),
        b"conflicting local lock"
    );
    let pending: Vec<Vec<u8>> = sqlx::query_scalar("SELECT op_id FROM pending_ops")
        .fetch_all(db.pool())
        .await
        .unwrap();
    for original in journal
        .items
        .as_ref()
        .unwrap()
        .iter()
        .filter(|i| i.path.starts_with("~$") && i.result.is_none())
    {
        // The journal keeps the intent, identities, temporary row and retained bytes, so
        // re-inclusion replans instead of replaying. Only the worker-owned queue row is
        // dropped: it has no action specification and would fail every restore round.
        assert!(
            !pending.contains(&op_id(&original.op).unwrap().as_bytes().to_vec()),
            "first-import excluded upload must not stay in the worker queue"
        );
    }
    assert_eq!(
        pending.len(),
        1,
        "the ordinary workbook still owns its queued upload"
    );
    // Subsequent rule removal must not resurrect the stale first-import journal.
    controls
        .change_rules(BINDING, "reinclude", "!~$*")
        .await
        .unwrap();
    assert!(
        import
            .run(BINDING, &mut NoNewIds, usize::MAX)
            .await
            .unwrap()
            .complete
    );
    assert_eq!(remote.state().unwrap().requests, requests);
    assert_eq!(
        controls
            .execution_gate(BINDING, Some(stable))
            .await
            .unwrap(),
        ExecutionGate::ReconcileRequired
    );
    drop(import);
    drop(controls);
    Arc::try_unwrap(db).unwrap().close().await;
}

#[tokio::test]
async fn excluded_snapshot_rows_still_require_full_count_and_valid_identity() {
    for count_only in [false, true] {
        let dir = TestDir::new();
        let (db, remote, local) = fixture(&dir.0).await;
        let import = runner(db.clone(), local, remote.clone());
        if count_only {
            import.run(BINDING, &mut Ids(0), 0).await.unwrap();
            let mut journal = import.store.load(BINDING).await.unwrap().unwrap();
            journal.snapshot.entry_count = "4".to_owned();
            import.store.save(BINDING, &mut journal).await.unwrap();
        } else {
            let mut state = remote.state().unwrap();
            state.entries[2].version_id = None;
            remote.save(&state).unwrap();
        }
        assert!(import.run(BINDING, &mut Ids(0), usize::MAX).await.is_err());
        assert!(remote.state().unwrap().requests.is_empty());
        assert!(!dir.0.join("local/remote.xlsx").exists());
        drop(import);
        Arc::try_unwrap(db).unwrap().close().await;
    }
}

#[tokio::test]
async fn excluded_parent_blocks_child_negation_in_first_import() {
    let dir = TestDir::new();
    let (db, remote, local) = fixture(&dir.0).await;
    write_file(&dir.0.join("local/private/keep.xlsx"), b"private workbook");
    let mut state = remote.state().unwrap();
    let mut parent = state.entries[0].clone();
    parent.entry_id = EntryId::from_bytes([4; 16]).to_string();
    parent.parent_id = Some(VAULT_ENTRY.to_string());
    parent.name = "private".to_owned();
    let mut child = state.entries[1].clone();
    child.entry_id = EntryId::from_bytes([5; 16]).to_string();
    child.parent_id = Some(parent.entry_id.clone());
    child.name = "remote-keep.xlsx".to_owned();
    state.entries.extend([parent, child]);
    remote.save(&state).unwrap();
    BindingControls::new(db.clone())
        .change_rules(BINDING, "parent-rule", "private/\n!private/*.xlsx")
        .await
        .unwrap();
    let import = runner(db.clone(), local, remote.clone());
    assert!(
        import
            .run(BINDING, &mut Ids(0), usize::MAX)
            .await
            .unwrap()
            .complete
    );
    assert!(!dir.0.join("local/private/remote-keep.xlsx").exists());
    assert_eq!(
        fs::read(dir.0.join("local/private/keep.xlsx")).unwrap(),
        b"private workbook"
    );
    let journal = import.store.load(BINDING).await.unwrap().unwrap();
    assert_eq!(journal.entries.len(), 5);
    assert!(
        !journal
            .items
            .unwrap()
            .iter()
            .any(|i| i.path.starts_with("private"))
    );
    let excluded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM binding_participation WHERE state='excluded'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        excluded, 3,
        "remote lock, excluded parent and its child identities"
    );
    drop(import);
    Arc::try_unwrap(db).unwrap().close().await;
}
