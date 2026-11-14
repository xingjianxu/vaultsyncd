use super::Database;
use crate::test_support::{TestDir, insert_user, user};

#[tokio::test]
async fn schema19_upgrade_does_not_invent_history_and_preserves_backup_and_old_binary_fence() {
    let dir = TestDir::new("activity-upgrade");
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::db::MIGRATOR
                .iter()
                .filter(|m| m.version <= 19)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    let old = Database::open_with(&dir.database(), &previous)
        .await
        .unwrap();
    let mut tx = old.begin_write().await.unwrap();
    insert_user(&mut tx, user(1)).await;
    sqlx::query("INSERT INTO client_connections(id,user_id,device_label,client_kind,created_at,login_ip) VALUES(?1,?2,'历史客户端','client',100,'192.0.2.19')")
        .bind([2u8;16].as_slice()).bind(user(1).as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    old.close().await;
    let new = Database::open(dir.database()).await.unwrap();
    assert_eq!(new.schema_version(), 22);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM file_activity")
            .fetch_one(new.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT login_ip FROM client_connections")
            .fetch_one(new.pool())
            .await
            .unwrap(),
        "192.0.2.19"
    );
    new.close().await;
    let before = std::fs::read(dir.database()).unwrap();
    assert!(matches!(
        Database::open_with(&dir.database(), &previous).await,
        Err(crate::db::DbError::SchemaTooNew {
            found: 22,
            supported: 19
        })
    ));
    assert_eq!(std::fs::read(dir.database()).unwrap(), before);
    let backup = crate::db::migration_backup_path(&dir.database(), 19, 22);
    let restored = dir.path.join("restored.sqlite");
    std::fs::copy(backup, &restored).unwrap();
    let recovered = Database::open_with(&restored, &previous).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT device_label FROM client_connections")
            .fetch_one(recovered.pool())
            .await
            .unwrap(),
        "历史客户端"
    );
    recovered.close().await;
    let upgraded = Database::open(restored).await.unwrap();
    assert_eq!(upgraded.schema_version(), 22);
}

#[tokio::test]
async fn schema20_host_properties_migration_preserves_unknown_history_backup_and_fence() {
    let dir = TestDir::new("host-properties-upgrade");
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::db::MIGRATOR
                .iter()
                .filter(|m| m.version <= 20)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    let old = Database::open_with(&dir.database(), &previous)
        .await
        .unwrap();
    let mut tx = old.begin_write().await.unwrap();
    insert_user(&mut tx, user(1)).await;
    sqlx::query("INSERT INTO client_connections(id,user_id,device_label,client_kind,created_at,login_ip) VALUES(?1,?2,'Windows agent','client',100,'192.0.2.197')")
        .bind([2u8;16].as_slice()).bind(user(1).as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    old.close().await;
    let current = Database::open(dir.database()).await.unwrap();
    let attributes: (Option<String>, Option<String>, String, String) = sqlx::query_as(
        "SELECT hostname,operating_system,login_ip,device_label FROM client_connections",
    )
    .fetch_one(current.pool())
    .await
    .unwrap();
    assert_eq!(
        attributes,
        (None, None, "192.0.2.197".into(), "Windows agent".into())
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM file_activity")
            .fetch_one(current.pool())
            .await
            .unwrap(),
        0
    );
    current.close().await;
    let bytes = std::fs::read(dir.database()).unwrap();
    assert!(matches!(
        Database::open_with(&dir.database(), &previous).await,
        Err(crate::db::DbError::SchemaTooNew {
            found: 22,
            supported: 20
        })
    ));
    assert!(
        std::fs::read(dir.database()).unwrap() == bytes,
        "旧二进制准入不得修改已checkpoint数据库"
    );
    let restored = dir.path.join("schema20-restored.sqlite");
    std::fs::copy(
        crate::db::migration_backup_path(&dir.database(), 20, 22),
        &restored,
    )
    .unwrap();
    let recovered = Database::open_with(&restored, &previous).await.unwrap();
    assert_eq!(recovered.schema_version(), 20);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT login_ip FROM client_connections")
            .fetch_one(recovered.pool())
            .await
            .unwrap(),
        "192.0.2.197"
    );
    recovered.close().await;
    let upgraded = Database::open(restored).await.unwrap();
    assert_eq!(upgraded.schema_version(), 22);
    upgraded.close().await;
}

