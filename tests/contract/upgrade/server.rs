//! S091固定server12历史profile；生产数据库入口迁移/失败/拒写，与原生整套回退actor。
use super::*;
#[path = "support.rs"]
mod support;
const QUERIES: &[&str] = &[
    "SELECT json_group_array(json_array(hex(id),login,status)) FROM users",
    "SELECT json_group_array(json_array(hex(id),hex(owner_id),absolute_path,seq,epoch)) FROM roots",
    "SELECT json_group_array(json_array(hex(id),name,revision,hex(content_version_id),deleted_at)) FROM entries",
    "SELECT json_group_array(json_array(hex(id),hex(object_hash),size,created_seq,superseded_at,origin)) FROM versions",
    "SELECT json_group_array(json_array(hex(hash),size,storage_path,verified_at,gc_state)) FROM objects",
];
#[tokio::test]
async fn s091_server_upgrade_preserves_history_and_failure_backup() {
    let run = support::Run::new("server");
    let path = run.path.join("server.sqlite");
    let previous = support::previous("server");
    assert_eq!(target_version_of(&previous), 12);
    let old = Database::open_with(&path, &previous).await.unwrap();
    let owner = crate::test_support::seed_owner(&old, 0x91).await;
    let (root, parent) = crate::test_support::register_root(&old, owner, 0x92).await;
    let entry = crate::test_support::entry(0x93);
    let mut tx = old.begin_write().await.unwrap();
    // 固定 profile 是 v12 schema：它还没有 `entries.modified_at`（S130 才加），所以历史装置按旧
    // schema 直接写行，不用当前写入口——用新写入口会要求旧库不存在的列。
    sqlx::query(
        "INSERT INTO entries \
         (id, root_id, parent_id, name, name_key, kind, revision, content_version_id, deleted_at) \
         VALUES (?1, ?2, ?3, 'history', 'history', 'file', 0, NULL, NULL)",
    )
    .bind(entry.as_bytes().as_slice())
    .bind(root.as_bytes().as_slice())
    .bind(parent.as_bytes().as_slice())
    .execute(&mut *tx)
    .await
    .unwrap();
    for marker in [0x94u8, 0x95] {
        crate::changes::append_change(
            &mut tx,
            owner,
            root,
            crate::test_support::tx(marker),
            "[{}]",
            crate::test_support::at(10),
        )
        .await
        .unwrap();
        let seq = crate::roots::root_in_scope(&mut tx, owner, root)
            .await
            .unwrap()
            .unwrap()
            .seq;
        sqlx::query("INSERT INTO objects(root_id,hash,size,storage_path,verified_at,gc_state) VALUES(?,?,7,?,10,'live')").bind(root.as_bytes().as_slice()).bind([marker;32].as_slice()).bind(format!("objects/{marker:02x}/fixture")).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO versions(id,root_id,entry_id,object_hash,size,created_seq,superseded_at,origin) VALUES(?,?,?,?,7,?,?,'external')").bind([marker;16].as_slice()).bind(root.as_bytes().as_slice()).bind(entry.as_bytes().as_slice()).bind([marker;32].as_slice()).bind(seq.to_db_integer()).bind(if marker==0x94{Some(20i64)}else{None}).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("UPDATE entries SET content_version_id=?,revision=2 WHERE id=?")
        .bind([0x95u8; 16].as_slice())
        .bind(entry.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    old.close().await;
    let before = support::snapshot(&path, QUERIES).await;
    let broken = support::fault_profile(
        &previous,
        &run.path,
        "broken",
        &[(
            13,
            "CREATE TABLE s091_half(value TEXT);INSERT INTO s091_missing VALUES(1);",
        )],
    )
    .await;
    let backup = match Database::open_with(&path, &broken).await.unwrap_err() {
        DbError::Migration {
            backup: Some(p), ..
        } => p,
        e => panic!("must pause without writable state: {e}"),
    };
    assert_eq!(support::snapshot(&path, QUERIES).await, before);
    assert_eq!(support::snapshot(&backup, QUERIES).await, before);
    assert_eq!(
        support::scalar::<i64>(&path, "PRAGMA user_version").await,
        12
    );
    let current = Database::open(&path).await.unwrap();
    current.close().await;
    assert_eq!(support::snapshot(&path, QUERIES).await, before);
    let bytes = std::fs::read(&path).unwrap();
    assert!(matches!(
        Database::open_with(&path, &previous).await.unwrap_err(),
        DbError::SchemaTooNew { supported: 12, .. }
    ));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let retained = run.path.join("prior-complete.sqlite");
    support::copy_file(&backup, &retained);
    assert_eq!(support::snapshot(&retained, QUERIES).await, before);
    println!(
        "S091 server fixture_schema=12 target_schema={} history_versions=2 objects=2 migration_failure=paused original_data=retained old_schema_writer=refused backup=verified",
        target_schema_version()
    );
}

#[tokio::test]
#[ignore = "S091 native runner explicitly supplies an isolated stopped state and shared StateLock"]
async fn s091_server_upgrade_actor() {
    let state = PathBuf::from(
        std::env::var_os("VAULTSYNCD_S091_ACTOR_STATE").expect("explicit stopped fixture state"),
    );
    assert!(state.starts_with("/data/vaultsyncd-lab/runs/"));
    assert!(
        state
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with("s091-"))
    );
    let _lock = crate::state_lock::StateLock::try_acquire_exclusive(&state).unwrap();
    let path = state.join("server.sqlite");
    let db = Database::open(&path).await.unwrap();
    db.close().await;
    let target = target_schema_version();
    let run = support::Run::new("server-actor");
    let partial = support::fault_profile(
        &MIGRATOR,
        &run.path,
        "future",
        &[
            (target + 1, "CREATE TABLE s091_future(value TEXT);"),
            (target + 2, "CREATE TABLE s091_invalid ("),
        ],
    )
    .await;
    let error = Database::open_with(&path, &partial).await.unwrap_err();
    assert!(matches!(
        error,
        DbError::Migration {
            backup: Some(_),
            ..
        }
    ));
    assert!(
        matches!(Database::open(&path).await.unwrap_err(),DbError::SchemaTooNew{found,..}if found==target+1)
    );
    println!(
        "S091 synthetic future migration stopped at {} before writable service; actual DB entry refuses write; use full consistent archive to restore",
        target + 1
    );
}
