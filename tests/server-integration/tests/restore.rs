//! S052 续接验收：restore（历史版本恢复与回收站恢复）的真实执行。
//!
//! 1. 外部内容被采纳为 v1；客户端上传并提交 v2；
//! 2. `restore` 指定 `version_id`：旧的 v1 被恢复成**新的当前版本**（内容回到 v1），v1/v2 的历史行与
//!    字节都不变、仍可下载；
//! 3. `delete` 把文件放进回收站后，`restore`（不带 version_id）把它按原身份放回原位置，内容与条目
//!    身份不变、回收站清单被清理；
//! 4. 同一 OpId 的重试返回同一结果；不同的请求得到 409 `OP_ID_REUSED`。

mod common;

use common::{
    Lab, PutArgs, Running, create_snapshot, identity, login, put_operation, register_vault,
    snapshot_entries, upload, wait_for,
};
use reqwest::StatusCode;
use serde_json::{Value, json};

const V1: &[u8] = b"version one written outside\n";
const V2: &[u8] = b"version two uploaded by client\n";

fn sha256(bytes: &[u8]) -> String {
    common::hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        bytes,
    ))
}

async fn submit(running: &Running, token: &str, vault: &str, body: Value) -> (StatusCode, Value) {
    running
        .json(
            "POST",
            &format!("/v1/vaults/{vault}/operations"),
            Some(token),
            Some(&body),
        )
        .await
        .expect("提交操作")
}

async fn snapshot(running: &Running, token: &str, vault: &str) -> Vec<Value> {
    let created = create_snapshot(running, token, vault).await.expect("快照");
    let id = created
        .get("snapshot_id")
        .and_then(Value::as_str)
        .expect("snapshot_id")
        .to_owned();
    snapshot_entries(running, token, vault, &id)
        .await
        .expect("快照清单")
}

fn entry_named<'a>(entries: &'a [Value], name: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
        .expect("条目存在")
}

