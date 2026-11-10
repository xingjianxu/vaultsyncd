use super::*;
use crate::{
    db::Database,
    sessions::{self, DIGEST_LEN},
    test_support::{TestDir, at, insert_user, open_database, session, user},
};

fn new_session(marker: u8, owner: UserId) -> NewSession {
    NewSession {
        id: session(marker),
        user_id: owner,
        device_label: "<script>设备</script>".into(),
        access_digest: [marker; DIGEST_LEN],
        refresh_digest: [marker.wrapping_add(1); DIGEST_LEN],
        created_at: at(1000),
        access_expires_at: at(1900),
        refresh_expires_at: at(10000),
    }
}

#[tokio::test]
async fn login_rotation_touch_scope_pagination_and_restart() {
    let dir = TestDir::new("clients-lifecycle");
    let db = open_database(&dir).await;
    let mut tx = db.begin_write().await.unwrap();
    insert_user(&mut tx, user(1)).await;
    insert_user(&mut tx, user(2)).await;
    for marker in 1..=53 {
        sessions::insert_session(&mut tx, &new_session(marker, user(1)))
            .await
            .unwrap();
    }
    sessions::insert_session(&mut tx, &new_session(100, user(2)))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut conn = db.pool().acquire().await.unwrap();
    let first = list(&mut conn, user(1), session(53), None).await.unwrap();
    assert_eq!(first.len(), 51);
    assert!(first[0].current);
    let cursor = SessionId::from_hex(&first[49].id).unwrap();
    let second = list(&mut conn, user(1), session(53), Some(cursor))
        .await
        .unwrap();
    assert_eq!(second.len(), 3);
    assert!(
        !cursor_exists(&mut conn, user(1), session(100))
            .await
            .unwrap()
    );
    assert!(
        list(&mut conn, user(1), session(53), Some(session(100)))
            .await
            .unwrap()
            .is_empty()
    );
    touch(&mut conn, session(53), at(1029)).await.unwrap();
    assert_eq!(
        list(&mut conn, user(1), session(53), None).await.unwrap()[0].last_seen_at,
        Some(1000)
    );
    touch(&mut conn, session(53), at(1030)).await.unwrap();
    touch(&mut conn, session(53), at(1001)).await.unwrap();
    assert_eq!(
        list(&mut conn, user(1), session(53), None).await.unwrap()[0].last_seen_at,
        Some(1030)
    );
    drop(conn);
    let mut successor = new_session(200, user(1));
    successor.created_at = at(1100);
    let mut tx = db.begin_write().await.unwrap();
    sessions::rotate_refresh(&mut tx, session(53), &successor, at(1100))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let mut conn = db.pool().acquire().await.unwrap();
    assert!(list(&mut conn, user(1), session(53), None).await.unwrap()[0].current);
    drop(conn);
    let mut tx = db.begin_write().await.unwrap();
    set_login_details(
        &mut tx,
        session(53),
        false,
        Some("2001:db8::1".parse().unwrap()),
    )
    .await
    .unwrap();
    sessions::rotate_refresh(&mut tx, session(53), &successor, at(1100))
        .await
        .unwrap();
    sessions::revoke_session(&mut tx, session(200), at(1200))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db.close().await;
    let db = Database::open(dir.database()).await.unwrap();
    let mut conn = db.pool().acquire().await.unwrap();
    let rows = list(&mut conn, user(1), session(200), None).await.unwrap();
    assert_eq!(rows[0].id, session(53).to_hex());
    assert!(rows[0].current);
    assert_eq!(rows[0].created_at, 1000);
    assert_eq!(rows[0].last_seen_at, Some(1100));
    assert_eq!(rows[0].revoked_at, Some(1200));
    assert_eq!(rows[0].client_kind, "client");
    assert_eq!(rows[0].login_ip.as_deref(), Some("2001:db8::1"));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM client_connections WHERE user_id=?")
        .bind(user(1).as_bytes().as_slice())
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(count, 53);
}
