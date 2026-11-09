//! S137：真实 HTTP 目录改名经过多轮观察后保留子文件，随后真实外部删除仍传播。
mod common;

use common::{
    Lab, Running, all_changes, create_snapshot, identity, login, register_vault, snapshot_entries,
    wait_for,
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::time::Duration;

async fn snapshot(running: &Running, token: &str, vault: &str) -> Vec<Value> {
    let meta = create_snapshot(running, token, vault).await.unwrap();
    snapshot_entries(running, token, vault, meta["snapshot_id"].as_str().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn consecutive_api_directory_renames_preserve_descendants_through_observer_cycles() {
    let lab = Lab::new("s137-api-directory-renames", None, None);
    std::fs::create_dir_all(lab.work().join("parent/child")).unwrap();
    let content = b"S137 directory rename preserves identity and immutable version\n";
    std::fs::write(lab.work().join("parent/child/note.txt"), content).unwrap();
    lab.init_owner().await;
    let running = Running::start(&lab).await.unwrap();
    let token = login(&running).await.unwrap();
    let vault = register_vault(&running, &token, &lab.work()).await.unwrap();
    wait_for("导入子文件", || async {
        Ok(snapshot(&running, &token, &vault)
            .await
            .iter()
            .any(|item| item["name"] == "note.txt"))
    })
    .await
    .unwrap();
    let entries = snapshot(&running, &token, &vault).await;
    let named = |name: &str| {
        entries
            .iter()
            .find(|item| item["name"] == name)
            .unwrap()
            .clone()
    };
    let original = named("note.txt");
    let parent = named("parent");
    let child = named("child");
    let vault_entry = named("");
    let epoch = create_snapshot(&running, &token, &vault).await.unwrap()["server_epoch"].clone();
    for (index, directory, destination, name, path) in [
        (
            0_u8,
            parent,
            vault_entry["entry_id"].clone(),
            "parent-renamed",
            "parent-renamed/child/note.txt",
        ),
        (
            1_u8,
            child.clone(),
            child["parent_id"].clone(),
            "child-renamed",
            "parent-renamed/child-renamed/note.txt",
        ),
    ] {
        let (status, result) = running.json("POST", &format!("/v1/vaults/{vault}/operations"), Some(&token), Some(&json!({
            "op_id": identity(0x71 + index), "device_id": identity(0x73), "server_epoch": epoch,
            "kind": "move", "entry_id": directory["entry_id"], "base_revision": directory["revision"],
            "parent_id": destination, "name": name,
        }))).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["state"], "committed");
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            let current = snapshot(&running, &token, &vault).await;
            let file = current
                .iter()
                .find(|item| item["entry_id"] == original["entry_id"])
                .expect("多轮观察后子文件仍活跃");
            assert_eq!(file["revision"], original["revision"]);
            assert_eq!(file["version_id"], original["version_id"]);
            assert_eq!(file["hash"], original["hash"]);
            assert_eq!(file["parent_id"], original["parent_id"]);
            assert_eq!(std::fs::read(lab.work().join(path)).unwrap(), content);
        }
    }
    let before = all_changes(&running, &token, &vault).await.unwrap();
    assert!(!before.iter().any(|batch| {
        batch["events_json"]
            .as_str()
            .is_some_and(|events| events.contains("\"delete\""))
    }));
    std::fs::remove_file(lab.work().join("parent-renamed/child-renamed/note.txt")).unwrap();
    wait_for("新路径真实删除被观察", || async {
        Ok(!snapshot(&running, &token, &vault)
            .await
            .iter()
            .any(|item| item["entry_id"] == original["entry_id"]))
    })
    .await
    .unwrap();
    let (status, trash) = running
        .json(
            "GET",
            &format!("/v1/vaults/{vault}/trash"),
            Some(&token),
            None,
        )
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    let deleted = trash["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["entry_id"] == original["entry_id"])
        .expect("回收站保留同一文件");
    assert_eq!(deleted["version_id"], original["version_id"]);
    running.stop().await;
}
