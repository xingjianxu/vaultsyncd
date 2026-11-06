//! Durable at-most-once desktop delivery claim per active problem episode; never marks it read.
use super::{Notification, NotificationError, notification};
use sqlx::{Sqlite, SqliteConnection, Transaction};
/// A root cause and its delivery episode.
#[derive(Debug)]
pub struct Delivery {
    /// Existing authoritative problem record.
    pub notification: Notification,
    /// New active episode after recovery/recurrence.
    pub generation: i64,
    /// A frontend has already claimed this episode; it may not have been shown if Windows disabled it.
    pub claimed: bool,
}
/// Read root/global notification state, including recovered records.
pub async fn list(conn: &mut SqliteConnection) -> Result<Vec<Delivery>, NotificationError> {
    let rows: Vec<(String, i64, bool)> = sqlx::query_as(
        "SELECT dedupe_key,generation,delivery_claimed FROM notifications ORDER BY dedupe_key",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(NotificationError::Db)?;
    let mut result = Vec::with_capacity(rows.len());
    for (key, generation, claimed) in rows {
        let notification =
            notification(conn, &key)
                .await?
                .ok_or(NotificationError::MalformedRow {
                    column: "dedupe_key",
                })?;
        result.push(Delivery {
            notification,
            generation,
            claimed,
        });
    }
    Ok(result)
}
/// Claim only this still-active episode. A stale GUI cannot consume a later recurrence.
pub async fn claim(
    tx: &mut Transaction<'_, Sqlite>,
    key: &str,
    generation: i64,
) -> Result<bool, NotificationError> {
    let result=sqlx::query("UPDATE notifications SET delivery_claimed=1 WHERE dedupe_key=? AND generation=? AND state='active' AND delivery_claimed=0")
        .bind(key).bind(generation).execute(&mut **tx).await.map_err(NotificationError::Db)?;
    Ok(result.rows_affected() == 1)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::Database,
        notifications::{NewNotification, raise, resolve},
    };
    use vaultsyncd_domain::error::ErrorCode;
    #[tokio::test]
    async fn claims_survive_restart_do_not_mark_read_and_recur_once() {
        let dir = crate::test_support::TestDir::new("s083-delivery");
        let path = dir.database();
        let db = Database::open(&path).await.unwrap();
        let n = NewNotification {
            dedupe_key: "root:11111111111111111111111111111111:INSUFFICIENT_STORAGE".into(),
            binding: None,
            entry: None,
            code: ErrorCode::InsufficientStorage,
        };
        let mut tx = db.begin_write().await.unwrap();
        raise(&mut tx, &n).await.unwrap();
        assert!(claim(&mut tx, &n.dedupe_key, 1).await.unwrap());
        assert!(!claim(&mut tx, &n.dedupe_key, 1).await.unwrap());
        assert!(
            !notification(&mut tx, &n.dedupe_key)
                .await
                .unwrap()
                .unwrap()
                .is_read
        );
        tx.commit().await.unwrap();
        db.close().await;
        let db = Database::open(&path).await.unwrap();
        let mut tx = db.begin_write().await.unwrap();
        assert!(!claim(&mut tx, &n.dedupe_key, 1).await.unwrap());
        resolve(&mut tx, &n.dedupe_key).await.unwrap();
        raise(&mut tx, &n).await.unwrap();
        assert!(!claim(&mut tx, &n.dedupe_key, 1).await.unwrap());
        assert!(claim(&mut tx, &n.dedupe_key, 2).await.unwrap());
        tx.commit().await.unwrap();
        db.close().await;
    }
}
