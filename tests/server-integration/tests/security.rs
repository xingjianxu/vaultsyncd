//! S105: real TCP authorization, exact-version binding and durable credential expiry.
//! Test principals and expired timestamps are installed only while the service is stopped.
mod common;

use common::{Lab, Running, create_snapshot, login, register_root, snapshot_entries, wait_for};
use reqwest::StatusCode;
use serde_json::json;
use vaultsyncd_server_store::db::Database;

#[tokio::test]
async fn root_version_resources_reject_foreign_owner_and_mixed_identity() {
    let lab = Lab::new("s105-authorization", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.unwrap();
    let owner = login(&running).await.unwrap();
    let root = register_root(&running, &owner, &lab.work()).await.unwrap();
    let second_path = lab.root().join("other-root");
    std::fs::create_dir(&second_path).unwrap();
    let second_root = register_root(&running, &owner, &second_path).await.unwrap();
    let bytes = b"S105 fixed content must never authorize another Root";
    std::fs::write(lab.work().join("private.txt"), bytes).unwrap();
    wait_for("S105 source adopted", || async {
        let snapshot = create_snapshot(&running, &owner, &root).await?;
        let entries = snapshot_entries(
            &running,
            &owner,
            &root,
            snapshot["snapshot_id"].as_str().unwrap(),
        )
        .await?;
        Ok(entries
            .iter()
            .any(|e| e["name"] == "private.txt" && e["version_id"].is_string()))
    })
    .await
    .unwrap();
    let snapshot = create_snapshot(&running, &owner, &root).await.unwrap();
    let entries = snapshot_entries(
        &running,
        &owner,
        &root,
        snapshot["snapshot_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    let entry = entries.iter().find(|e| e["name"] == "private.txt").unwrap();
    let entry_id = entry["entry_id"].as_str().unwrap();
    let version_id = entry["version_id"].as_str().unwrap();
    let base = format!("/v1/roots/{root}");
    let content = format!("{base}/entries/{entry_id}/versions/{version_id}/content");
    assert_eq!(running.download(&content, &owner).await.unwrap().1, bytes);
    running.stop().await;
    let database = Database::open(lab.state().join("server.sqlite"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO users(id,login,password_hash,status) SELECT ?, 's105-other',password_hash,'active' FROM users LIMIT 1")
        .bind([0xa5u8; 16].as_slice()).execute(database.pool()).await.unwrap();
    database.close().await;
    let running = Running::start(&lab).await.unwrap();
    let (status, issued) = running.json("POST", "/v1/auth/login", None, Some(&json!({"login":"s105-other","password":common::PASSWORD,"device_label":"S105 foreign owner"}))).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let other = issued["access_token"].as_str().unwrap();
    for (method, path, body) in [
        ("GET", content.clone(), None),
        ("GET", format!("{base}/entries/{entry_id}/history"), None),
        ("GET", format!("{base}/trash"), None),
        ("GET", format!("{base}/recovery"), None),
        ("GET", format!("{base}/conflicts"), None),
        (
            "GET",
            format!(
                "{base}/snapshots/{}/entries",
                snapshot["snapshot_id"].as_str().unwrap()
            ),
            None,
        ),
        ("POST", format!("{base}/snapshots"), Some(json!({}))),
        (
            "POST",
            format!("{base}/entries/{entry_id}/versions/{version_id}/leases"),
            Some(json!({"lease_id":"a5".repeat(16),"sha256":entry["hash"],"size":entry["size"]})),
        ),
    ] {
        let (status, rejected) = running
            .json(method, &path, Some(other), body.as_ref())
            .await
            .unwrap();
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {rejected}");
        assert_eq!(rejected["error"]["code"], "ACCESS_DENIED");
        assert!(!rejected.to_string().contains("private.txt"));
    }
    // Even the same owner cannot move a fixed version into a different Root's URL.
    let mixed = content.replace(&root, &second_root);
    assert_eq!(
        running.download(&mixed, &owner).await.unwrap().0,
        StatusCode::GONE
    );
    let mixed_entry = content.replace(entry_id, &"f1".repeat(16));
    assert_eq!(
        running.download(&mixed_entry, &owner).await.unwrap().0,
        StatusCode::GONE
    );
    assert_eq!(
        std::fs::read(lab.work().join("private.txt")).unwrap(),
        bytes
    );
    running.stop().await;
}

#[tokio::test]
async fn expired_access_and_refresh_cannot_read_or_rotate_after_real_restart() {
    let lab = Lab::new("s105-expiry", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.unwrap();
    let (status, issued) = running.json("POST", "/v1/auth/login", None, Some(&json!({"login":common::LOGIN,"password":common::PASSWORD,"device_label":"S105 expiry"}))).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    running.stop().await;
    let database = Database::open(lab.state().join("server.sqlite"))
        .await
        .unwrap();
    // Offline fixture: retain real digests and IDs, move the complete validity interval into the past.
    sqlx::query("UPDATE sessions SET created_at=1,access_expires_at=2,refresh_expires_at=3")
        .execute(database.pool())
        .await
        .unwrap();
    database.close().await;
    let running = Running::start(&lab).await.unwrap();
    for (method, path, body) in [
        ("GET", "/v1/auth/account", None),
        ("GET", "/v1/roots", None),
        ("GET", "/v1/admin/backup-snapshots", None),
        (
            "POST",
            "/v1/auth/refresh",
            Some(json!({"refresh_token":issued["refresh_token"]})),
        ),
    ] {
        let (status, rejected) = running
            .json(
                method,
                path,
                Some(issued["access_token"].as_str().unwrap()),
                body.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path}: {rejected}"
        );
        assert_eq!(rejected["error"]["code"], "AUTH_EXPIRED");
    }
    // Expiry is not a service outage: a fresh password login still works.
    assert!(!login(&running).await.unwrap().is_empty());
    running.stop().await;
}
