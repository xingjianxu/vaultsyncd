//! S084: real product IPC, abnormal process exit, NTFS locks and an independent readable export.
//! No online SQLite writes, synthetic success snapshots, or execution of recovered data.
#![cfg(windows)]
use rand_core::{OsRng, RngCore};
use serde_json::{Value, json};
use sqlx::Connection;
use std::{
    fs::{File, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use vaultsyncd_http_client::{ApiClient, ClientConfig, Credentials, ReqwestTransport, sync};
use vaultsyncd_local_ipc::generated::{GetStatusResult, ManagementResult, Response, ResponseKind};
use vaultsyncd_protocol::generated::{
    OperationKind, OperationRequest, SessionIssued, SnapshotEntry,
};
use vaultsyncd_transfer::hash::{digest_of, to_hex};

const UPLOAD: &[u8] = b"S084 fixed upload bytes before the later edit";
const LATEST: &[u8] = b"S084 newest dirty bytes must survive abnormal exit";
const NEW: &[u8] = b"S084 new unobserved content; this is data, never executed";
const INTENT: &[u8] = b"S084 original intent content";
const BASELINE: &[u8] = b"S084 acknowledged baseline";
fn id() -> String {
    let mut bytes = [0; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash(bytes: &[u8]) -> String {
    to_hex(&digest_of(bytes))
}
struct Agent {
    child: Child,
    log: PathBuf,
}
impl Agent {
    fn launch(exe: &Path, config: &Path, log: &Path) -> Self {
        let child = Command::new(exe)
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(log).unwrap())
            .spawn()
            .unwrap();
        use std::io::Write;
        writeln!(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(config.parent().unwrap().join("agent-pids.txt"))
                .unwrap(),
            "{}",
            child.id()
        )
        .unwrap();
        Self {
            child,
            log: log.to_path_buf(),
        }
    }
    fn alive(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "owned agent exited: {}",
            std::fs::read_to_string(&self.log).unwrap()
        );
    }
    fn terminate(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
}
impl Drop for Agent {
    fn drop(&mut self) {
        self.terminate();
    }
}
async fn request(name: &str, parameters: Value) -> Response {
    let request=serde_json::from_value(json!({"request_id":id(),"ipc_version":{"major":1,"minor":4},"command":{"name":name,"parameters":parameters}})).unwrap();
    let result = vaultsyncd_platform_windows::ipc::Endpoint::current("vaultsyncd-agent")
        .unwrap()
        .request(&request)
        .await
        .unwrap();
    result.validate().unwrap();
    result
}
async fn ok(name: &str, p: Value) -> ManagementResult {
    let result = request(name, p).await;
    assert_eq!(result.kind, ResponseKind::Ok, "{name}: {:?}", result.error);
    result.result.unwrap().management.unwrap()
}
async fn status(agent: &mut Agent) -> GetStatusResult {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        agent.alive();
        let request=serde_json::from_value(json!({"request_id":id(),"ipc_version":{"major":1,"minor":4},"command":{"name":"get_status"}})).unwrap();
        if let Ok(response) =
            vaultsyncd_platform_windows::ipc::Endpoint::current("vaultsyncd-agent")
                .unwrap()
                .request(&request)
                .await
        {
            if let Some(result) = response.result {
                return result;
            }
        }
        assert!(Instant::now() < end, "actual IPC startup timeout");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn readonly(path: &Path) -> sqlx::SqliteConnection {
    sqlx::SqliteConnection::connect_with(
        &sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true),
    )
    .await
    .unwrap()
}
async fn counts(path: &Path) -> (i64, i64, i64) {
    let mut conn = readonly(path).await;
    let value=sqlx::query_as("SELECT (SELECT count(*) FROM local_intents),(SELECT count(*) FROM upload_resumes),(SELECT count(*) FROM local_entries WHERE dirty!='clean')").fetch_one(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    value
}
async fn listing(client: &ApiClient<ReqwestTransport>, root: &str) -> Vec<SnapshotEntry> {
    let (s, _) = sync::snapshot(client, root).await.unwrap();
    sync::snapshot_page(client, root, &s.snapshot_id, None)
        .await
        .unwrap()
        .entries
}
async fn failed_export(binding: &str, destination: &Path) -> Value {
    let response = request(
        "export_recovery",
        json!({"binding_id":binding,"destination":destination}),
    )
    .await;
    assert_eq!(
        response.kind,
        ResponseKind::Error,
        "unsafe/incomplete export succeeded"
    );
    assert!(response.result.is_none());
    serde_json::to_value(response.error.unwrap()).unwrap()
}
fn verify_package(path: &Path, binding: &str) -> (Value, Value) {
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], 1);
    assert_eq!(manifest["binding"], binding);
    let mut names = std::collections::HashSet::new();
    for record in manifest["files"].as_array().unwrap() {
        let name = record["name"].as_str().unwrap();
        assert!(
            !name.is_empty()
                && !name.contains(['/', '\\', ':'])
                && name != ".."
                && names.insert(name)
        );
        let bytes = std::fs::read(path.join(name)).unwrap();
        assert_eq!(record["size"].as_u64(), Some(bytes.len() as u64));
        assert_eq!(record["sha256"], hash(&bytes));
    }
    assert!(names.contains("state.json"));
    let state: Value =
        serde_json::from_slice(&std::fs::read(path.join("state.json")).unwrap()).unwrap();
    assert_eq!(
        state["binding"]["id"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
        binding
    );
    (manifest, state)
}
fn retrieve(
    package: &Path,
    manifest: &Value,
    target: &Path,
    name: &str,
    bytes: &[u8],
    kind: &str,
) -> Value {
    let digest = hash(bytes);
    let item = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["sha256"] == digest && f["source"]["kind"] == kind)
        .expect("every unique content must be readable in the expected export role");
    let source = package.join(item["name"].as_str().unwrap());
    let destination = target.join(name);
    // Deliberately manual, create-new copying into a separate directory. Never execute content.
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .unwrap();
    std::io::copy(&mut File::open(source).unwrap(), &mut output).unwrap();
    output.sync_all().unwrap();
    drop(output);
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    json!({"package_name":item["name"],"restored_path":destination,"sha256":digest,"kind":kind})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_agent_crash_exports_every_unconfirmed_byte_before_recovery() {
    let fixture_path = PathBuf::from(
        std::env::var_os("VAULTSYNCD_S084_FIXTURE").expect("native run fixture required"),
    );
    let fixture: Value = serde_json::from_slice(&std::fs::read(&fixture_path).unwrap()).unwrap();
    let exe =
        PathBuf::from(std::env::var_os("VAULTSYNCD_AGENT_EXE").expect("product agent required"));
    let base = fixture_path
        .parent()
        .unwrap()
        .join(format!("run-s084-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    for directory in ["state", "root", "cache", "manual-recovered"] {
        std::fs::create_dir(base.join(directory)).unwrap();
    }
    let root = fixture["root"].as_str().unwrap();
    let binding = id();
    let server = id();
    let config = base.join("agent.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"server":{"id":server,"url":fixture["url"],"development_http":true},"device_id":id(),"state_dir":base.join("state"),"poll_millis":250,"bindings":[]})).unwrap()).unwrap();
    let mut agent = Agent::launch(&exe, &config, &base.join("first-agent.log"));
    status(&mut agent).await;
    ok(
        "login",
        json!({"username":fixture["login"],"password":fixture["password"]}),
    )
    .await;
    ok("create_binding",json!({"binding_id":binding,"root_id":root,"local_path":base.join("root"),"cache_path":base.join("cache"),"mode":"full"})).await;
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let s = status(&mut agent).await;
        if s.bindings.as_ref().is_some_and(|rows| {
            rows.iter()
                .any(|b| b.binding_id == binding && b.synchronized)
        }) {
            break;
        }
        assert!(Instant::now() < end, "first import did not complete: {s:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        std::fs::read(base.join("root/dirty.txt")).unwrap(),
        BASELINE
    );
    assert_eq!(std::fs::read(base.join("root/intent.txt")).unwrap(), INTENT);
    let issued: SessionIssued = serde_json::from_value(fixture["issued"].clone()).unwrap();
    let client =
        ApiClient::new(ClientConfig::new(fixture["url"].as_str().unwrap()).allow_plaintext_http())
            .unwrap();
    client
        .auth()
        .install(Credentials::from_issued(&issued).unwrap());
    let before = listing(&client, root).await;
    let dirty = before
        .iter()
        .find(|e| e.name == "dirty.txt")
        .unwrap()
        .clone();
    let intent = before
        .iter()
        .find(|e| e.name == "intent.txt")
        .unwrap()
        .clone();
    std::fs::write(base.join("root/dirty.txt"), UPLOAD).unwrap();
    let db = base.join("state/client.sqlite");
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let c = counts(&db).await;
        if c.1 > 0 && c.2 > 0 {
            break;
        }
        agent.alive();
        assert!(
            Instant::now() < end,
            "real fixed upload resume not persisted: {c:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ok("pause_binding", json!({"binding_id":binding})).await;
    std::fs::write(base.join("root/dirty.txt"), LATEST).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(base.join("root/intent.txt"))
        .unwrap();

    let (snapshot, _) = sync::snapshot(&client, root).await.unwrap();
    let move_request = OperationRequest {
        device_id: id(),
        op_id: id(),
        server_epoch: snapshot.server_epoch,
        kind: OperationKind::Move,
        entry_id: Some(intent.entry_id.clone()),
        base_revision: Some(intent.revision.clone()),
        name: Some("remote-renamed.txt".into()),
        parent_id: intent.parent_id.clone(),
        version_id: None,
        base_version_id: None,
        conflict_policy: None,
        expected_absent: None,
        local_generation: None,
        subtree_token: None,
        upload_id: None,
    };
    assert_eq!(
        sync::submit(&client, root, &move_request)
            .await
            .unwrap()
            .state,
        vaultsyncd_protocol::generated::OperationState::Committed
    );
    ok("resume_binding", json!({"binding_id":binding})).await;
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let c = counts(&db).await;
        if c.0 > 0 {
            break;
        }
        agent.alive();
        assert!(
            Instant::now() < end,
            "real blocked move intent not persisted"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    agent.terminate(); // Real abnormal exit, only the PID owned by this test.
    std::fs::write(base.join("root/new-未上传.txt"), NEW).unwrap();
    let before_export = counts(&db).await;
    assert!(before_export.0 > 0 && before_export.1 > 0 && before_export.2 > 0);
    agent = Agent::launch(&exe, &config, &base.join("restarted-agent.log"));
    let s = status(&mut agent).await;
    assert!(
        !s.bindings
            .as_ref()
            .unwrap()
            .iter()
            .find(|b| b.binding_id == binding)
            .unwrap()
            .synchronized
    );
    let end = Instant::now() + Duration::from_secs(30);
    while !std::fs::read_to_string(base.join("restarted-agent.log"))
        .unwrap()
        .contains("event=binding-start-blocked")
    {
        agent.alive();
        assert!(
            Instant::now() < end,
            "restart did not retain blocked namespace intent"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let existing = base.join("existing");
    std::fs::create_dir(&existing).unwrap();
    std::fs::write(existing.join("keep.txt"), b"do not overwrite").unwrap();
    let existing_error = failed_export(&binding, &existing).await;
    assert_eq!(
        std::fs::read(existing.join("keep.txt")).unwrap(),
        b"do not overwrite"
    );
    let protected = base.join("cache/recovery-inside");
    let overlap_error = failed_export(&binding, &protected).await;
    assert!(!protected.exists());
    let busy = OpenOptions::new()
        .write(true)
        .share_mode(0)
        .open(base.join("root/dirty.txt"))
        .unwrap();
    let incomplete = base.join("incomplete-busy");
    let busy_error = failed_export(&binding, &incomplete).await;
    assert!(!incomplete.join("manifest.json").exists());
    drop(busy);
    assert_eq!(std::fs::read(base.join("root/dirty.txt")).unwrap(), LATEST);
    assert_eq!(counts(&db).await, before_export);
    let release = request(
        "release_entry",
        json!({"binding_id":binding,"entry_id":dirty.entry_id,"operation_id":id()}),
    )
    .await;
    assert_eq!(release.kind, ResponseKind::Error);
    let package = base.join("recovery-package");
    let exported = ok(
        "export_recovery",
        json!({"binding_id":binding,"destination":package}),
    )
    .await;
    let (manifest, state) = verify_package(&package, &binding);
    assert_eq!(
        exported.exported_files.unwrap().parse::<usize>().unwrap(),
        manifest["files"].as_array().unwrap().len()
    );
    let baseline = state["baseline_entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["entry_id"] == dirty.entry_id)
        .unwrap();
    assert_eq!(
        baseline["revision"].as_i64(),
        Some(dirty.revision.parse().unwrap())
    );
    assert_eq!(dirty.hash, Some(hash(BASELINE)));
    assert_eq!(
        baseline["content_version_id"],
        dirty.version_id.clone().unwrap()
    );
    assert!(!state["local_intents"].as_array().unwrap().is_empty());
    assert!(!state["upload_resumes"].as_array().unwrap().is_empty());
    assert!(!state["pending_ops"].as_array().unwrap().is_empty());
    let manual = base.join("manual-recovered");
    let recovered = vec![
        retrieve(
            &package,
            &manifest,
            &manual,
            "latest-dirty.txt",
            LATEST,
            "current",
        ),
        retrieve(
            &package,
            &manifest,
            &manual,
            "fixed-upload.txt",
            UPLOAD,
            "snapshot",
        ),
        retrieve(
            &package,
            &manifest,
            &manual,
            "new-未上传.txt",
            NEW,
            "candidate",
        ),
        retrieve(
            &package,
            &manifest,
            &manual,
            "intent-source.txt",
            INTENT,
            "current",
        ),
    ];
    assert_eq!(std::fs::read(base.join("root/dirty.txt")).unwrap(), LATEST);
    assert_eq!(
        std::fs::read(base.join("root/new-未上传.txt")).unwrap(),
        NEW
    );
    assert_eq!(std::fs::read(base.join("root/intent.txt")).unwrap(), INTENT);
    assert!(!base.join("root/remote-renamed.txt").exists());
    assert_eq!(counts(&db).await, before_export);
    let text = std::fs::read_to_string(package.join("state.json")).unwrap();
    for secret in [
        fixture["password"].as_str().unwrap(),
        issued.access_token.as_str(),
        issued.refresh_token.as_str(),
    ] {
        assert!(!text.contains(secret));
    }
    let remote = listing(&client, root).await;
    assert_eq!(
        remote.iter().find(|e| e.name == "dirty.txt").unwrap().hash,
        Some(hash(BASELINE))
    );
    assert!(!remote.iter().any(|e| e.name == "new-未上传.txt"));
    let report = json!({"ok":true,"binding":binding,"root":root,"run":base,"package":package,"manual_recovered":recovered,"baseline_version":dirty.version_id,"baseline_hash":hash(BASELINE),"durable_counts_before_and_after":before_export,"failures":{"existing_destination":existing_error,"protected_destination":overlap_error,"busy_source":busy_error},"abnormal_exit":true,"real_pending_intent":true,"real_upload_part_transport_failure":true,"sources_unchanged":true,"first_export_precedes_recovery":true,"registered_root_preserved":true,"installation_uninstallation":"not exercised; S099 pending"});
    ok("logout", json!({})).await;
    agent.terminate();
    drop(lock);
    // Preserve the registered root, database, cache and package. Unregistering an unsettled
    // Cloud Files root is not part of recovery and may delete partial content.
    std::fs::write(
        fixture_path.parent().unwrap().join("result.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!(
        "check=pass real_agent_abnormal_exit_export_preserves_every_unconfirmed_hash_baseline_and_intent"
    );
    println!(
        "RECOVERY_RESULT {}",
        serde_json::to_string(&report).unwrap()
    );
}
