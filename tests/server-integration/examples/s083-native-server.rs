//! Real isolated Linux server facts for S083 native desktop recovery; never online database writes.
#[path = "../tests/common/mod.rs"]
mod common;
use common::*;
use serde_json::{Value, json};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};
async fn adopted(r: &Running, t: &str, vault: &str, name: &str, bytes: &[u8]) -> Value {
    let hash = hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        bytes,
    ));
    wait_for("external content adoption", || async {
        Ok(snapshot(r, t, vault)
            .await
            .iter()
            .any(|e| e["name"] == name && e["hash"] == hash))
    })
    .await
    .unwrap();
    entry_named(&snapshot(r, t, vault).await, name).clone()
}
#[tokio::main]
async fn main() {
    let manifest = PathBuf::from(std::env::args_os().nth(1).unwrap());
    let stop = PathBuf::from(std::env::args_os().nth(2).unwrap());
    let lab = Lab::new("s083-native-history", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.unwrap();
    let (_, issued) = running
        .json(
            "POST",
            "/v1/auth/login",
            None,
            Some(&json!({"login":LOGIN,"password":PASSWORD,"device_label":"S083 native"})),
        )
        .await
        .unwrap();
    let token = issued["access_token"].as_str().unwrap();
    let vault = register_vault(&running, token, &lab.work()).await.unwrap();
    std::fs::write(lab.work().join("history.txt"), b"S083 history original").unwrap();
    let original = adopted(
        &running,
        token,
        &vault,
        "history.txt",
        b"S083 history original",
    )
    .await;
    std::fs::write(lab.work().join("history.txt"), b"S083 history current").unwrap();
    let head = adopted(
        &running,
        token,
        &vault,
        "history.txt",
        b"S083 history current",
    )
    .await;
    std::fs::write(lab.work().join("trash.txt"), b"S083 deleted content").unwrap();
    let deleted = adopted(
        &running,
        token,
        &vault,
        "trash.txt",
        b"S083 deleted content",
    )
    .await;
    std::fs::remove_file(lab.work().join("trash.txt")).unwrap();
    wait_for("real trash adoption", || async {
        let (_, p) = running
            .json(
                "GET",
                &format!("/v1/vaults/{vault}/trash"),
                Some(token),
                None,
            )
            .await?;
        Ok(p["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["entry_id"] == deleted["entry_id"]))
    })
    .await
    .unwrap();
    std::fs::write(lab.work().join("trash.txt"), b"S083 same-name target kept").unwrap();
    adopted(
        &running,
        token,
        &vault,
        "trash.txt",
        b"S083 same-name target kept",
    )
    .await;
    // Stale CAS uses the real upload/operation path and creates a durable conflict copy.
    let (upload, _, _) = upload(&running, token, &vault, b"S083 independent conflict", 0)
        .await
        .unwrap();
    let epoch = epoch_of(&running, token, &vault).await;
    let (_, conflict) = put_operation(
        &running,
        token,
        &vault,
        &PutArgs {
            op_id: &"c3".repeat(16),
            device_id: &"d3".repeat(16),
            epoch: &epoch,
            entry: head["entry_id"].as_str().unwrap(),
            revision: original["revision"].as_str().unwrap(),
            upload: &upload,
        },
    )
    .await
    .unwrap();
    assert_eq!(conflict["state"], "committed");
    assert!(conflict["conflict"].is_object());
    let foreign_dir = lab.vault().join("foreign");
    std::fs::create_dir(&foreign_dir).unwrap();
    let foreign_vault = register_vault(&running, token, &foreign_dir).await.unwrap();
    std::fs::write(
        foreign_dir.join("secret.txt"),
        b"S083 foreign Vault content",
    )
    .unwrap();
    let foreign = adopted(
        &running,
        token,
        &foreign_vault,
        "secret.txt",
        b"S083 foreign Vault content",
    )
    .await;
    let fixture = json!({"url":running.uri(""),"vault":vault,"login":LOGIN,"password":PASSWORD,"work_dir":lab.work(),"original":original,"head":head,"deleted":deleted,"foreign":foreign,"conflict":conflict});
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&manifest)
        .unwrap();
    file.write_all(serde_json::to_string(&fixture).unwrap().as_bytes())
        .unwrap();
    file.sync_all().unwrap();
    println!("READY {}", running.uri(""));
    std::io::stdout().flush().unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(1800);
    while !stop.exists() && std::time::Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    running.stop().await;
}
