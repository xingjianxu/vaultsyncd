//! 真实 SQLite 事务的观察路径围栏：父目录改名不推进子文件 revision。
use super::*;
use vaultsyncd_domain::version::ServerEpoch;
use vaultsyncd_server_store::entries::EntryKind as StoreKind;

#[tokio::test]
async fn stale_observed_paths_cannot_move_or_delete_a_file_after_parent_rename() {
    let path =
        std::env::temp_dir().join(format!("vaultsyncd-s137-observer-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    let database = Database::open(path.join("server.sqlite")).await.unwrap();
    let owner = UserId::from_bytes([1; 16]);
    let vault = VaultId::from_bytes([2; 16]);
    let vault_entry = EntryId::from_bytes([3; 16]);
    let parent = EntryId::from_bytes([4; 16]);
    let file = EntryId::from_bytes([5; 16]);
    let at = UtcTimestamp::from_unix_seconds(1_700_000_000).unwrap();
    let revision = Revision::new(1).unwrap();
    let mut tx = database.begin_write().await.unwrap();
    sqlx::query("INSERT INTO users(id,login,password_hash,status) VALUES(?,'observer-test','test-only','active')")
        .bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
    store_vaults::insert_vault(
        &mut tx,
        &store_vaults::NewVault {
            id: vault,
            entry: vault_entry,
            owner,
            absolute_path: path.join("work").display().to_string(),
            volume_identity: vec![1],
            vault_file_identity: vec![2],
            epoch: ServerEpoch::ZERO,
        },
    )
    .await
    .unwrap();
    for (id, parent, name, kind) in [
        (parent, vault_entry, "before", StoreKind::Directory),
        (file, parent, "note.txt", StoreKind::File),
    ] {
        store_entries::insert_entry(
            &mut tx,
            owner,
            &NewEntry {
                id,
                vault,
                parent,
                name: name.into(),
                name_key: name.into(),
                kind,
                revision,
                content_version: None,
            },
            at,
        )
        .await
        .unwrap();
    }
    // 真实 API namespace 提交使用的同一存储函数；子文件修订号未变。
    store_entries::move_entry(
        &mut tx,
        owner,
        parent,
        vault_entry,
        "after",
        "after",
        revision.next().unwrap(),
        at,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut store = SqlExternalStore::new(database.clone(), owner);
    let deletion = ExternalDelete {
        vault,
        path: "before/note.txt".into(),
        entry: file,
        expected_revision: revision,
        at,
    };
    assert!(matches!(
        store.apply_external_delete(&deletion).await,
        Err(ExternalStoreError::PathConflict { .. })
    ));
    let movement = ExternalMove {
        vault,
        from: "before/note.txt".into(),
        to: "moved.txt".into(),
        entry: file,
        expected_revision: revision,
        at,
    };
    assert!(matches!(
        store.apply_external_move(&movement).await,
        Err(ExternalStoreError::PathConflict { .. })
    ));
    let mut conn = database.pool().acquire().await.unwrap();
    let preserved = store_entries::entry_by_path(&mut conn, owner, vault, "after/note.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preserved.id, file);
    assert_eq!(preserved.revision, revision);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tombstones")
            .fetch_one(&mut *conn)
            .await
            .unwrap(),
        0
    );
    drop(conn);
    // 当前路径仍可正常移动与删除；围栏不以“禁用观察器”掩盖缺陷。
    let moved = store
        .apply_external_move(&ExternalMove {
            from: "after/note.txt".into(),
            ..movement
        })
        .await
        .unwrap();
    let deleted = store
        .apply_external_delete(&ExternalDelete {
            path: "moved.txt".into(),
            expected_revision: moved.revision,
            ..deletion
        })
        .await
        .unwrap();
    assert_eq!(deleted.entry, file);
    database.close().await;
    std::fs::remove_dir_all(path).unwrap();
}
