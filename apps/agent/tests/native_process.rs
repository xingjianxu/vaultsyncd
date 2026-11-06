//! Real headless process, Windows NTFS/CFAPI/WinCred, and a real Linux server bridge.
#![cfg(windows)]
#![allow(unsafe_code)]
use rand_core::{OsRng, RngCore};
use std::{
    io::Write,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use vaultsyncd_http_client::{ApiClient, ClientConfig, Credentials, sync};
use vaultsyncd_protocol::generated::{
    ConflictPolicy, OperationKind, OperationRequest, SessionIssued, SnapshotEntry,
};
use vaultsyncd_transfer::{
    ports::{PortError, PortFuture, SnapshotSource},
    upload::{self, UploadOptions},
};
#[path = "../src/credentials.rs"]
mod credentials;

fn identity() -> String {
    let mut b = [0; 16];
    OsRng.fill_bytes(&mut b);
    vaultsyncd_domain::ids::ServerId::from_bytes(b).to_string()
}
fn invoke(exe: &Path, config: &Path, extra: &[&str], input: Option<&[u8]>) -> std::process::Output {
    let mut cmd = Command::new(exe);
    cmd.arg("--config")
        .arg(config)
        .args(extra)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    child.wait_with_output().unwrap()
}
fn success(output: std::process::Output) {
    assert!(
        output.status.success(),
        "exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}
async fn listing(
    client: &ApiClient<vaultsyncd_http_client::ReqwestTransport>,
    root: &str,
) -> Vec<SnapshotEntry> {
    let (s, _) = sync::snapshot(client, root).await.unwrap();
    let mut cursor = None;
    let mut all = Vec::new();
    loop {
        let page = sync::snapshot_page(client, root, &s.snapshot_id, cursor.as_deref())
            .await
            .unwrap();
        all.extend(page.entries);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return all;
        }
    }
}
struct Bytes(Vec<u8>);
#[test]
fn native_cleanup_only_s070_synthetic_failed_runs() {
    if std::env::var_os("VAULTSYNCD_PROCESS_FIXTURE").is_none() {
        return;
    }
    let current = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let allowed: Vec<PathBuf> = serde_json::from_str(
        &std::env::var("VAULTSYNCD_PROCESS_CLEANUP_ALLOWLIST").unwrap_or_else(|_| "[]".into()),
    )
    .unwrap();
    for driver in std::fs::read_dir("C:/vaultsyncd-lab")
        .unwrap()
        .filter_map(Result::ok)
    {
        if !driver
            .file_name()
            .to_string_lossy()
            .starts_with("s070-driver-")
            || driver.path() == current
        {
            continue;
        }
        for run in std::fs::read_dir(driver.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("run-process-"))
        {
            let path = run.path();
            let config_path = path.join("agent.json");
            if !config_path.exists() {
                if !std::fs::symlink_metadata(path.join("root"))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    assert!(
                        allowed.contains(&path),
                        "preserve unknown run without config: {}",
                        path.display()
                    );
                    let root =
                        vaultsyncd_platform_windows::files::RootHandle::open(&path.join("root"))
                            .unwrap();
                    let report = root.scan();
                    assert!(
                        report.permits_missing_comparison() && report.entries.is_empty(),
                        "allowlisted partial cleanup root must be empty"
                    );
                    drop(root);
                    use std::os::windows::ffi::OsStrExt;
                    #[link(name = "cldapi")]
                    unsafe extern "system" {
                        fn CfUnregisterSyncRoot(path: *const u16) -> i32;
                    }
                    let wide: Vec<u16> = path
                        .join("root")
                        .as_os_str()
                        .encode_wide()
                        .chain(Some(0))
                        .collect();
                    let status = unsafe { CfUnregisterSyncRoot(wide.as_ptr()) };
                    assert!(
                        status >= 0 || status as u32 == 0x80070186,
                        "empty allowlisted root unregister {status:08x}"
                    );
                }
                std::fs::remove_dir_all(&path).unwrap();
                continue;
            }
            let config = vaultsyncd_agent::config::AgentConfig::load(&config_path)
                .unwrap_or_else(|e| panic!("{}: {e}", config_path.display()));
            assert_eq!(config.bindings.len(), 1);
            assert_eq!(config.state_dir, path.join("state"));
            assert_eq!(config.bindings[0].path, path.join("root"));
            assert_eq!(config.bindings[0].cache, path.join("cache"));
            credentials::CredentialVault::new(&config.server.id)
                .unwrap()
                .erase()
                .unwrap();
            let mut adapter = vaultsyncd_platform_windows::cfapi::adapter::Adapter::connect(
                &path.join("root"),
                vaultsyncd_platform_windows::cfapi::catalog::Catalog::new(
                    vaultsyncd_domain::ids::BindingId::from_hex(&config.bindings[0].id).unwrap(),
                    vaultsyncd_domain::ids::RootId::from_hex(&config.bindings[0].root_id).unwrap(),
                ),
                Arc::new(vaultsyncd_platform_windows::cfapi::adapter::MetadataOnly),
            )
            .unwrap();
            let root =
                vaultsyncd_platform_windows::files::RootHandle::open(&path.join("root")).unwrap();
            let report = root.scan();
            assert!(report.permits_missing_comparison());
            let mut entries = report.entries;
            entries.sort_by_key(|e| std::cmp::Reverse(e.path.matches('/').count()));
            for entry in entries {
                let meta = entry.metadata.unwrap();
                let target = path.join("root").join(entry.path);
                if meta.directory {
                    std::fs::remove_dir(target).unwrap();
                } else {
                    std::fs::remove_file(target).unwrap();
                }
            }
            drop(root);
            adapter.unregister().unwrap();
            drop(adapter);
            std::fs::remove_dir_all(path).unwrap();
        }
        std::fs::remove_dir_all(driver.path()).unwrap();
    }
    println!("check=pass only_previous_s070_synthetic_runs_and_credentials_cleaned");
}
async fn remote_put(
    client: &ApiClient<vaultsyncd_http_client::ReqwestTransport>,
    root: &str,
    old: &SnapshotEntry,
    bytes: &[u8],
) {
    let upload = upload::run(
        &Bytes(bytes.to_vec()),
        &vaultsyncd_http_client::uploads::UploadEndpoint::new(client.clone(), root).unwrap(),
        None,
        &UploadOptions::default(),
    )
    .await
    .unwrap();
    let (snapshot, _) = sync::snapshot(client, root).await.unwrap();
    let mut request =
        namespace_request(old, OperationKind::PutContent, snapshot.server_epoch, None);
    request.base_version_id = old.version_id.clone();
    request.upload_id = Some(upload.session);
    request.local_generation = Some("1".into());
    sync::submit(client, root, &request).await.unwrap();
}
struct ChildGuard(std::process::Child);
struct WritableMapping {
    mapping: *mut std::ffi::c_void,
    view: *mut std::ffi::c_void,
}
impl WritableMapping {
    fn open(path: &Path) -> Self {
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateFileMappingW(
                file: *mut std::ffi::c_void,
                security: *const std::ffi::c_void,
                protect: u32,
                high: u32,
                low: u32,
                name: *const u16,
            ) -> *mut std::ffi::c_void;
            fn MapViewOfFile(
                mapping: *mut std::ffi::c_void,
                access: u32,
                high: u32,
                low: u32,
                length: usize,
            ) -> *mut std::ffi::c_void;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let mapping = unsafe {
            CreateFileMappingW(
                file.as_raw_handle(),
                std::ptr::null(),
                4,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert!(!mapping.is_null());
        let view = unsafe { MapViewOfFile(mapping, 2, 0, 0, 0) };
        assert!(!view.is_null());
        drop(file);
        Self { mapping, view }
    }
}
impl Drop for WritableMapping {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn UnmapViewOfFile(view: *mut std::ffi::c_void) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            UnmapViewOfFile(self.view);
            CloseHandle(self.mapping);
        }
    }
}
impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
async fn ipc_status(child: &mut ChildGuard) -> vaultsyncd_local_ipc::generated::GetStatusResult {
    use vaultsyncd_local_ipc::{
        generated::{Command, CommandName, Request},
        version,
    };
    let endpoint = vaultsyncd_platform_windows::ipc::Endpoint::current("vaultsyncd-agent").unwrap();
    let request = Request {
        request_id: "native-agent-status".into(),
        ipc_version: version::supported(),
        command: Command {
            name: CommandName::GetStatus,
        },
    };
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            child.try_wait().unwrap().is_none(),
            "agent must survive IPC requests"
        );
        match endpoint.request(&request).await {
            Ok(response) => {
                assert_eq!(response.request_id, request.request_id);
                let result = response.result.expect("real agent status");
                assert_eq!(result.agent_version, env!("CARGO_PKG_VERSION"));
                assert_eq!(result.ipc_protocol, version::supported());
                assert!(
                    result.bindings.is_some(),
                    "identity-only stub is not real status"
                );
                return result;
            }
            Err(e) => assert!(Instant::now() < end, "real agent IPC unavailable: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn until_server(
    client: &ApiClient<vaultsyncd_http_client::ReqwestTransport>,
    root: &str,
    predicate: impl Fn(&[SnapshotEntry]) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let rows = listing(client, root).await;
        if predicate(&rows) {
            return;
        }
        assert!(Instant::now() < end, "namespace did not converge: {rows:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn until_file(path: &Path, bytes: Option<&[u8]>) {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let read = bytes.map(|_| std::fs::read(path));
        let matches = match bytes {
            Some(expected) => read
                .as_ref()
                .unwrap()
                .as_ref()
                .is_ok_and(|actual| actual == expected),
            None => !path.exists(),
        };
        if matches {
            return;
        }
        assert!(
            Instant::now() < end,
            "native path did not converge: {} actual={read:?} metadata={:?}",
            path.display(),
            vaultsyncd_platform_windows::files::RootHandle::open(path.parent().unwrap())
                .and_then(|r| r.inspect(path.file_name().unwrap().to_str().unwrap()))
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn until_durable_idle(path: &Path) {
    use sqlx::Connection;
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        // Read-only verification of the actual running agent; never open a second writer/migrator.
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .read_only(true);
        let mut conn = sqlx::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        let remaining:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM pending_ops),(SELECT count(*) FROM local_intents WHERE state='pending'),(SELECT count(*) FROM received_changes WHERE state!='applied')").fetch_one(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        if remaining == (0, 0, 0) {
            return;
        }
        assert!(
            Instant::now() < end,
            "durable work did not converge: {remaining:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
fn namespace_request(
    row: &SnapshotEntry,
    kind: OperationKind,
    epoch: String,
    name: Option<&str>,
) -> OperationRequest {
    OperationRequest {
        op_id: identity(),
        device_id: identity(),
        kind,
        server_epoch: epoch,
        entry_id: Some(row.entry_id.clone()),
        base_revision: Some(row.revision.clone()),
        base_version_id: None,
        upload_id: None,
        local_generation: Some("1".into()),
        conflict_policy: Some(ConflictPolicy::PreserveBoth),
        expected_absent: None,
        name: name.map(str::to_owned),
        parent_id: if kind == OperationKind::Move {
            row.parent_id.clone()
        } else {
            None
        },
        subtree_token: None,
        version_id: None,
    }
}
impl SnapshotSource for Bytes {
    fn identity(&self) -> &str {
        "remote-process-fixture"
    }
    fn total_size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, o: u64, n: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move { Ok(self.0[o as usize..(o + n) as usize].to_vec()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_headless_import_real_sync_queue_restart_and_single_instance() {
    let fixture_path =
        PathBuf::from(std::env::var_os("VAULTSYNCD_PROCESS_FIXTURE").expect("real server fixture"));
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture_path).unwrap()).unwrap();
    let exe =
        PathBuf::from(std::env::var_os("VAULTSYNCD_AGENT_EXE").expect("real agent executable"));
    let base = fixture_path
        .parent()
        .unwrap()
        .join(format!("run-process-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    for p in ["root", "cache", "state"] {
        std::fs::create_dir(base.join(p)).unwrap();
    }
    std::fs::write(base.join("root/local.txt"), b"local-first-import").unwrap();
    let server = identity();
    let device = identity();
    let binding = identity();
    let root = fixture["root"].as_str().unwrap();
    let mut config = serde_json::json!({"server":{"id":server,"url":fixture["url"],"development_http":true},"device_id":device,"state_dir":base.join("state"),"poll_millis":250,"bindings":[{"id":binding,"root_id":root,"path":base.join("root"),"cache":base.join("cache"),"mode":"full"}]});
    let config_path = base.join("agent.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let login = serde_json::json!({"login":"s052-owner","password":"S052-Correct-Horse-Battery-9!","device_label":"S070 native process"});
    success(invoke(
        &exe,
        &config_path,
        &["--login-stdin"],
        Some(&serde_json::to_vec(&login).unwrap()),
    ));
    let vault = credentials::CredentialVault::new(&server).unwrap();
    let saved = vault.load().unwrap().expect("persisted process login");
    let _: SessionIssued = serde_json::from_slice(&saved).unwrap();
    vault.store(&saved).unwrap();
    println!("check=pass real_process_login_to_current_user_wincred");
    success(invoke(&exe, &config_path, &["--once"], None));
    assert_eq!(
        std::fs::read(base.join("root/file.txt")).unwrap(),
        fixture["payload"].as_str().unwrap().as_bytes()
    );
    let client =
        ApiClient::new(ClientConfig::new(fixture["url"].as_str().unwrap()).allow_plaintext_http())
            .unwrap();
    client.auth().install(
        Credentials::from_issued(
            &serde_json::from_value::<SessionIssued>(fixture["issued"].clone()).unwrap(),
        )
        .unwrap(),
    );
    assert!(
        listing(&client, root)
            .await
            .iter()
            .any(|r| r.name == "local.txt")
    );
    println!("check=pass no_gui_first_merge_upload_download");
    std::fs::write(base.join("root/local.txt"), b"new-local-before-restart").unwrap();
    config["server"]["url"] = "http://127.0.0.1:1".into();
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let offline = invoke(&exe, &config_path, &["--once"], None);
    assert!(!offline.status.success());
    let db = vaultsyncd_client_store::db::Database::open(base.join("state/client.sqlite"))
        .await
        .unwrap();
    let pending: Vec<u8> =
        sqlx::query_scalar("SELECT op_id FROM pending_ops WHERE kind='upload_content'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    db.close().await;
    // Query a real running offline agent: durable dirty/pending facts must not become green.
    let log = std::fs::File::create(base.join("offline-ipc.log")).unwrap();
    let mut offline_agent = ChildGuard(
        Command::new(&exe)
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let status = ipc_status(&mut offline_agent).await;
    let statuses = status.bindings.unwrap();
    assert_eq!(statuses.len(), 1);
    let status = &statuses[0];
    assert_eq!(status.binding_id, binding);
    assert!(status.dirty.parse::<u64>().unwrap() > 0);
    assert!(status.pending.parse::<u64>().unwrap() > 0);
    assert!(!status.synchronized);
    drop(offline_agent);
    println!("check=pass real_agent_ipc_exposes_durable_dirty_pending_not_committed");
    config["server"]["url"] = fixture["url"].clone();
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let output = invoke(&exe, &config_path, &["--once"], None);
        if output.status.success() {
            let db = vaultsyncd_client_store::db::Database::open(base.join("state/client.sqlite"))
                .await
                .unwrap();
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_ops WHERE op_id=?")
                .bind(&pending)
                .fetch_one(db.pool())
                .await
                .unwrap();
            db.close().await;
            if count == 0 {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "queue restart: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let op = vaultsyncd_domain::ids::OpId::from_bytes(pending.try_into().unwrap());
    let ack = sync::operation(&client, root, &op.to_string())
        .await
        .unwrap();
    assert_eq!(
        ack.state,
        vaultsyncd_protocol::generated::OperationState::Committed
    );
    println!("check=pass real_sqlite_queue_restart_reuses_original_operation");
    let rows = listing(&client, root).await;
    let old = rows.iter().find(|r| r.name == "file.txt").unwrap();
    let mut bytes = Bytes(b"real-remote-next-version".to_vec());
    let upload = upload::run(
        &bytes,
        &vaultsyncd_http_client::uploads::UploadEndpoint::new(client.clone(), root).unwrap(),
        None,
        &UploadOptions::default(),
    )
    .await
    .unwrap();
    let (snapshot, _) = sync::snapshot(&client, root).await.unwrap();
    let request = OperationRequest {
        op_id: identity(),
        device_id: identity(),
        kind: OperationKind::PutContent,
        server_epoch: snapshot.server_epoch,
        entry_id: Some(old.entry_id.clone()),
        base_revision: Some(old.revision.clone()),
        base_version_id: old.version_id.clone(),
        upload_id: Some(upload.session.clone()),
        local_generation: Some("1".into()),
        conflict_policy: Some(ConflictPolicy::PreserveBoth),
        expected_absent: None,
        name: None,
        parent_id: None,
        subtree_token: None,
        version_id: None,
    };
    sync::submit(&client, root, &request).await.unwrap();
    success(invoke(&exe, &config_path, &["--once"], None));
    assert_eq!(std::fs::read(base.join("root/file.txt")).unwrap(), bytes.0);
    println!("check=pass real_remote_version_download_controlled_publish");
    std::fs::write(base.join("root/file.txt"), b"late-local-conflicting-edit").unwrap();
    config["server"]["url"] = "http://127.0.0.1:1".into();
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert!(
        !invoke(&exe, &config_path, &["--once"], None)
            .status
            .success()
    );
    let db = vaultsyncd_client_store::db::Database::open(base.join("state/client.sqlite"))
        .await
        .unwrap();
    let conflict_op: Vec<u8> =
        sqlx::query_scalar("SELECT op_id FROM pending_ops WHERE kind='upload_content'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    db.close().await;
    let head = listing(&client, root)
        .await
        .into_iter()
        .find(|r| r.name == "file.txt")
        .unwrap();
    remote_put(&client, root, &head, b"remote-wins-original-name").await;
    bytes = Bytes(b"remote-wins-original-name".to_vec());
    config["server"]["url"] = fixture["url"].clone();
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut attempts = String::new();
    let conflict_ack = loop {
        let output = invoke(&exe, &config_path, &["--once"], None);
        attempts.push_str(&String::from_utf8_lossy(&output.stderr));
        let op = vaultsyncd_domain::ids::OpId::from_bytes(conflict_op.clone().try_into().unwrap());
        if let Ok(ack) = sync::operation(&client, root, &op.to_string()).await {
            if ack.conflict.is_some()
                && std::fs::read(base.join("root/file.txt")).is_ok_and(|b| b == bytes.0)
            {
                break ack;
            }
        }
        assert!(
            Instant::now() < deadline,
            "conflict did not settle: {attempts}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    let sibling = conflict_ack
        .conflict
        .as_ref()
        .unwrap()
        .conflict_entry_id
        .clone();
    let copy = listing(&client, root)
        .await
        .into_iter()
        .find(|r| r.entry_id == sibling)
        .unwrap();
    assert_eq!(
        std::fs::read(base.join("root").join(&copy.name)).unwrap(),
        b"late-local-conflicting-edit"
    );
    success(invoke(&exe, &config_path, &["--once"], None));
    assert_eq!(
        listing(&client, root)
            .await
            .iter()
            .filter(|r| r.entry_id == sibling)
            .count(),
        1
    );
    println!("check=pass committed_conflict_canonical_copy_both_bytes_and_restart_identity");
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AllocConsole() -> i32;
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
    }
    unsafe {
        AllocConsole();
    }
    let log = std::fs::File::create(base.join("background.log")).unwrap();
    let mut first = ChildGuard(
        Command::new(&exe)
            .arg("--config")
            .arg(&config_path)
            .creation_flags(0x200)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let end = Instant::now() + Duration::from_secs(15);
    while !std::fs::read_to_string(base.join("background.log"))
        .unwrap()
        .contains("event=recovery-complete")
    {
        assert!(
            first.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(base.join("background.log")).unwrap()
        );
        assert!(Instant::now() < end);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let local_before = listing(&client, root)
        .await
        .into_iter()
        .find(|r| r.name == "local.txt")
        .unwrap();
    let initial_status = ipc_status(&mut first).await;
    assert_eq!(
        initial_status.bindings.as_ref().unwrap()[0].binding_id,
        binding
    );
    // Malformed and partial clients are isolated from this actual syncing process.
    use tokio::io::AsyncWriteExt;
    let endpoint = vaultsyncd_platform_windows::ipc::Endpoint::current("vaultsyncd-agent").unwrap();
    for bytes in [vec![0, 0], vec![0, 0, 0, 0], vec![0, 0, 0, 1, b'{']] {
        let mut pipe = endpoint.connect().await.unwrap();
        pipe.write_all(&bytes).await.unwrap();
        drop(pipe);
    }
    assert_eq!(ipc_status(&mut first).await.bindings.unwrap().len(), 1);
    println!("check=pass real_agent_ipc_same_user_status_after_malformed_and_partial_disconnects");
    let native_root =
        vaultsyncd_platform_windows::files::RootHandle::open(&base.join("root")).unwrap();
    let before_identity = native_root.inspect("local.txt").unwrap().identity;
    std::fs::rename(base.join("root/local.txt"), base.join("root/renamed.txt")).unwrap();
    assert_eq!(
        native_root.inspect("renamed.txt").unwrap().identity,
        before_identity
    );
    until_server(&client, root, |rows| {
        rows.iter().any(|r| {
            r.name == "renamed.txt"
                && r.entry_id == local_before.entry_id
                && r.version_id == local_before.version_id
        })
    })
    .await;
    println!("check=pass local_move_keeps_entry_and_fixed_version");
    let remote_before = listing(&client, root)
        .await
        .into_iter()
        .find(|r| r.name == "file.txt")
        .unwrap();
    let mapping = WritableMapping::open(&base.join("root/file.txt"));
    let (s, _) = sync::snapshot(&client, root).await.unwrap();
    sync::submit(
        &client,
        root,
        &namespace_request(
            &remote_before,
            OperationKind::Move,
            s.server_epoch,
            Some("remote-renamed.txt"),
        ),
    )
    .await
    .unwrap();
    {
        use sqlx::Connection;
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            let mut conn = sqlx::SqliteConnection::connect_with(
                &sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(base.join("state/client.sqlite"))
                    .read_only(true),
            )
            .await
            .unwrap();
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM local_intents WHERE kind='move' AND target_path='remote-renamed.txt'").fetch_one(&mut conn).await.unwrap();
            conn.close().await.unwrap();
            if count == 1 {
                break;
            }
            assert!(
                Instant::now() < end,
                "real blocked move intent was not persisted"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    first.kill().unwrap();
    first.wait().unwrap();
    drop(mapping);
    let restarted_log = std::fs::File::create(base.join("restarted.log")).unwrap();
    first = ChildGuard(
        Command::new(&exe)
            .arg("--config")
            .arg(&config_path)
            .creation_flags(0x200)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(restarted_log)
            .spawn()
            .unwrap(),
    );
    until_file(&base.join("root/remote-renamed.txt"), Some(&bytes.0)).await;
    println!("check=pass killed_real_process_recovers_open_move_before_sync");
    assert!(!base.join("root/file.txt").exists());
    println!("check=pass both_direction_moves_keep_entry_and_fixed_version");
    let remote = listing(&client, root)
        .await
        .into_iter()
        .find(|r| r.name == "remote-renamed.txt")
        .unwrap();
    let (s, _) = sync::snapshot(&client, root).await.unwrap();
    sync::submit(
        &client,
        root,
        &namespace_request(&remote, OperationKind::Delete, s.server_epoch, None),
    )
    .await
    .unwrap();
    until_file(&base.join("root/remote-renamed.txt"), None).await;
    assert!(
        std::fs::read_dir(base.join("cache"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("deleted-"))
            .any(|e| std::fs::read(e.path()).is_ok_and(|b| b == bytes.0))
    );
    std::fs::remove_file(base.join("root/renamed.txt")).unwrap();
    until_server(&client, root, |rows| {
        !rows.iter().any(|r| r.entry_id == local_before.entry_id)
    })
    .await;
    println!("check=pass both_direction_deletes_preserve_confirmed_native_object");
    std::fs::create_dir(base.join("root/nested")).unwrap();
    std::fs::write(base.join("root/nested/data.txt"), b"nested-new-file").unwrap();
    until_server(&client, root, |rows| {
        let parent = rows.iter().find(|r| r.name == "nested");
        parent.is_some_and(|p| {
            rows.iter()
                .any(|r| r.name == "data.txt" && r.parent_id.as_ref() == Some(&p.entry_id))
        })
    })
    .await;
    std::fs::remove_dir_all(base.join("root/nested")).unwrap();
    until_server(&client, root, |rows| {
        !rows
            .iter()
            .any(|r| r.name == "nested" || r.name == "data.txt")
    })
    .await;
    println!("check=pass native_directory_create_child_upload_and_fenced_subtree_delete");
    until_durable_idle(&base.join("state/client.sqlite")).await;
    println!("check=pass real_sqlite_queue_intents_and_received_batches_converged");
    let final_status = ipc_status(&mut first).await.bindings.unwrap().remove(0);
    assert_eq!(final_status.binding_id, binding);
    assert_eq!(final_status.pending, "0");
    assert_eq!(final_status.intents, "0");
    assert_eq!(final_status.event_lag, "0");
    println!("check=pass real_agent_ipc_tracks_queue_and_intent_convergence");
    let second = invoke(&exe, &config_path, &["--once"], None);
    assert_eq!(
        second.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("ALREADY_RUNNING"));
    println!("check=pass second_real_agent_refuses_before_database_open");
    assert_ne!(unsafe { GenerateConsoleCtrlEvent(1, first.id()) }, 0);
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = first.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < end);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        std::fs::read_to_string(base.join("restarted.log"))
            .unwrap()
            .contains("event=stopped")
    );
    println!("check=pass actual_agent_control_break_drains_and_releases_database");
    assert!(
        endpoint.connect().await.is_err(),
        "stopped agent must release its pipe"
    );
    drop(native_root);
    vault.erase().unwrap();
    let root_path = base.join("root");
    for item in std::fs::read_dir(&root_path).unwrap() {
        let p = item.unwrap().path();
        if p.is_dir() {
            std::fs::remove_dir_all(p).unwrap();
        } else {
            std::fs::remove_file(p).unwrap();
        }
    }
    let mut adapter = vaultsyncd_platform_windows::cfapi::adapter::Adapter::connect(
        &root_path,
        vaultsyncd_platform_windows::cfapi::catalog::Catalog::new(
            vaultsyncd_domain::ids::BindingId::from_hex(&binding).unwrap(),
            vaultsyncd_domain::ids::RootId::from_hex(root).unwrap(),
        ),
        Arc::new(vaultsyncd_platform_windows::cfapi::adapter::MetadataOnly),
    )
    .unwrap();
    adapter.unregister().unwrap();
    drop(adapter);
    let end = Instant::now() + Duration::from_secs(5);
    while let Err(e) = std::fs::remove_dir_all(&base) {
        assert!(Instant::now() < end, "cleanup {e:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
