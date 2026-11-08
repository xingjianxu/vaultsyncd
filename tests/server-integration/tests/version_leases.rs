//! S069: real authenticated HTTP leases protect exact objects across server restarts.
mod common;
use common::*;
use reqwest::StatusCode;
use serde_json::{Value, json};
async fn fixed(r: &Running, token: &str, root: &str) -> Value {
    let snap = create_snapshot(r, token, root).await.unwrap();
    snapshot_entries(r, token, root, snap["snapshot_id"].as_str().unwrap())
        .await
        .unwrap()
        .into_iter()
        .find(|e| e["name"] == "lease.txt" && e["version_id"].is_string())
        .unwrap()
}
#[tokio::test]
async fn exact_version_leases_survive_restart_reject_reuse_and_release_idempotently() {
    let lab = Lab::new("s069-lease-http", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.unwrap();
    let token = login(&running).await.unwrap();
    let root = register_root(&running, &token, &lab.work()).await.unwrap();
    std::fs::write(lab.work().join("lease.txt"), b"old fixed server content").unwrap();
    wait_for("adopt", || async {
        let snap = create_snapshot(&running, &token, &root).await?;
        let es = snapshot_entries(
            &running,
            &token,
            &root,
            snap["snapshot_id"].as_str().unwrap(),
        )
        .await?;
        Ok(es
            .iter()
            .any(|e| e["name"] == "lease.txt" && e["version_id"].is_string()))
    })
    .await
    .unwrap();
    let first = fixed(&running, &token, &root).await;
    let base = format!(
        "/v1/roots/{root}/entries/{}/versions/{}/leases",
        first["entry_id"].as_str().unwrap(),
        first["version_id"].as_str().unwrap()
    );
    let id = "69696969696969696969696969696969";
    let body = json!({"lease_id":id,"sha256":first["hash"],"size":first["size"]});
    assert_eq!(
        running
            .json("POST", &base, None, Some(&body))
            .await
            .unwrap()
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, lease) = running
        .json("POST", &base, Some(&token), Some(&body))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{lease}");
    assert_eq!(lease["version_id"], first["version_id"]);
    assert_eq!(lease["ttl_seconds"], "120");
    let (_, replay) = running
        .json("POST", &base, Some(&token), Some(&body))
        .await
        .unwrap();
    assert_eq!(replay["expires_at"], lease["expires_at"]);
    let mut bad = body.clone();
    bad["size"] = json!("1");
    assert_eq!(
        running
            .json("POST", &base, Some(&token), Some(&bad))
            .await
            .unwrap()
            .0,
        StatusCode::CONFLICT
    );
    std::fs::write(
        lab.work().join("lease.txt"),
        b"a newer head must not change the leased version",
    )
    .unwrap();
    wait_for("new head", || async {
        let snap = create_snapshot(&running, &token, &root).await?;
        let entries = snapshot_entries(
            &running,
            &token,
            &root,
            snap["snapshot_id"].as_str().unwrap(),
        )
        .await?;
        Ok(entries
            .iter()
            .any(|e| e["name"] == "lease.txt" && e["version_id"] != first["version_id"]))
    })
    .await
    .unwrap();
    let (status, old) = running
        .json("POST", &base, Some(&token), Some(&body))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(old["version_id"], first["version_id"]);
    running.stop().await;
    let running = Running::start(&lab).await.unwrap();
    let token = login(&running).await.unwrap();
    assert_eq!(
        running
            .json("POST", &base, Some(&token), Some(&body))
            .await
            .unwrap()
            .0,
        StatusCode::OK
    );
    assert_eq!(
        running
            .json(
                "POST",
                &format!("{base}/{id}/renew"),
                Some(&token),
                Some(&body)
            )
            .await
            .unwrap()
            .0,
        StatusCode::OK
    );
    for _ in 0..2 {
        let (status, closed) = running
            .json(
                "POST",
                &format!("{base}/{id}/release"),
                Some(&token),
                Some(&body),
            )
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(closed["closed"], true);
    }
    assert_eq!(
        running
            .json("POST", &base, Some(&token), Some(&body))
            .await
            .unwrap()
            .0,
        StatusCode::GONE
    );
    let digest =
        vaultsyncd_server_core::objects::ContentDigest::from_hex(first["hash"].as_str().unwrap())
            .unwrap();
    let object = lab.state().join("objects").join(
        vaultsyncd_server_core::objects::ObjectLayout::scoped_relative_object_path(
            vaultsyncd_domain::ids::RootId::from_hex(&root).unwrap(),
            &digest,
        ),
    );
    let retained = object.with_extension("s069-retained");
    std::fs::rename(&object, &retained).unwrap();
    let mut missing = body.clone();
    missing["lease_id"] = json!("70707070707070707070707070707070");
    assert_eq!(
        running
            .json("POST", &base, Some(&token), Some(&missing))
            .await
            .unwrap()
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    std::fs::rename(retained, &object).unwrap();
    let mismatch = base.replacen(&root, "ffffffffffffffffffffffffffffffff", 1);
    assert_eq!(
        running
            .json("POST", &mismatch, Some(&token), Some(&body))
            .await
            .unwrap()
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        running
            .json(
                "POST",
                &format!("{base}/{id}/renew"),
                Some(&token),
                Some(&body)
            )
            .await
            .unwrap()
            .0,
        StatusCode::GONE
    );
    running.stop().await;
}
