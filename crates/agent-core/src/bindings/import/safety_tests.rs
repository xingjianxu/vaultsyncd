//! 首次导入的并发 CAS、坏快照与冲突 ACK 围栏。
use super::*;

/// 测试专用事件格式明确为 SnapshotEntry 数组；不猜测并行 S052 的业务事件格式。
pub(super) struct DiskCatchUp(pub PathBuf);
impl ChangeSource for DiskCatchUp {
    fn pull(
        &self,
        root: RootId,
        after: RootSeq,
        _: u32,
    ) -> PortFuture<'_, Result<ChangeList, PortError>> {
        Box::pin(async move {
            assert_eq!(after.get(), 7, "必须先接原快照之后的全部事件");
            let state: RemoteDisk = read_json(&self.0.join("server.json"))?;
            let mut batches = Vec::new();
            for result in state.results.values() {
                let seq = result.root_seq.as_ref().expect("提交序号");
                let entry = state
                    .entries
                    .iter()
                    .find(|entry| Some(&entry.entry_id) == result.entry_id.as_ref())
                    .expect("提交条目");
                batches.push(vaultsyncd_protocol::generated::ChangeBatch {
                    created_at: "1899990001".to_owned(),
                    events_json: serde_json::to_string(&vec![entry]).map_err(io)?,
                    seq: seq.clone(),
                    tx_id: result.op_id.clone(),
                });
            }
            batches.sort_by_key(|batch| number(&batch.seq).expect("序号"));
            Ok(ChangeList {
                batches,
                has_more: false,
                head_seq: state.seq.to_string(),
                next_after: state.seq.to_string(),
                root_id: root.to_string(),
                server_epoch: "3".to_owned(),
            })
        })
    }
}
impl EventDecoder for DiskCatchUp {
    fn decode(
        &self,
        binding: BindingId,
        envelope: &EventEnvelope,
    ) -> Result<events::IncomingBatch, DecodeError> {
        let entries: Vec<SnapshotEntry> = serde_json::from_str(&envelope.events_json)
            .map_err(|error| DecodeError::new("events_json", error.to_string()))?;
        let changes = entries
            .iter()
            .map(|entry| {
                let base = baseline(entry)
                    .map_err(|error| DecodeError::new("entry", error.to_string()))?;
                Ok(vaultsyncd_client_store::entries::RemoteEntry {
                    entry: base.entry,
                    parent: base.parent,
                    name: base.name,
                    kind: base.kind,
                    revision: base.revision,
                    content_version: base.content_version,
                    deleted: false,
                })
            })
            .collect::<Result<Vec<_>, DecodeError>>()?;
        Ok(events::IncomingBatch {
            binding,
            seq: envelope.seq,
            tx: envelope.tx,
            events_json: envelope.events_json.clone(),
            changes,
            tasks: vec![],
        })
    }
}
impl EventApplier for DiskCatchUp {
    fn apply(
        &self,
        _: BindingId,
        changes: &[vaultsyncd_client_store::entries::RemoteEntry],
        tasks: &[vaultsyncd_client_store::queue::NewPendingOp],
    ) -> PortFuture<'_, Result<ApplyResult, PortError>> {
        let changes = changes.to_vec();
        assert!(tasks.is_empty());
        Box::pin(async move {
            let state: RemoteDisk = read_json(&self.0.join("server.json"))?;
            let paths = remote_paths(&state.entries)?;
            for change in changes {
                let (path, entry) = paths
                    .iter()
                    .find(|(_, entry)| entry.entry_id == change.entry.to_string())
                    .expect("事件条目位置");
                let target = self.0.join("local").join(path);
                if let Some(hash) = &entry.hash {
                    assert_eq!(&hex(&digest_of(&std::fs::read(target).map_err(io)?)), hash);
                } else {
                    assert!(target.is_dir());
                }
            }
            Ok(ApplyResult::Applied)
        })
    }
}
use vaultsyncd_protocol::generated::{ConflictReason, OperationConflict, OperationState};

