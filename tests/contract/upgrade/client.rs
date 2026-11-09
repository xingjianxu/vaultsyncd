//! S091固定client11开发profile，当前迁移入口在真实SQLite/文件上演练；Windows必须原生再跑。
use super::*;
#[path = "support.rs"]
mod support;
/// 状态快照查询；`vault_id` 列在 S140 的迁移 0013 才由 `root_id` 改名，旧库快照必须用旧列名。
fn queries(column: &str) -> Vec<String> {
    vec![
        format!("SELECT json_group_array(json_array(hex(id),hex(server_id),hex(device_id),hex({column}),local_path,cache_path,mode,rules_version,server_epoch)) FROM bindings"),
        "SELECT json_group_array(json_array(hex(entry_id),generation,dirty,residency,hex(fingerprint),hex(file_identity))) FROM local_entries".to_owned(),
        "SELECT json_group_array(json_array(hex(op_id),kind,attempts,last_error_code,hex(pinned_version_id))) FROM pending_ops".to_owned(),
        "SELECT json_group_array(json_array(hex(op_id),priority,due_at,cancelled,halted,offline)) FROM operation_schedule".to_owned(),
        "SELECT json_group_array(json_array(hex(version_id),hex(digest),size)) FROM agent_versions".to_owned(),
        "SELECT json_group_array(json_array(dedupe_key,code,state,is_read,occurrences)) FROM notifications".to_owned(),
    ]
}

