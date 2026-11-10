use super::Database;
use crate::clients::list;
use crate::test_support::{TestDir, insert_user, session, user};

#[tokio::test]
async fn schema17_upgrade_preserves_refresh_chain_and_backup() {
    use sqlx::migrate::Migrator;
    let dir = TestDir::new("clients-upgrade");
    let previous = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::db::MIGRATOR
                .iter()
                .filter(|m| m.version <= 17)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    let db = Database::open_with(&dir.database(), &previous)
        .await
        .unwrap();
    let mut tx = db.begin_write().await.unwrap();
    insert_user(&mut tx, user(1)).await;
    for marker in [1u8, 2] {
        let id = session(marker);
        let access = [marker; 32];
        let refresh = [marker + 1; 32];
        sqlx::query("INSERT INTO sessions VALUES(?,?,?, ?,?,1000,1900,10000,NULL,NULL)")
            .bind(id.as_bytes().as_slice())
            .bind(user(1).as_bytes().as_slice())
            .bind("旧客户端")
            .bind(access.as_slice())
            .bind(refresh.as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE sessions SET replaced_by=?1,revoked_at=1100 WHERE id=?2")
        .bind(session(2).as_bytes().as_slice())
        .bind(session(1).as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db.close().await;
    let db = Database::open(dir.database()).await.unwrap();
    assert_eq!(db.schema_version(), 20);
    let mut conn = db.pool().acquire().await.unwrap();
    let rows = list(&mut conn, user(1), session(2), None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, session(1).to_hex());
    assert!(rows[0].current);
    assert_eq!(rows[0].last_seen_at, None);
    assert_eq!(rows[0].client_kind, "unknown");
    assert_eq!(rows[0].login_ip, None);
    assert!(crate::db::migration_backup_path(&dir.database(), 17, 20).exists());
    drop(conn);
    db.close().await;
    let backup = Database::open_with(
        &crate::db::migration_backup_path(&dir.database(), 17, 20),
        &previous,
    )
    .await
    .unwrap();
    assert_eq!(backup.schema_version(), 17);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(backup.pool())
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn schema18_upgrade_preserves_history_and_recoverable_backup() {
    use sqlx::migrate::Migrator;
    let dir = TestDir::new("clients-ip-upgrade");
    let previous = Migrator {
        migrations: std::borrow::Cow::Owned(
            crate::db::MIGRATOR
                .iter()
                .filter(|m| m.version <= 18)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    let old = Database::open_with(&dir.database(), &previous)
        .await
        .unwrap();
    let mut tx = old.begin_write().await.unwrap();
    insert_user(&mut tx, user(1)).await;
    sqlx::query("INSERT INTO client_connections(id,user_id,device_label,client_kind,created_at,last_seen_at) VALUES(?1,?2,'旧客户端','client',100,200)")
        .bind(session(1).as_bytes().as_slice()).bind(user(1).as_bytes().as_slice())
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    old.close().await;
    let new = Database::open(dir.database()).await.unwrap();
    let mut conn = new.pool().acquire().await.unwrap();
    let rows = list(&mut conn, user(1), session(1), None).await.unwrap();
    assert_eq!(rows[0].login_ip, None);
    assert_eq!(rows[0].last_seen_at, Some(200));
    assert_eq!(rows[0].device_label, "旧客户端");
    drop(conn);
    new.close().await;
    assert!(matches!(
        Database::open_with(&dir.database(), &previous).await,
        Err(crate::db::DbError::SchemaTooNew {
            found: 20,
            supported: 18
        })
    ));
    let backup = crate::db::migration_backup_path(&dir.database(), 18, 20);
    let restored = dir.path.join("restored.sqlite");
    std::fs::copy(&backup, &restored).unwrap();
    let old = Database::open_with(&restored, &previous).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM client_connections WHERE device_label='旧客户端' AND last_seen_at=200")
        .fetch_one(old.pool()).await.unwrap();
    assert_eq!(count, 1);
    old.close().await;
    let new = Database::open(&restored).await.unwrap();
    assert_eq!(new.schema_version(), 20);
    new.close().await;
}