#[tokio::test]
async fn concurrent_remote_creation_fails_cas_without_overwrite_or_cursor_advance() {
    let dir = TestDir::new();
    let database = setup(&dir.0).await;
    let remote = Arc::new(DiskRemote {
        base: dir.0.clone(),
        fault: String::new(),
    });
    let import = runner(
        database.clone(),
        Arc::new(DiskLocal::new(&dir.0, "")),
        remote.clone(),
    );
    import.run(BINDING, &mut Ids(0), 0).await.expect("固定清单");
    let mut state = remote.state().expect("远端");
    let version = EntryId::from_bytes([0xfb; 16]).to_string();
    write_file(
        &dir.0.join("content").join(&version),
        b"concurrent remote creation",
    );
    state.entries.push(SnapshotEntry {
        entry_id: EntryId::from_bytes([0xfa; 16]).to_string(),
        hash: Some(hex(&digest_of(b"concurrent remote creation"))),
        kind: WireKind::File,
        name: "a-local.txt".to_owned(),
        parent_id: Some(ROOT_ENTRY.to_string()),
        revision: "1".to_owned(),
        size: Some("26".to_owned()),
        version_id: Some(version.clone()),
    });
    remote.save(&state).expect("并发创建");
    let error = import
        .run(BINDING, &mut NoNewIds, usize::MAX)
        .await
        .expect_err("旧快照不能覆盖新条目");
    assert_eq!(error.code(), Some("BASE_MISMATCH"));
    assert_eq!(
        std::fs::read(dir.0.join("local/a-local.txt")).expect("本地内容"),
        b"local only"
    );
    assert_eq!(
        std::fs::read(dir.0.join("content").join(version)).expect("服务器新内容"),
        b"concurrent remote creation"
    );
    assert!(remote.state().expect("远端").requests.is_empty());
    let (received, applied): (i64, i64) =
        sqlx::query_as("SELECT received_seq,applied_seq FROM bindings")
            .fetch_one(database.pool())
            .await
            .expect("游标");
    assert_eq!((received, applied), (0, 0));
    assert!(
        !import
            .store
            .load(BINDING)
            .await
            .expect("日志")
            .expect("日志")
            .complete
    );
    drop(import);
    Arc::try_unwrap(database).expect("独占库").close().await;
}