/// 当前 schema 的列名。
fn column(version: i64) -> &'static str {
    if version >= 13 { "vault_id" } else { "root_id" }
}
#[tokio::test]
async fn s091_client_upgrade_failure_and_complete_state_rollback() {
    let run = support::Run::new("client");
    let path = run.path.join("client.sqlite");
    let work = run.path.join("work");
    let cache = run.path.join("cache");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(&cache).unwrap();
    let dirty = b"S091 unacknowledged local bytes";
    let cached = b"S091 retained complete cache bytes";
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(cached);
    std::fs::write(work.join("dirty"), dirty).unwrap();
    std::fs::write(cache.join("complete"), cached).unwrap();
    let previous = support::previous("client");
    let old_version = target_version_of(&previous);
    assert_eq!(old_version, 11);
    let old = Database::open_with(&path, &previous).await.unwrap();
    let binding = crate::test_support::seed_binding_at_v1(&old, 0x91).await;
    sqlx::query("UPDATE bindings SET local_path=?,cache_path=?")
        .bind(work.to_string_lossy().as_ref())
        .bind(cache.to_string_lossy().as_ref())
        .execute(old.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO local_entries(binding_id,entry_id,generation,dirty,residency,fingerprint,file_identity) VALUES(?,?,41,'awaiting_commit','materialized',?,?)").bind(binding.as_bytes().as_slice()).bind([0x92u8;16].as_slice()).bind(b"fixed-full-fingerprint".as_slice()).bind(b"fixed-file-identity".as_slice()).execute(old.pool()).await.unwrap();
    sqlx::query("INSERT INTO pending_ops(op_id,binding_id,kind,entry_id,pinned_version_id,attempts,last_error_code) VALUES(?,?,'upload_content',?,?,3,'NETWORK_UNAVAILABLE')").bind([0x93u8;16].as_slice()).bind(binding.as_bytes().as_slice()).bind([0x92u8;16].as_slice()).bind([0x94u8;16].as_slice()).execute(old.pool()).await.unwrap();
    sqlx::query("INSERT INTO operation_schedule(op_id,priority,due_at,cancelled,halted,offline) VALUES(?,2,1234,1,0,1)").bind([0x93u8;16].as_slice()).execute(old.pool()).await.unwrap();
    sqlx::query(
        "INSERT INTO agent_versions(binding_id,entry_id,version_id,digest,size) VALUES(?,?,?,?,?)",
    )
    .bind(binding.as_bytes().as_slice())
    .bind([0x92u8; 16].as_slice())
    .bind([0x94u8; 16].as_slice())
    .bind(digest.as_slice())
    .bind(cached.len() as i64)
    .execute(old.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO notifications(dedupe_key,binding_id,code,state,is_read,occurrences) VALUES('s091-notice',?,'VAULT_UNAVAILABLE','active',1,5)").bind(binding.as_bytes().as_slice()).execute(old.pool()).await.unwrap();
    old.close().await;
    let before = support::snapshot(&path, &queries(column(11))).await;
    let broken = support::fault_profile(
        &previous,
        &run.path,
        "broken",
        &[(
            12,
            "CREATE TABLE s091_half(value TEXT);INSERT INTO s091_missing VALUES(1);",
        )],
    )
    .await;
    let error = Database::open_with(&path, &broken).await.unwrap_err();
    let backup = match error {
        DbError::Migration {
            backup: Some(p), ..
        } => p,
        other => panic!("must pause before any writable DB: {other}"),
    };
    assert_eq!(
        support::scalar::<i64>(&path, "PRAGMA user_version").await,
        11
    );
    assert_eq!(
        support::snapshot(
            &path,
            &queries(column(
                support::scalar::<i64>(&path, "PRAGMA user_version").await
            ))
        )
        .await,
        before
    );
    assert_eq!(
        support::snapshot(&backup, &queries(column(11))).await,
        before
    );
    assert_eq!(
        support::scalar::<i64>(
            &path,
            "SELECT count(*) FROM sqlite_master WHERE name='s091_half'"
        )
        .await,
        0
    );
    let success = Database::open(&path).await.unwrap();
    assert_eq!(success.schema_version(), target_schema_version());
    success.close().await;
    assert_eq!(
        support::snapshot(
            &path,
            &queries(column(
                support::scalar::<i64>(&path, "PRAGMA user_version").await
            ))
        )
        .await,
        before
    );
    let image = std::fs::read(&path).unwrap();
    assert!(matches!(
        Database::open_with(&path, &previous).await.unwrap_err(),
        DbError::SchemaTooNew { supported: 11, .. }
    ));
    assert_eq!(std::fs::read(&path).unwrap(), image);
    // 多步迁移部分成功后连当前程序也必须暂停，不能用较低user_version绕过迁移记账。
    let target = target_schema_version();
    let partial = support::fault_profile(
        &MIGRATOR,
        &run.path,
        "partial",
        &[
            (target + 1, "CREATE TABLE s091_future(value TEXT);"),
            (target + 2, "CREATE TABLE s091_invalid ("),
        ],
    )
    .await;
    assert!(matches!(
        Database::open_with(&path, &partial).await.unwrap_err(),
        DbError::Migration {
            backup: Some(_),
            ..
        }
    ));
    assert!(matches!(
        Database::open(&path).await.unwrap_err(),
        DbError::SchemaTooNew { .. }
    ));
    assert_eq!(
        support::snapshot(
            &path,
            &queries(column(
                support::scalar::<i64>(&path, "PRAGMA user_version").await
            ))
        )
        .await,
        before
    );
    // 回退用迁移前DB加同一受控停止区间的工作/缓存整体备份，不运行down SQL、不注销绑定。
    let full = run.path.join("full-before");
    std::fs::create_dir(&full).unwrap();
    support::copy_file(&backup, &full.join("client.sqlite"));
    support::copy_file(&work.join("dirty"), &full.join("dirty"));
    support::copy_file(&cache.join("complete"), &full.join("complete"));
    std::fs::rename(&path, run.path.join("upgraded-retained.sqlite")).unwrap();
    support::copy_file(&full.join("client.sqlite"), &path);
    let recovered = Database::open_with(&path, &previous).await.unwrap();
    assert_eq!(recovered.schema_version(), 11);
    recovered.close().await;
    assert_eq!(
        support::snapshot(
            &path,
            &queries(column(
                support::scalar::<i64>(&path, "PRAGMA user_version").await
            ))
        )
        .await,
        before
    );
    assert_eq!(std::fs::read(work.join("dirty")).unwrap(), dirty);
    assert_eq!(std::fs::read(cache.join("complete")).unwrap(), cached);
    assert_eq!(std::fs::read(full.join("dirty")).unwrap(), dirty);
    assert_eq!(std::fs::read(full.join("complete")).unwrap(), cached);
    println!(
        "S091 client platform={} fixture_schema=11 target_schema={target} dirty_generation=41 queue_attempts=3 pinned_version=retained notice=retained full_cache=retained migration_failure=paused newer_schema=refused full_rollback=verified",
        std::env::consts::OS
    );
}
