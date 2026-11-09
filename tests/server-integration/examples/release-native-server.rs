//! Native Windows release bridge: real Linux production server, isolated btrfs fixture.
#[path = "../tests/common/mod.rs"]
mod common;
use common::*;
use serde_json::json;
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};
#[tokio::main]
async fn main() {
    let manifest = PathBuf::from(std::env::args_os().nth(1).expect("manifest argument"));
    let stop = PathBuf::from(std::env::args_os().nth(2).expect("stop-file argument"));
    let lab = Lab::new("s069-native-server", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("real server startup");
    let (_, issued) = running
        .json(
            "POST",
            "/v1/auth/login",
            None,
            Some(
                &json!({"login":LOGIN,"password":PASSWORD,"device_label":"S069 native acceptance"}),
            ),
        )
        .await
        .unwrap();
    let token = issued["access_token"].as_str().unwrap();
    let vault = register_vault(&running, token, &lab.work()).await.unwrap();
    let payload = "S069 real committed immutable content, retained by a real HTTP object lease.\n"
        .repeat(128);
    std::fs::write(lab.work().join("file.txt"), &payload).unwrap();
    wait_for("adopt fixed version", || async {
        let snap = create_snapshot(&running, token, &vault).await?;
        let rows = snapshot_entries(
            &running,
            token,
            &vault,
            snap["snapshot_id"].as_str().unwrap(),
        )
        .await?;
        Ok(rows
            .iter()
            .any(|e| e["name"] == "file.txt" && e["version_id"].is_string()))
    })
    .await
    .unwrap();
    let snap = create_snapshot(&running, token, &vault).await.unwrap();
    let rows = snapshot_entries(
        &running,
        token,
        &vault,
        snap["snapshot_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    let entry = rows.into_iter().find(|e| e["name"] == "file.txt").unwrap();
    let fixture = json!({"url":running.uri(""),"vault":vault,"entry":entry,"issued":issued,"payload":payload});
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&manifest)
        .unwrap();
    f.write_all(serde_json::to_string(&fixture).unwrap().as_bytes())
        .unwrap();
    f.sync_all().unwrap();
    println!("READY {}", running.uri(""));
    std::io::stdout().flush().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(1800);
    while !stop.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    running.stop().await;
}
