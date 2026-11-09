//! S074 native bridge: real Linux server + isolated Vault for the three-identity Windows e2e run.
//!
//! `tests/platform/windows/run-native-e2e.sh` starts this binary on the host, rewrites the URL to
//! the SSH loopback tunnel address, and copies the fixture into the guest. The helper seeds the
//! server Vault with the scenario's `server_seed` files so the first client import really is
//! "non-empty on both sides" (design §21 T01).
#[path = "../tests/common/mod.rs"]
mod common;
use common::*;
use serde_json::{Value, json};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};

#[tokio::main]
async fn main() {
    let manifest = PathBuf::from(std::env::args_os().nth(1).expect("manifest argument"));
    let stop = PathBuf::from(std::env::args_os().nth(2).expect("stop-file argument"));
    let scenario_path = PathBuf::from(std::env::args_os().nth(3).expect("scenario argument"));
    let scenario: Value = serde_json::from_slice(
        &std::fs::read(&scenario_path).expect("read scenario for server_seed"),
    )
    .expect("scenario JSON");
    let seeds = scenario["server_seed"]
        .as_array()
        .expect("scenario server_seed")
        .clone();

    let lab = Lab::new("s074-native-server", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("real server startup");
    let (status, issued) = running
        .json(
            "POST",
            "/v1/auth/login",
            None,
            Some(&json!({"login":LOGIN,"password":PASSWORD,"device_label":"S074 e2e owner"})),
        )
        .await
        .expect("owner login");
    assert_eq!(status, reqwest::StatusCode::OK);
    let token = issued["access_token"].as_str().expect("access token");
    let vault = register_vault(&running, token, &lab.work())
        .await
        .expect("register vault");

    let mut seeded = Vec::new();
    for seed in &seeds {
        let path = seed["path"].as_str().expect("seed path");
        let content = seed["content"].as_str().expect("seed content").as_bytes();
        let target = lab.work().join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("seed directory");
        }
        std::fs::write(&target, content).expect("seed file");
        seeded.push(json!({
            "path": path,
            "hash": hex(&vaultsyncd_platform_linux_server::files::hash::digest(content)),
            "size": content.len().to_string(),
        }));
    }
    // Wait until the real observer has adopted every seed as a fixed version.
    wait_for("seed adoption", || async {
        let snapshot = create_snapshot(&running, token, &vault).await?;
        let rows = snapshot_entries(
            &running,
            token,
            &vault,
            snapshot["snapshot_id"].as_str().unwrap(),
        )
        .await?;
        Ok(seeded.iter().all(|seed| {
            rows.iter().any(|row| {
                row["name"] == seed["path"]
                    && row["hash"] == seed["hash"]
                    && row["version_id"].is_string()
            })
        }))
    })
    .await
    .expect("seed adoption");

    let fixture = json!({
        "url": running.uri(""),
        "vault": vault,
        "issued": issued,
        "login": LOGIN,
        "password": PASSWORD,
        "server_version": env!("CARGO_PKG_VERSION"),
        "seeded": seeded,
        "work_dir": lab.work().display().to_string(),
    });
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&manifest)
        .expect("create fixture");
    file.write_all(serde_json::to_string(&fixture).unwrap().as_bytes())
        .expect("write fixture");
    file.sync_all().expect("sync fixture");
    println!("READY {} vault={vault}", running.uri(""));
    std::io::stdout().flush().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    while !stop.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    running.stop().await;
}
