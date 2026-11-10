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
    assert_eq!(new.schema_version(), 20);
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
            found: 20,
            supported: 19
        })
    ));
    assert_eq!(std::fs::read(dir.database()).unwrap(), before);
    let backup = crate::db::migration_backup_path(&dir.database(), 19, 20);
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
    assert_eq!(upgraded.schema_version(), 20);
}