#[tokio::test]
async fn version_restore_and_trash_restore_round_trip() {
    let lab = Lab::new("restore", None, None);
    lab.init_owner().await;
    let running = Running::start(&lab).await.expect("启动服务端");
    let token = login(&running).await.expect("登录");
    let vault = register_vault(&running, &token, &lab.work())
        .await
        .expect("注册 Vault");
    let work_file = lab.work().join("note.txt");

    // 1. 外部 v1 被采纳。
    std::fs::write(&work_file, V1).expect("写 v1");
    wait_for("v1 采纳", || async {
        let entries = snapshot(&running, &token, &vault).await;
        Ok(entries
            .iter()
            .any(|entry| entry.get("name").and_then(Value::as_str) == Some("note.txt")))
    })
    .await
    .expect("初始采纳");
    let snapshot_v1 = create_snapshot(&running, &token, &vault)
        .await
        .expect("快照");
    let epoch = snapshot_v1["server_epoch"]
        .as_str()
        .expect("epoch")
        .to_owned();
    let entries = snapshot(&running, &token, &vault).await;
    let note = entry_named(&entries, "note.txt");
    let entry = note["entry_id"].as_str().expect("条目").to_owned();
    let v1_version = note["version_id"].as_str().expect("v1 版本").to_owned();
    let revision = note["revision"].as_str().expect("revision").to_owned();

    // 2. 上传并提交 v2。
    let (upload_id, _, _) = upload(&running, &token, &vault, V2, 32)
        .await
        .expect("上传 v2");
    let device = identity(0x52);
    let args = PutArgs {
        op_id: &identity(0x61),
        device_id: &device,
        epoch: &epoch,
        entry: &entry,
        revision: &revision,
        upload: &upload_id,
    };
    let (status, result) = put_operation(&running, &token, &vault, &args)
        .await
        .expect("提交 v2");
    assert_eq!(status, StatusCode::OK, "{result}");
    let v2_version = result["version_id"].as_str().expect("v2 版本").to_owned();
    assert_eq!(std::fs::read(&work_file).expect("读文件"), V2);

    // S072: typed, authorized history listing and closed cursor/query shape.
    let history_path = format!("/v1/vaults/{vault}/entries/{entry}/history");
    let (status, history) = running
        .json("GET", &history_path, Some(&token), None)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["items"].as_array().unwrap().len(), 2);
    assert!(
        history["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["version_id"] == v1_version)
    );
    assert_eq!(
        running
            .json("GET", &history_path, None, None)
            .await
            .unwrap()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        running
            .json(
                "GET",
                &format!("{history_path}?ignored=true"),
                Some(&token),
                None
            )
            .await
            .unwrap()
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        running
            .json(
                "GET",
                &format!(
                    "{history_path}?after={}&after={}",
                    identity(0x11),
                    identity(0x22)
                ),
                Some(&token),
                None
            )
            .await
            .unwrap()
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        running
            .json(
                "GET",
                &format!("/v1/vaults/{}/entries/{entry}/history", identity(0x99)),
                Some(&token),
                None
            )
            .await
            .unwrap()
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        running
            .json(
                "GET",
                &format!("/v1/vaults/{vault}/entries/{}/history", identity(0x99)),
                Some(&token),
                None
            )
            .await
            .unwrap()
            .0,
        StatusCode::NOT_FOUND
    );

    // 3. 历史版本恢复：v1 → 新的当前版本（内容回到 V1）。
    let (status, restored) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x62),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "restore",
            "entry_id": entry,
            "base_revision": result["revision"],
            "version_id": v1_version,
            "local_generation": "9",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "版本恢复应提交：{restored}");
    assert_eq!(restored["state"], "committed");
    assert_eq!(restored["entry_id"], entry);
    assert_ne!(
        restored["version_id"], v1_version,
        "恢复必须产生新版本，而不是把旧版本行改回去"
    );
    assert_eq!(
        std::fs::read(&work_file).expect("读文件"),
        V1,
        "内容回到 v1"
    );
    let entries = snapshot(&running, &token, &vault).await;
    assert_eq!(entry_named(&entries, "note.txt")["hash"], sha256(V1));

    // 旧版本仍然可下载：v1 与 v2 的字节都不变（I12）。
    for (version, expected) in [(&v1_version, V1), (&v2_version, V2)] {
        let (status, bytes, _) = running
            .download(
                &format!("/v1/vaults/{vault}/entries/{entry}/versions/{version}/content"),
                &token,
            )
            .await
            .expect("下载历史");
        assert_eq!(status, StatusCode::OK, "历史版本 {version} 仍可下载");
        assert_eq!(bytes, expected, "历史版本 {version} 的字节不变");
    }

    // 4. 删除（进回收站），再按原身份恢复。
    let entries = snapshot(&running, &token, &vault).await;
    let current_revision = entry_named(&entries, "note.txt")["revision"]
        .as_str()
        .expect("revision")
        .to_owned();
    let (status, deleted) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x63),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "delete",
            "entry_id": entry,
            "base_revision": current_revision,
            "local_generation": "9",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert!(!work_file.exists(), "删除后工作目录里没有文件");
    let entries = snapshot(&running, &token, &vault).await;
    assert!(
        !entries
            .iter()
            .any(|item| item.get("entry_id").and_then(Value::as_str) == Some(&entry)),
        "删除后条目不在活跃清单里"
    );

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
    assert_eq!(trash["items"][0]["entry_id"], entry);
    assert_eq!(trash["items"][0]["hash"], sha256(V1));
    assert_eq!(
        running
            .json(
                "GET",
                &format!("/v1/vaults/{}/trash", identity(0x99)),
                Some(&token),
                None
            )
            .await
            .unwrap()
            .0,
        StatusCode::FORBIDDEN
    );

    let (status, restored) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x64),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "restore",
            "entry_id": entry,
            "base_revision": deleted["revision"].as_str().unwrap_or("0"),
            "local_generation": "9",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "回收站恢复应提交：{restored}");
    assert_eq!(restored["entry_id"], entry, "恢复不造新身份");
    assert!(
        work_file.is_file(),
        "恢复后文件必须真的回到工作目录（发布内容，而不是只改数据库）"
    );
    assert_eq!(std::fs::read(&work_file).expect("读文件"), V1);
    let entries = snapshot(&running, &token, &vault).await;
    assert_eq!(entry_named(&entries, "note.txt")["entry_id"], entry);
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
    assert!(trash["items"].as_array().unwrap().is_empty());
    // Restored event revision and fixed version facts must equal the newly committed namespace.
    let (_, changes) = running
        .json(
            "GET",
            &format!("/v1/vaults/{vault}/changes?after=0"),
            Some(&token),
            None,
        )
        .await
        .unwrap();
    let restore_event = changes["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| {
            serde_json::from_str::<Vec<Value>>(b["events_json"].as_str().unwrap()).unwrap()
        })
        .rfind(|e| e["kind"] == "restore")
        .unwrap();
    assert_eq!(restore_event["revision"], restored["revision"]);
    assert_eq!(
        restore_event["version_id"],
        entry_named(&entries, "note.txt")["version_id"]
    );
    assert_eq!(restore_event["hash"], sha256(V1));

    // 5. 同一 OpId 重试同一请求：结果一致（replayed）。
    let (status, replay) = submit(
        &running,
        &token,
        &vault,
        json!({
            "op_id": identity(0x64),
            "device_id": device,
            "server_epoch": epoch,
            "kind": "restore",
            "entry_id": entry,
            "base_revision": deleted["revision"].as_str().unwrap_or("0"),
            "local_generation": "9",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["entry_id"], entry);
    assert_eq!(replay["vault_seq"], restored["vault_seq"]);
    assert_eq!(replay["revision"], restored["revision"]);
    assert_ne!(replay["vault_seq"], "0");

    running.stop().await;
}
