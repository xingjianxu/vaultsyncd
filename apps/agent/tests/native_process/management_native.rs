//! S072 acceptance: every operation traverses the actual product agent's authenticated named pipe.
use super::*;
use vaultsyncd_local_ipc::generated::{
    GetStatusResult, ManagementResult, PROTOCOL_MAJOR, PROTOCOL_MINOR, Response, ResponseKind,
};

async fn request(name: &str, parameters: serde_json::Value) -> Response {
    let wire = serde_json::json!({"request_id":identity(),"ipc_version":{"major":PROTOCOL_MAJOR,"minor":PROTOCOL_MINOR},"command":{"name":name,"parameters":parameters}});
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
fn unregister(path: &Path, binding: &str, vault: &str) {
    use vaultsyncd_platform_windows::cfapi::{
        adapter::{Adapter, MetadataOnly},
        catalog::Catalog,
    };
    let mut adapter = Adapter::connect(
        path,
        Catalog::new(
            vaultsyncd_domain::ids::BindingId::from_hex(binding).unwrap(),
            vaultsyncd_domain::ids::VaultId::from_hex(vault).unwrap(),
        ),
        Arc::new(MetadataOnly),
    )
    .unwrap();
    adapter.unregister().unwrap();
    vaultsyncd_platform_windows::shell::unregister(
        vaultsyncd_domain::ids::BindingId::from_hex(binding).unwrap(),
    )
    .unwrap();
}

// Runs the *product* assembly, never shell::register from the test. Covers both
// migration from a CFAPI-only vault and binding creation through the public IPC.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_explorer_registration_and_states() {
    use vaultsyncd_domain::ids::{BindingId, VaultId};
    use vaultsyncd_platform_windows::{
        cfapi::{
            adapter::{Adapter, MetadataOnly},
            catalog::Catalog,
        },
        files::VaultHandle,
        shell,
    };
    let fixture_path =
        PathBuf::from(std::env::var_os("VAULTSYNCD_PROCESS_FIXTURE").expect("real server fixture"));
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture_path).unwrap()).unwrap();
    let exe = PathBuf::from(std::env::var_os("VAULTSYNCD_AGENT_EXE").expect("product agent"));
    let driver = fixture_path.parent().unwrap();
    let base = driver.join(format!("run-explorer-{}", std::process::id()));
    let evidence = driver.join("explorer-evidence");
    std::fs::create_dir(&base).unwrap();
    std::fs::create_dir(&evidence).unwrap();
    for name in ["state", "vault", "cache", "new-vault", "new-cache"] {
        std::fs::create_dir(base.join(name)).unwrap();
    }
    let server = identity();
    let binding = identity();
    let second = identity();
    let id = BindingId::from_hex(&binding).unwrap();
    let vault = fixture["vault"].as_str().unwrap();
    let entry = fixture["entry"]["entry_id"].as_str().unwrap();
    let config_path = base.join("agent.json");
    let config = serde_json::json!({"server":{"id":server,"url":fixture["url"],"development_http":true},"device_id":identity(),"state_dir":base.join("state"),"poll_millis":250,"bindings":[{"id":binding,"vault_id":vault,"path":base.join("vault"),"cache":base.join("cache"),"mode":"on_demand"}]});
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let login = serde_json::json!({"login":"s052-owner","password":"S052-Correct-Horse-Battery-9!","device_label":"S129 Explorer"});
    success(invoke(
        &exe,
        &config_path,
        &["--login-stdin"],
        Some(&serde_json::to_vec(&login).unwrap()),
    ));
    // Reproduce an old client vault: CFAPI registered, no Shell registration.
    drop(
        Adapter::connect(
            &base.join("vault"),
            Catalog::new(id, VaultId::from_hex(vault).unwrap()),
            Arc::new(MetadataOnly),
        )
        .unwrap(),
    );
    assert!(!shell::registered(id).unwrap());
    let mut child = launch(&exe, &config_path, &base.join("agent.log"));
    until_status(&mut child, |s| {
        s.bindings
            .as_ref()
            .is_some_and(|b| b.len() == 1 && b[0].metadata_only != "0")
    })
    .await;
    assert!(
        shell::registered(id).unwrap(),
        "startup must repair CFAPI-only vaults"
    );
    let inspect = || {
        VaultHandle::open(&base.join("vault"))
            .unwrap()
            .inspect("file.txt")
            .unwrap()
    };
    let original = inspect();
    assert_eq!(original.cloud.as_ref().unwrap().on_disk, 0);
    let probe = |phase: &str, expected: u32| {
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(driver.join("explorer-probe.ps1"))
            .arg("-Vault")
            .arg(base.join("vault"))
            .arg("-Evidence")
            .arg(&evidence)
            .arg("-Phase")
            .arg(phase)
            .arg("-ExpectedState")
            .arg(expected.to_string())
            .output()
            .unwrap();
        println!("{}", String::from_utf8_lossy(&output.stdout));
        success(output);
    };
    probe("startup-online-only", 1); // SDK STORAGEPROVIDERSTATE_SPARSE
    assert_eq!(
        inspect().cloud.as_ref().unwrap().on_disk,
        0,
        "Explorer browsing must not hydrate"
    );
    ok("create_binding", serde_json::json!({"binding_id":second,"vault_id":vault,"local_path":base.join("new-vault"),"cache_path":base.join("new-cache"),"mode":"on_demand"})).await;
    until_status(&mut child, |s| {
        s.bindings
            .as_ref()
            .is_some_and(|b| b.len() == 2 && b.iter().all(|b| b.metadata_only != "0"))
    })
    .await;
    assert!(
        shell::registered(BindingId::from_hex(&second).unwrap()).unwrap(),
        "IPC create_binding must register with Explorer"
    );
    ok(
        "download_entry",
        serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity()}),
    )
    .await;
    until_file(
        &base.join("vault/file.txt"),
        Some(fixture["payload"].as_str().unwrap().as_bytes()),
    )
    .await;
    probe("downloaded-local", 2); // SDK STORAGEPROVIDERSTATE_IN_SYNC
    let downloaded = inspect();
    drop(child);
    child = launch(&exe, &config_path, &base.join("restart-local.log"));
    status(&mut child).await;
    let retained = inspect();
    assert_eq!(retained.identity, downloaded.identity);
    assert_eq!(
        retained.cloud.as_ref().unwrap().on_disk,
        downloaded.cloud.as_ref().unwrap().on_disk
    );
    assert_eq!(
        retained.cloud.as_ref().unwrap().pin_state,
        downloaded.cloud.as_ref().unwrap().pin_state
    );
    assert_eq!(
        std::fs::read(base.join("vault/file.txt")).unwrap(),
        fixture["payload"].as_str().unwrap().as_bytes()
    );
    probe("restarted-local", 2);
    ok(
        "release_entry",
        serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity()}),
    )
    .await;
    assert_eq!(inspect().cloud.as_ref().unwrap().on_disk, 0);
    probe("released-online-only", 1);
    for restart in 0..2 {
        drop(child);
        assert!(
            shell::registered(id).unwrap(),
            "exit must retain registration"
        );
        child = launch(
            &exe,
            &config_path,
            &base.join(format!("restart-{restart}.log")),
        );
        status(&mut child).await;
        assert!(shell::registered(id).unwrap());
        assert_eq!(
            inspect().identity,
            original.identity,
            "registration must not replace placeholders"
        );
        assert_eq!(inspect().cloud.as_ref().unwrap().on_disk, 0);
    }
    probe("restarted-online-only", 1);
    drop(child);
    success(invoke(&exe, &config_path, &["--once"], None));
    assert!(
        shell::registered(id).unwrap(),
        "normal exit must retain registration"
    );
    assert_eq!(inspect().cloud.as_ref().unwrap().on_disk, 0);
    // Only this run's synthetic files are explicitly removed; unregister is never used
    // to repair a vault or to remove partial placeholders.
    for (directory, binding) in [("vault", &binding), ("new-vault", &second)] {
        std::fs::remove_file(base.join(directory).join("file.txt")).unwrap();
        unregister(&base.join(directory), binding, vault);
        assert!(!shell::registered(BindingId::from_hex(binding).unwrap()).unwrap());
    }
    credentials::CredentialVault::new(&server)
        .unwrap()
        .erase()
        .unwrap();
    std::fs::remove_dir_all(&base).unwrap();
    println!(
        "check=pass s129_product_startup_repair_ipc_create_cloud_local_release_restart_identity_zero_hydration_and_persistent_registration"
    );
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
    for p in ["state", "vault", "cache"] {
        std::fs::create_dir(base.join(p)).unwrap();
    }
    let server = identity();
    let binding = identity();
    let vault = fixture["vault"].as_str().unwrap();
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
        ok("list_vaults", serde_json::json!({}))
            .await
            .items
            .unwrap()
            .iter()
            .any(|i| i.id == vault)
    );
    bad("create_binding",serde_json::json!({"binding_id":binding,"vault_id":vault,"local_path":base.join("state"),"cache_path":base.join("cache"),"mode":"on_demand"})).await;
    ok("create_binding",serde_json::json!({"binding_id":binding,"vault_id":vault,"local_path":base.join("vault"),"cache_path":base.join("cache"),"mode":"on_demand"})).await;
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
        std::fs::read(base.join("vault/file.txt")).unwrap(),
        fixture["payload"].as_str().unwrap().as_bytes()
    );
    ok(
        "release_entry",
        serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity()}),
    )
    .await;
    let native = vaultsyncd_platform_windows::files::VaultHandle::open(&base.join("vault"))
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
        std::fs::read(base.join("vault/file.txt")).unwrap(),
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
    let old = listing(&client, vault)
        .await
        .into_iter()
        .find(|e| e.entry_id == entry)
        .unwrap();
    let mapping = WritableMapping::open(&base.join("vault/file.txt"));
    remote_put(
        &client,
        vault,
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
    // The user fixes the actual cause (the open mapping) before asking for a retry.
    drop(mapping);
    ok(
        "retry_problem",
        serde_json::json!({"binding_id":binding,"operation_id":problem.problem_id}),
    )
    .await;
    until_file(
        &base.join("vault/file.txt"),
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
    let current = listing(&client, vault)
        .await
        .into_iter()
        .find(|e| e.entry_id == entry)
        .unwrap();
    let original = fixture["entry"]["version_id"].as_str().unwrap();
    ok("restore_entry",serde_json::json!({"binding_id":binding,"entry_id":entry,"operation_id":identity(),"base_revision":current.revision,"version_id":original})).await;
    until_file(
        &base.join("vault/file.txt"),
        Some(fixture["payload"].as_str().unwrap().as_bytes()),
    )
    .await;
    let current = listing(&client, vault)
        .await
        .into_iter()
        .find(|e| e.entry_id == entry)
        .unwrap();
    let (s, _) = sync::snapshot(&client, vault).await.unwrap();
    sync::submit(
        &client,
        vault,
        &namespace_request(&current, OperationKind::Delete, s.server_epoch, None),
    )
    .await
    .unwrap();
    until_file(&base.join("vault/file.txt"), None).await;
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
        &base.join("vault/file.txt"),
        Some(fixture["payload"].as_str().unwrap().as_bytes()),
    )
    .await;
    ok("pause_binding", serde_json::json!({"binding_id":binding})).await;
    std::fs::write(
        base.join("vault/unique-unconfirmed.bin"),
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
        std::fs::read(base.join("vault/unique-unconfirmed.bin")).unwrap(),
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
        "list_vaults",
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
        std::fs::read(base.join("vault/file.txt")).unwrap(),
        fixture["payload"].as_str().unwrap().as_bytes()
    );
    for name in ["file.txt", "unique-unconfirmed.bin"] {
        std::fs::remove_file(base.join("vault").join(name)).unwrap();
    }
    unregister(&base.join("vault"), &binding, vault);
    std::fs::remove_dir_all(&base).unwrap();
    println!(
        "check=pass s072_all_21_handlers_real_agent_named_pipe_effects_errors_versions_recovery_and_credential_redaction"
    );
}