#[tokio::test]
async fn conflict_ack_keeps_both_stable_ids_and_dirty_state_in_one_durable_transaction() {
    let dir = TestDir::new();
    let database = setup(&dir.0).await;
    let import = runner(
        database.clone(),
        Arc::new(DiskLocal::new(&dir.0, "")),
        Arc::new(DiskRemote {
            base: dir.0.clone(),
            fault: String::new(),
        }),
    );
    import.run(BINDING, &mut Ids(0), 0).await.expect("清单");
    let journal = import
        .store
        .load(BINDING)
        .await
        .expect("日志")
        .expect("日志");
    let item = journal
        .items
        .as_ref()
        .expect("清单")
        .iter()
        .find(|item| item.path == "a-local.txt")
        .expect("项")
        .clone();
    let primary = EntryId::from_bytes([0xf0; 16]);
    let sibling = EntryId::from_bytes([0xf1; 16]);
    let result = OperationResult {
        conflict: Some(OperationConflict {
            conflict_entry_id: sibling.to_string(),
            entry_id: primary.to_string(),
            reason: ConflictReason::StaleBase,
        }),
        echo_local_generation: Some("1".to_owned()),
        entry_id: Some(primary.to_string()),
        error_code: None,
        error_message: None,
        op_id: item.op.clone(),
        replayed: Some(false),
        revision: Some("2".to_owned()),
        root_seq: Some("8".to_owned()),
        state: OperationState::Committed,
        version_id: Some(EntryId::from_bytes([0xf2; 16]).to_string()),
    };
    assert!(
        import
            .store
            .ack(BINDING, op_id(&item.op).expect("操作"), &result)
            .await
            .is_err(),
        "不能猜冲突副本最终路径"
    );
    assert!(
        import
            .store
            .ack(BINDING, op_id(&item.op).expect("操作"), &result)
            .await
            .is_err(),
        "重放也不能把未定位冲突当成功"
    );
    drop(import);
    Arc::try_unwrap(database).expect("独占库").close().await;
    let database = Arc::new(
        Database::open(dir.0.join("client.sqlite"))
            .await
            .expect("重启数据库"),
    );
    let store = ImportStore::new(database.clone());
    let journal = store.load(BINDING).await.expect("日志").expect("日志");
    assert_eq!(
        journal
            .items
            .as_ref()
            .expect("项")
            .iter()
            .find(|row| row.op == item.op)
            .expect("项")
            .result,
        Some(result.clone())
    );
    let (mapped, raw): (Vec<u8>, String) =
        sqlx::query_as("SELECT entry_id,result_json FROM import_id_map")
            .fetch_one(database.pool())
            .await
            .expect("身份映射");
    assert_eq!(mapped, sibling.as_bytes().as_slice());
    assert_eq!(
        serde_json::from_str::<OperationResult>(&raw).expect("原回执"),
        result
    );
    let mut conn = database.pool().acquire().await.expect("连接");
    let local = entries::local_entry(
        &mut conn,
        BINDING,
        entry_id(item.temporary.as_ref().expect("临时身份")).expect("身份"),
    )
    .await
    .expect("临时内容")
    .expect("未确认内容保留");
    assert_eq!(local.dirty, Dirty::Dirty);
    assert!(
        entries::baseline_entry(&mut conn, BINDING, primary)
            .await
            .expect("主条目基线")
            .is_none()
    );
    assert!(
        entries::baseline_entry(&mut conn, BINDING, sibling)
            .await
            .expect("副本基线")
            .is_none()
    );
    drop(conn);
    drop(store);
    Arc::try_unwrap(database).expect("独占库").close().await;
}

#[tokio::test]
async fn mixed_snapshot_pages_and_invalid_cursors_are_rejected_before_actions() {
    let dir = TestDir::new();
    let database = setup(&dir.0).await;
    let import = runner(
        database.clone(),
        Arc::new(DiskLocal::new(&dir.0, "")),
        Arc::new(DiskRemote {
            base: dir.0.clone(),
            fault: String::new(),
        }),
    );
    import.run(BINDING, &mut Ids(0), 0).await.expect("固定清单");
    let mut journal = import
        .store
        .load(BINDING)
        .await
        .expect("日志")
        .expect("日志");
    let next_entry = journal.entries[2].clone();
    journal.entries.truncate(2);
    journal.cursor = Some("2".to_owned());
    let mut page = SnapshotPage {
        entries: vec![next_entry],
        high_water_seq: "8".to_owned(),
        next_cursor: Some("3".to_owned()),
        snapshot_id: journal.snapshot.snapshot_id.clone(),
    };
    assert!(validate_page(&journal, &page).is_err(), "混入另一个高水位");
    page.high_water_seq = "7".to_owned();
    page.snapshot_id = EntryId::from_bytes([0xfe; 16]).to_string();
    assert!(validate_page(&journal, &page).is_err(), "混入另一个快照");
    page.snapshot_id = journal.snapshot.snapshot_id.clone();
    page.next_cursor = Some("1".to_owned());
    assert!(validate_page(&journal, &page).is_err(), "倒退游标");
    page.next_cursor = Some("3".to_owned());
    journal.cursor = Some("not-a-number".to_owned());
    assert!(
        validate_page(&journal, &page).is_err(),
        "坏日志游标不能默认为零"
    );
    drop(import);
    Arc::try_unwrap(database).expect("独占库").close().await;
}
