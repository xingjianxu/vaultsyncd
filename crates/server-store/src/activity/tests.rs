use super::*;
use crate::{operations, test_support::*};

fn at() -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(100).unwrap()
}

#[tokio::test]
async fn commit_records_are_transactional_idempotent_scoped_and_keep_snapshots() {
    let dir = TestDir::new("activity-commit");
    let db = open_database(&dir).await;
    let owner = seed_owner(&db, 1).await;
    let other = seed_owner(&db, 2).await;
    let (vault, _) = register_vault(&db, owner, 3).await;
    let key = OperationKey {
        vault,
        device: device(4),
        op: op(5),
    };
    let digest = operations::request_digest(&[("kind", b"put")]).unwrap();
    let mut tx = db.begin_write().await.unwrap();
    sqlx::query("INSERT INTO sessions VALUES(?1,?2,'原设备',?3,?4,1,1000,2000,NULL,NULL)")
        .bind(session(6).as_bytes().as_slice())
        .bind(owner.as_bytes().as_slice())
        .bind([6u8; 32].as_slice())
        .bind([7u8; 32].as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO client_connections(id,session_id,user_id,device_label,client_kind,created_at) VALUES(?1,?1,?2,'客户端旧标签','client',1)")
        .bind(session(6).as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.unwrap();
    // An unresolved pre-execution capture survives a process restart independently of credentials.
    bind_operation(&mut tx, owner, session(6), &key, &digest, Some(upload(11)))
        .await
        .unwrap();
    operations::begin_operation(&mut tx, owner, &operations::NewOperation { key, digest })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db.close().await;
    let db = open_database(&dir).await;
    let mut tx = db.begin_write().await.unwrap();
    sqlx::query("INSERT INTO sessions VALUES(?1,?2,'新凭据',?3,?4,100,1100,2100,NULL,NULL)")
        .bind(session(7).as_bytes().as_slice())
        .bind(owner.as_bytes().as_slice())
        .bind([8u8; 32].as_slice())
        .bind([9u8; 32].as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET replaced_by=?1,revoked_at=100 WHERE id=?2")
        .bind(session(7).as_bytes().as_slice())
        .bind(session(6).as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE client_connections SET session_id=?1 WHERE id=?2")
        .bind(session(7).as_bytes().as_slice())
        .bind(session(6).as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
    let inflight = client(&mut tx, owner, session(6)).await.unwrap();
    assert_eq!(inflight.id, Some(session(6).as_bytes().to_vec()));
    assert_eq!(inflight.label, "客户端旧标签");
    assert_eq!(inflight.kind, "client");
    let wrong_digest = operations::request_digest(&[("kind", b"different")]).unwrap();
    assert!(
        !bind_operation(&mut tx, owner, session(6), &key, &wrong_digest, None)
            .await
            .unwrap()
    );
    sqlx::query(
        "UPDATE client_connections SET device_label='新标签',client_kind='browser' WHERE id=?1",
    )
    .bind(session(6).as_bytes().as_slice())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let events = format!(
        r#"[{{"kind":"put","entry_id":"{}","version_id":"{}","path":"旧目录/文件.txt","size":"123"}}]"#,
        entry(7),
        entry(8)
    );
    let result = r#"{"state":"committed"}"#;
    let mut tx = db.begin_write().await.unwrap();
    operations::commit_operation(
        &mut tx,
        owner,
        &key,
        crate::test_support::tx(9),
        &events,
        result,
        at(),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let metadata_key = OperationKey { op: op(12), ..key };
    let mut tx = db.begin_write().await.unwrap();
    bind_operation(&mut tx, owner, session(6), &metadata_key, &digest, None)
        .await
        .unwrap();
    operations::begin_operation(
        &mut tx,
        owner,
        &operations::NewOperation {
            key: metadata_key,
            digest,
        },
    )
    .await
    .unwrap();
    operations::commit_operation(
        &mut tx,
        owner,
        &metadata_key,
        crate::test_support::tx(13),
        &events,
        result,
        at(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut conn = db.pool().acquire().await.unwrap();
    assert!(
        list(&mut conn, owner, vault, None, None)
            .await
            .unwrap()
            .is_empty()
    );
    drop(conn);
    let mut tx = db.begin_write().await.unwrap();
    operations::commit_operation(
        &mut tx,
        owner,
        &key,
        crate::test_support::tx(9),
        &events,
        result,
        at(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = db.begin_write().await.unwrap();
    assert!(
        operations::commit_operation(
            &mut tx,
            owner,
            &key,
            crate::test_support::tx(10),
            &events,
            result,
            at()
        )
        .await
        .is_err()
    );
    tx.rollback().await.unwrap();
    let mut conn = db.pool().acquire().await.unwrap();
    let rows = list(&mut conn, owner, vault, None, None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path.as_deref(), Some("旧目录/文件.txt"));
    assert_eq!(rows[0].size, Some(123));
    assert_eq!(rows[0].status, "committed");
    let observed: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert!(rows[0].occurred_at > 100 && rows[0].occurred_at <= observed);
    assert_eq!(rows[0].client_id, Some(session(6).to_string()));
    assert_eq!(rows[0].client_label, "客户端旧标签");
    assert_eq!(rows[0].client_kind, "client");
    assert!(
        list(&mut conn, other, vault, None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        list(&mut conn, owner, vault, None, Some("download"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn transfers_page_by_identity_and_finalize_deduplicates_without_losing_download_ranges() {
    let dir = TestDir::new("activity-transfer");
    let db = open_database(&dir).await;
    let owner = seed_owner(&db, 1).await;
    let (vault, root) = register_vault(&db, owner, 2).await;
    let client = Client {
        id: Some(vec![3; 16]),
        label: "测试设备".into(),
        kind: "client".into(),
    };
    let mut conn = db.pool().acquire().await.unwrap();
    assert_eq!(
        path(&mut conn, vault, &root.to_string()).await.unwrap(),
        Some(String::new())
    );
    let mut item = Transfer {
        vault,
        at: at(),
        client: &client,
        action: "upload_finalize",
        status: "ready",
        entry: None,
        version: None,
        path: None,
        size: Some(0),
        bytes: Some(0),
        offset: None,
        upload: Some(upload(4)),
        error: None,
    };
    for _ in 0..2 {
        record(&mut conn, &item).await.unwrap();
    }
    assert_eq!(
        list(&mut conn, owner, vault, None, None)
            .await
            .unwrap()
            .len(),
        1
    );
    item.action = "download";
    item.status = "served";
    item.upload = None;
    item.size = Some(100);
    item.bytes = Some(4);
    item.offset = Some(12);
    for _ in 0..55 {
        record(&mut conn, &item).await.unwrap();
    }
    let first = list(&mut conn, owner, vault, None, Some("download"))
        .await
        .unwrap();
    assert_eq!(first.len(), 51);
    assert_eq!(first[0].offset, Some(12));
    assert_eq!(first[0].bytes, Some(4));
    let second = list(
        &mut conn,
        owner,
        vault,
        Some(first[49].id),
        Some("download"),
    )
    .await
    .unwrap();
    assert_eq!(second.len(), 5);
    assert!(second.iter().all(|r| r.id < first[49].id));
}
