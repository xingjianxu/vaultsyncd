//! The real content Create consumer: fixed upload, absent CAS, replay and process restart.
mod common;
use common::*;
use reqwest::StatusCode;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalized_content_create_preserves_original_result_across_restart() {
    let lab = Lab::new("s070-create-file", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.unwrap();
    let token = login(&running).await.unwrap();
    let vault = register_vault(&running, &token, &lab.work()).await.unwrap();
    let snap = create_snapshot(&running, &token, &vault).await.unwrap();
    let rows = snapshot_entries(
        &running,
        &token,
        &vault,
        snap["snapshot_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    let parent = rows.iter().find(|r| r["parent_id"].is_null()).unwrap()["entry_id"]
        .as_str()
        .unwrap();
    let (upload, _, _) = upload(
        &running,
        &token,
        &vault,
        b"new file through real content Create",
        1024,
    )
    .await
    .unwrap();
    let body = json!({"op_id":identity(0x70),"device_id":identity(0x71),"server_epoch":snap["server_epoch"],"kind":"create","parent_id":parent,"name":"created.txt","expected_absent":true,"upload_id":upload,"local_generation":"9","conflict_policy":"preserve_both"});
    let endpoint = format!("/v1/vaults/{vault}/operations");
    let (status, first) = running
        .json("POST", &endpoint, Some(&token), Some(&body))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["state"], "committed");
    assert!(first["version_id"].is_string());
    assert_eq!(first["echo_local_generation"], "9");
    assert_eq!(
        std::fs::read(lab.work().join("created.txt")).unwrap(),
        b"new file through real content Create"
    );
    let (_, replayed) = running
        .json("POST", &endpoint, Some(&token), Some(&body))
        .await
        .unwrap();
    for key in ["entry_id", "version_id", "revision", "vault_seq"] {
        assert_eq!(first[key], replayed[key]);
    }
    let mut altered = body.clone();
    altered["name"] = "different.txt".into();
    let (code, _) = running
        .json("POST", &endpoint, Some(&token), Some(&altered))
        .await
        .unwrap();
    assert_eq!(code, StatusCode::CONFLICT);
    running.stop().await;
    let restarted = Running::start(&lab).await.unwrap();
    let token = login(&restarted).await.unwrap();
    let (_, after) = restarted
        .json("POST", &endpoint, Some(&token), Some(&body))
        .await
        .unwrap();
    for key in ["entry_id", "version_id", "revision", "vault_seq"] {
        assert_eq!(first[key], after[key]);
    }
    let s = create_snapshot(&restarted, &token, &vault).await.unwrap();
    let rows = snapshot_entries(
        &restarted,
        &token,
        &vault,
        s["snapshot_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        rows.iter().filter(|r| r["name"] == "created.txt").count(),
        1
    );
    assert!(!lab.work().join("different.txt").exists());
    restarted.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killed_create_publication_recovers_once_and_corrupt_context_never_republishes() {
    use std::{
        os::unix::process::ExitStatusExt,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for (phase, tamper) in [
        ("prepared", false),
        ("published", false),
        ("committed", false),
        ("prepared", true),
    ] {
        let lab = Lab::new("s070-create-kill", None, None);
        lab.init_owner().await;
        let running = Running::start(&lab).await.unwrap();
        let token = login(&running).await.unwrap();
        let vault = register_vault(&running, &token, &lab.work()).await.unwrap();
        let snapshot = create_snapshot(&running, &token, &vault).await.unwrap();
        let rows = snapshot_entries(
            &running,
            &token,
            &vault,
            snapshot["snapshot_id"].as_str().unwrap(),
        )
        .await
        .unwrap();
        let parent = rows.iter().find(|e| e["parent_id"].is_null()).unwrap()["entry_id"].clone();
        let (upload, _, _) = upload(&running, &token, &vault, b"fixed create crash bytes", 1024)
            .await
            .unwrap();
        running.stop().await;
        let fixture = lab.vault().join("create-fixture.json");
        let marker = lab.vault().join("create-boundary");
        let value = json!({"state":lab.state(),"vault":vault,"parent":parent,"upload":upload,"op":identity(0x73),"device":identity(0x74),"epoch":snapshot["server_epoch"],"name":"crash.txt","phase":phase,"marker":marker});
        std::fs::write(&fixture, serde_json::to_vec(&value).unwrap()).unwrap();
        let helper = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples/create-file-crash");
        assert!(
            helper.is_file(),
            "build create-file-crash example before this acceptance test"
        );
        let mut child = Command::new(helper)
            .arg(&fixture)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(15);
        while !marker.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "child failed before {phase}"
            );
            assert!(Instant::now() < end);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        child.kill().unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(9));
        if tamper {
            let db = vaultsyncd_server_store::db::Database::open(lab.state().join("server.sqlite"))
                .await
                .unwrap();
            let text: String = sqlx::query_scalar("SELECT context_json FROM execution_journal")
                .fetch_one(db.pool())
                .await
                .unwrap();
            let mut context: serde_json::Value = serde_json::from_str(&text).unwrap();
            context["name"] = "wrong.txt".into();
            sqlx::query("UPDATE execution_journal SET context_json=?")
                .bind(context.to_string())
                .execute(db.pool())
                .await
                .unwrap();
            db.close().await;
        }
        if tamper {
            let service = vaultsyncd_server::bootstrap::prepare(lab.config())
                .await
                .unwrap();
            assert!(!service.assessment().is_complete());
            drop(service);
            let db = vaultsyncd_server_store::db::Database::open(lab.state().join("server.sqlite"))
                .await
                .unwrap();
            let pending: String = sqlx::query_scalar("SELECT state FROM operations")
                .fetch_one(db.pool())
                .await
                .unwrap();
            assert_eq!(pending, "pending");
            db.close().await;
            assert!(!lab.work().join("crash.txt").exists());
            assert!(!lab.work().join("wrong.txt").exists());
            continue;
        }
        let restarted = Running::start(&lab)
            .await
            .unwrap_or_else(|e| panic!("restart {phase}: {e}"));
        let token = login(&restarted).await.unwrap();
        let (code, result) = restarted
            .json(
                "GET",
                &format!("/v1/vaults/{vault}/operations/{}", identity(0x73)),
                Some(&token),
                None,
            )
            .await
            .unwrap();
        if tamper {
            assert_eq!(code, StatusCode::ACCEPTED);
            assert_eq!(result["state"], "pending");
            assert!(!lab.work().join("crash.txt").exists());
            assert!(!lab.work().join("wrong.txt").exists());
        } else {
            assert_eq!(code, StatusCode::OK, "{phase}: {result}");
            assert_eq!(result["state"], "committed");
            assert_eq!(
                std::fs::read(lab.work().join("crash.txt")).unwrap(),
                b"fixed create crash bytes"
            );
            let snap = create_snapshot(&restarted, &token, &vault).await.unwrap();
            let rows = snapshot_entries(
                &restarted,
                &token,
                &vault,
                snap["snapshot_id"].as_str().unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(rows.iter().filter(|e| e["name"] == "crash.txt").count(), 1);
        }
        restarted.stop().await;
    }
}