#[tokio::test]
async fn schema21_operation_activity_preserves_rows_pagination_sequence_and_recoverable_backup() {
    use crate::test_support::{register_vault, seed_owner};
    let dir = TestDir::new("operation-activity-upgrade");
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::db::MIGRATOR
                .iter()
                .filter(|m| m.version <= 21)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    let old = Database::open_with(&dir.database(), &previous)
        .await
        .unwrap();
    let owner = seed_owner(&old, 1).await;
    let (vault, _) = register_vault(&old, owner, 2).await;
    let mut tx = old.begin_write().await.unwrap();
    for (id, action, direction, status) in [
        (7, "upload_finalize", "upload", "ready"),
        (8, "sync_commit", "upload", "committed"),
        (9, "download", "download", "served"),
    ] {
        sqlx::query("INSERT INTO file_activity(id,vault_id,occurred_at,client_id,client_label,client_kind,device_id,direction,action,status,entry_id,version_id,path,size,bytes,offset,operation_id,upload_id,error_code) VALUES(?1,?2,100,?3,'历史设备','client',?3,?4,?5,?6,'条目','版本','原目录/文档.txt',123,100,23,?3,?3,NULL)")
            .bind(id).bind(vault.as_bytes().as_slice()).bind([3u8;16].as_slice()).bind(direction).bind(action).bind(status).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("DELETE FROM file_activity WHERE id=9")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let snapshots = "SELECT json_group_array(json_array(id,hex(vault_id),occurred_at,hex(client_id),client_label,client_kind,hex(device_id),direction,action,status,entry_id,version_id,path,size,bytes,offset,hex(operation_id),hex(upload_id),error_code)) FROM (SELECT * FROM file_activity ORDER BY id)";
    let before: String = sqlx::query_scalar(snapshots)
        .fetch_one(old.pool())
        .await
        .unwrap();
    old.close().await;
    let current = Database::open(dir.database()).await.unwrap();
    assert_eq!(current.schema_version(), 22);
    let after: String = sqlx::query_scalar(snapshots)
        .fetch_one(current.pool())
        .await
        .unwrap();
    assert_eq!(after, before);
    let indexes: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='index' AND name IN ('file_activity_page','file_activity_direction_page','file_activity_finalized','file_activity_committed')").fetch_one(current.pool()).await.unwrap();
    assert_eq!(indexes, 4);
    let mut conn = current.pool().acquire().await.unwrap();
    let rows = crate::activity::list(&mut conn, owner, vault, Some(8), None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, 7);
    let inserted = sqlx::query("INSERT INTO file_activity(vault_id,occurred_at,client_label,client_kind,direction,action,status,entry_id,path) VALUES(?1,101,'新设备','unknown','operation','delete','committed','删除条目','原目录/文档.txt')")
        .bind(vault.as_bytes().as_slice()).execute(&mut *conn).await.unwrap();
    assert_eq!(inserted.last_insert_rowid(), 10, "删除过的历史ID也不能复用");
    // Rebuilt CHECKs and partial unique indexes still enforce evidence semantics.
    assert!(sqlx::query("INSERT INTO file_activity(vault_id,occurred_at,client_label,client_kind,direction,action,status) VALUES(?1,101,'错误','unknown','upload','delete','committed')").bind(vault.as_bytes().as_slice()).execute(&mut *conn).await.is_err());
    assert!(sqlx::query("INSERT INTO file_activity(vault_id,occurred_at,client_label,client_kind,direction,action,status,upload_id) VALUES(?1,101,'重复','unknown','upload','upload_finalize','ready',?2)").bind(vault.as_bytes().as_slice()).bind([3u8;16].as_slice()).execute(&mut *conn).await.is_err());
    assert!(sqlx::query("INSERT INTO file_activity(vault_id,occurred_at,client_label,client_kind,device_id,direction,action,status,entry_id,operation_id) VALUES(?1,101,'重复','unknown',?2,'operation','delete','committed','条目',?2)").bind(vault.as_bytes().as_slice()).bind([3u8;16].as_slice()).execute(&mut *conn).await.is_err());
    // 完成已授权写入的checkpoint，再比较旧二进制准入前后的静态文件。
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&mut *conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    current.close().await;
    let bytes = std::fs::read(dir.database()).unwrap();
    assert!(matches!(
        Database::open_with(&dir.database(), &previous).await,
        Err(crate::db::DbError::SchemaTooNew {
            found: 22,
            supported: 21
        })
    ));
    assert!(
        std::fs::read(dir.database()).unwrap() == bytes,
        "旧二进制准入不得修改已checkpoint数据库"
    );
    let restored = dir.path.join("schema21-restored.sqlite");
    std::fs::copy(
        crate::db::migration_backup_path(&dir.database(), 21, 22),
        &restored,
    )
    .unwrap();
    let recovered = Database::open_with(&restored, &previous).await.unwrap();
    assert_eq!(recovered.schema_version(), 21);
    let saved: String = sqlx::query_scalar(snapshots)
        .fetch_one(recovered.pool())
        .await
        .unwrap();
    assert_eq!(saved, before);
    recovered.close().await;
    let upgraded = Database::open(restored).await.unwrap();
    assert_eq!(upgraded.schema_version(), 22);
    let saved: String = sqlx::query_scalar(snapshots)
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
    assert_eq!(saved, before);
    upgraded.close().await;
}

#[tokio::test]
async fn empty_activity_migration_keeps_deleted_identity_high_watermark() {
    use crate::test_support::{register_vault, seed_owner};
    let dir = TestDir::new("operation-activity-empty");
    let previous = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::db::MIGRATOR
                .iter()
                .filter(|m| m.version <= 21)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    let old = Database::open_with(&dir.database(), &previous)
        .await
        .unwrap();
    let owner = seed_owner(&old, 1).await;
    let (vault, _) = register_vault(&old, owner, 2).await;
    let mut tx = old.begin_write().await.unwrap();
    sqlx::query("INSERT INTO file_activity(id,vault_id,occurred_at,client_label,client_kind,direction,action,status) VALUES(40,?1,100,'历史','unknown','download','download','served')").bind(vault.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
    sqlx::query("DELETE FROM file_activity")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    old.close().await;
    let current = Database::open(dir.database()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM file_activity")
            .fetch_one(current.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT seq FROM sqlite_sequence WHERE name='file_activity'")
            .fetch_one(current.pool())
            .await
            .unwrap(),
        40
    );
    let inserted = sqlx::query("INSERT INTO file_activity(vault_id,occurred_at,client_label,client_kind,direction,action,status) VALUES(?1,101,'新设备','unknown','operation','create_directory','committed')").bind(vault.as_bytes().as_slice()).execute(current.pool()).await.unwrap();
    assert_eq!(inserted.last_insert_rowid(), 41);
    current.close().await;
}
