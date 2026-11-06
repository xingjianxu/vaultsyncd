//! S072 acceptance: every operation traverses the actual product agent's authenticated named pipe.
use super::*;
use vaultsyncd_local_ipc::generated::{GetStatusResult, ManagementResult, Response, ResponseKind};

async fn request(name: &str, parameters: serde_json::Value) -> Response {
    let wire = serde_json::json!({"request_id":identity(),"ipc_version":{"major":1,"minor":2},"command":{"name":name,"parameters":parameters}});
    let request = serde_json::from_value(wire).unwrap();
    let response = vaultsyncd_platform_windows::ipc::Endpoint::current("vaultsyncd-agent")
        .unwrap()
        .request(&request)
        .await
        .unwrap_or_else(|e| panic!("{name}: product management response unavailable: {e}"));
    response.validate().unwrap();
    response
}
async fn ok(name: &str, p: serde_json::Value) -> ManagementResult {
    let response = request(name, p).await;
    assert_eq!(
        response.kind,
        ResponseKind::Ok,
        "{name}: {:?}",
        response.error
    );
    response
        .result
        .unwrap()
        .management
        .expect("typed real management result")
}
async fn bad(name: &str, p: serde_json::Value) {
    let response = request(name, p).await;
    assert_eq!(
        response.kind,
        ResponseKind::Error,
        "{name} must reject unsafe inputs"
    );
}
fn launch(exe: &Path, config: &Path, log: &Path) -> ChildGuard {
    ChildGuard(
        Command::new(exe)
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(log).unwrap())
            .spawn()
            .unwrap(),
    )
}
async fn status(child: &mut ChildGuard) -> GetStatusResult {
    ipc_status(child).await
}
async fn until_status(child: &mut ChildGuard, test: impl Fn(&GetStatusResult) -> bool) {
    let end = Instant::now() + Duration::from_secs(25);
    loop {
        let value = status(child).await;
        if test(&value) {
            break;
        }
        assert!(
            Instant::now() < end,
            "management status did not converge: {value:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
fn unregister(path: &Path, binding: &str, root: &str) {
    use vaultsyncd_platform_windows::cfapi::{
        adapter::{Adapter, MetadataOnly},
        catalog::Catalog,
    };
    let mut adapter = Adapter::connect(
        path,
        Catalog::new(
            vaultsyncd_domain::ids::BindingId::from_hex(binding).unwrap(),
            vaultsyncd_domain::ids::RootId::from_hex(root).unwrap(),
        ),
        Arc::new(MetadataOnly),
    )
    .unwrap();
    adapter.unregister().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_management_every_command() {
    let fixture_path =
        PathBuf::from(std::env::var_os("VAULTSYNCD_PROCESS_FIXTURE").expect("real server fixture"));
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture_path).unwrap()).unwrap();
    let exe = PathBuf::from(std::env::var_os("VAULTSYNCD_AGENT_EXE").expect("product agent"));
    let base = fixture_path
        .parent()
        .unwrap()
        .join(format!("run-management-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    for p in ["state", "root", "cache"] {
        std::fs::create_dir(base.join(p)).unwrap();
    }
    let server = identity();
    let binding = identity();
    let root = fixture["root"].as_str().unwrap();
    let entry = fixture["entry"]["entry_id"].as_str().unwrap();
    let config_path = base.join("agent.json");
    let config = serde_json::json!({"server":{"id":server,"url":fixture["url"],"development_http":true},"device_id":identity(),"state_dir":base.join("state"),"poll_millis":250,"bindings":[]});
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut child = launch(&exe, &config_path, &base.join("agent.log"));
    let initial = status(&mut child).await;
    assert!(initial.bindings.unwrap().is_empty());
    assert!(
        !ok("get_connection", serde_json::json!({}))
            .await
            .connection
            .unwrap()
            .authenticated
    );
    assert!(
        ok("list_bindings", serde_json::json!({}))
            .await
            .bindings
            .unwrap()
            .is_empty()
    );
    ok(
        "set_connection",
        serde_json::json!({"server_id":server,"server_url":fixture["url"]}),
    )
    .await;
    bad(
        "set_connection",
        serde_json::json!({"server_id":server,"server_url":"http://remote.invalid/"}),
    )
    .await;
    bad(
        "login",
        serde_json::json!({"username":"s052-owner","password":"wrong-password-not-logged"}),
    )
    .await;
    ok(
        "login",
        serde_json::json!({"username":"s052-owner","password":"S052-Correct-Horse-Battery-9!"}),
    )
    .await;
    assert!(
        ok("get_connection", serde_json::json!({}))
            .await
            .connection
            .unwrap()
            .authenticated
    );
    assert!(
        ok("list_roots", serde_json::json!({}))
            .await
            .items
            .unwrap()
            .iter()
            .any(|i| i.id == root)
    );
    bad("create_binding",serde_json::json!({"binding_id":binding,"root_id":root,"local_path":base.join("state"),"cache_path":base.join("cache"),"mode":"on_demand"})).await;
    ok("create_binding",serde_json::json!({"binding_id":binding,"root_id":root,"local_path":base.join("root"),"cache_path":base.join("cache"),"mode":"on_demand"})).await;
    until_status(&mut child, |s| {
        s.bindings
            .as_ref()
            .is_some_and(|b| b.len() == 1 && b[0].metadata_only != "0")
    })
    .await;
    let infos = ok("list_bindings", serde_json::json!({}))
        .await
        .bindings
        .unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].binding_id, binding);
    bad(
        "set_connection",
        serde_json::json!({"server_id":identity(),"server_url":fixture["url"]}),
    )
    .await;
    // Same-server endpoint/trust changes really rebuild the native/HTTP adapters and retain bindings.
    ok(
        "set_connection",
        serde_json::json!({"server_id":server,"server_url":fixture["url"]}),
    )
    .await;
    ok(
        "download_entry",
        serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity()}),
    )
    .await;
    assert_eq!(
        std::fs::read(base.join("root/file.txt")).unwrap(),
        fixture["payload"].as_str().unwrap().as_bytes()
    );
    ok(
        "release_entry",
        serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity()}),
    )
    .await;
    let native = vaultsyncd_platform_windows::files::RootHandle::open(&base.join("root"))
        .unwrap()
        .inspect("file.txt")
        .unwrap();
    assert_eq!(native.cloud.unwrap().on_disk, 0);
    let before = status(&mut child).await.snapshot_version.unwrap();
    let binding_for_pause = binding.clone();
    let push = tokio::spawn(async move {
        request(
            "subscribe_status",
            serde_json::json!({"after_version":before}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    ok(
        "pause_binding",
        serde_json::json!({"binding_id":binding_for_pause}),
    )
    .await;
    let pushed = push.await.unwrap().result.unwrap();
    assert!(pushed.bindings.unwrap()[0].paused);
    bad(
        "download_entry",
        serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity()}),
    )
    .await;
    ok("resume_binding", serde_json::json!({"binding_id":binding})).await;
    ok(
        "set_mode",
        serde_json::json!({"binding_id":binding,"mode":"full"}),
    )
    .await;
    until_status(&mut child, |s| {
        s.bindings.as_ref().is_some_and(|b| !b[0].mode_pending)
    })
    .await;
    assert_eq!(
        std::fs::read(base.join("root/file.txt")).unwrap(),
        fixture["payload"].as_str().unwrap().as_bytes()
    );
    bad(
        "release_entry",
        serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity()}),
    )
    .await;
    ok(
        "set_mode",
        serde_json::json!({"binding_id":binding,"mode":"on_demand"}),
    )
    .await;
    // The missing-push path always recovers all current facts, not an empty delta or fake success.
    let recovered = request(
        "subscribe_status",
        serde_json::json!({"after_version":"lost-process:0"}),
    )
    .await
    .result
    .unwrap();
    assert_eq!(
        recovered.management.unwrap().bindings.unwrap()[0].mode,
        "on_demand"
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
    let old = listing(&client, root)
        .await
        .into_iter()
        .find(|e| e.entry_id == entry)
        .unwrap();
    let mapping = WritableMapping::open(&base.join("root/file.txt"));
    remote_put(
        &client,
        root,
        &old,
        b"S072 remote version behind a real writable mapping",
    )
    .await;
    let end = Instant::now() + Duration::from_secs(20);
    let problem = loop {
        let problems = ok("list_problems", serde_json::json!({"binding_id":binding}))
            .await
            .problems
            .unwrap();
        if let Some(problem) = problems.into_iter().find(|p| p.retryable) {
            break problem;
        }
        assert!(
            Instant::now() < end,
            "real blocked apply must expose a persistent problem"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    ok(
        "retry_problem",
        serde_json::json!({"binding_id":binding,"operation_id":problem.problem_id}),
    )
    .await;
    drop(mapping);
    until_file(
        &base.join("root/file.txt"),
        Some(b"S072 remote version behind a real writable mapping"),
    )
    .await;
    until_durable_idle(&base.join("state/client.sqlite")).await;
    let history = ok(
        "list_history",
        serde_json::json!({"binding_id":binding,"entry_id":entry}),
    )
    .await
    .items
    .unwrap();
    assert!(history.len() >= 2);
    let current = listing(&client, root)
        .await
        .into_iter()
        .find(|e| e.entry_id == entry)
        .unwrap();
    let original = fixture["entry"]["version_id"].as_str().unwrap();
    ok("restore_entry",serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity(),"base_revision":current.revision,"version_id":original})).await;
    until_file(
        &base.join("root/file.txt"),
        Some(fixture["payload"].as_str().unwrap().as_bytes()),
    )
    .await;
    let current = listing(&client, root)
        .await
        .into_iter()
        .find(|e| e.entry_id == entry)
        .unwrap();
    let (s, _) = sync::snapshot(&client, root).await.unwrap();
    sync::submit(
        &client,
        root,
        &namespace_request(&current, OperationKind::Delete, s.server_epoch, None),
    )
    .await
    .unwrap();
    until_file(&base.join("root/file.txt"), None).await;
    let trash = ok("list_trash", serde_json::json!({"binding_id":binding}))
        .await
        .items
        .unwrap();
    let item = trash
        .iter()
        .find(|i| i.entry_id.as_deref() == Some(entry))
        .unwrap();
    ok("restore_entry",serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity(),"base_revision":item.revision})).await;
    until_file(
        &base.join("root/file.txt"),
        Some(fixture["payload"].as_str().unwrap().as_bytes()),
    )
    .await;
    ok("pause_binding", serde_json::json!({"binding_id":binding})).await;
    std::fs::write(
        base.join("root/unique-unconfirmed.bin"),
        b"S072 unique local recovery bytes",
    )
    .unwrap();
    let export = base.join("recovery");
    assert!(
        ok(
            "export_recovery",
            serde_json::json!({"binding_id":binding,"destination":export})
        )
        .await
        .exported_files
        .unwrap()
        .parse::<u64>()
        .unwrap()
            > 1
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(export.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["files"].as_array().unwrap().iter().any(|f| {
        std::fs::read(export.join(f["name"].as_str().unwrap())).unwrap()
            == b"S072 unique local recovery bytes"
    }));
    bad(
        "export_recovery",
        serde_json::json!({"binding_id":binding,"destination":export}),
    )
    .await;
    let detached = ok("detach_binding", serde_json::json!({"binding_id":binding})).await;
    assert!(detached.unconfirmed.is_some());
    assert_eq!(
        std::fs::read(base.join("root/unique-unconfirmed.bin")).unwrap(),
        b"S072 unique local recovery bytes"
    );
    bad("resume_binding", serde_json::json!({"binding_id":binding})).await;
    drop(child);
    child = launch(&exe, &config_path, &base.join("restart.log"));
    let restarted = status(&mut child).await;
    assert!(restarted.bindings.unwrap()[0].detached);
    ok(
        "export_recovery",
        serde_json::json!({"binding_id":binding,"destination":base.join("recovery-after-restart")}),
    )
    .await;
    assert!(
        ok("get_connection", serde_json::json!({}))
            .await
            .connection
            .unwrap()
            .authenticated
    );
    ok("logout", serde_json::json!({})).await;
    assert!(
        !ok("get_connection", serde_json::json!({}))
            .await
            .connection
            .unwrap()
            .authenticated
    );
    // Every advertised handler has a real negative sample; no UI hiding or Unsupported fallback.
    for name in [
        "get_status",
        "subscribe_status",
        "list_bindings",
        "create_binding",
        "pause_binding",
        "resume_binding",
        "download_entry",
        "release_entry",
        "list_problems",
        "retry_problem",
        "detach_binding",
        "export_recovery",
        "get_connection",
        "set_connection",
        "login",
        "logout",
        "list_roots",
        "set_mode",
        "list_history",
        "list_trash",
        "restore_entry",
    ] {
        bad(name, serde_json::json!({"mode":"invalid"})).await;
    }
    drop(child);
    let logs = std::fs::read_to_string(base.join("agent.log")).unwrap();
    assert!(
        !logs.contains("wrong-password-not-logged")
            && !logs.contains("S052-Correct-Horse-Battery-9!")
    );
    credentials::CredentialVault::new(&server)
        .unwrap()
        .erase()
        .unwrap();
    assert_eq!(
        std::fs::read(base.join("root/file.txt")).unwrap(),
        fixture["payload"].as_str().unwrap().as_bytes()
    );
    for name in ["file.txt", "unique-unconfirmed.bin"] {
        std::fs::remove_file(base.join("root").join(name)).unwrap();
    }
    unregister(&base.join("root"), &binding, root);
    std::fs::remove_dir_all(&base).unwrap();
    println!(
        "check=pass s072_all_21_handlers_real_agent_named_pipe_effects_errors_versions_recovery_and_credential_redaction"
    );
}
