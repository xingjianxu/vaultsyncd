//! Real native transport and SQLite errors must never become successful empty snapshots.
use super::*;

#[tokio::test]
async fn new_binding_without_import_proof_is_unknown() {
    use vaultsyncd_client_store::bindings::{NewBinding, SyncMode, insert_binding};
    use vaultsyncd_domain::{
        ids::{DeviceId, ServerId, VaultId},
        version::ServerEpoch,
    };
    let path = std::env::temp_dir().join(format!(
        "s081-no-import-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
    let db = Database::open(path.join("client.sqlite")).await.unwrap();
    let binding = BindingId::from_bytes([1; 16]);
    let mut tx = db.begin_write().await.unwrap();
    insert_binding(
        &mut tx,
        &NewBinding {
            id: binding,
            server: ServerId::from_bytes([2; 16]),
            device: DeviceId::from_bytes([3; 16]),
            vault: VaultId::from_bytes([4; 16]),
            local_path: "C:/test/local".into(),
            cache_path: "C:/test/cache".into(),
            mode: SyncMode::Full,
            rules_version: "1".into(),
            server_epoch: ServerEpoch::new(1).unwrap(),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let (_connection, connections) = watch::channel(vec![ConnectionInfo {
        server_id: "02".repeat(16),
        server_url: "https://example.invalid".into(),
        ca_file: None,
        authenticated: false,
        username: None,
    }]);
    let (_problems, problems) = watch::channel(Vec::new());
    let (updates, _) = watch::channel(0);
    let (_offline, offline) = watch::channel(std::collections::HashSet::new());
    let hub = Hub {
        state: Mutex::new((0, None)),
        updates,
        instance: "test".into(),
        connections,
        problems,
        offline,
    };
    let result = snapshot(&db, &hub).await.unwrap();
    let row = &result.bindings.unwrap()[0];
    assert_eq!(row.unknown, "1");
    assert!(!row.synchronized);
    db.close().await;
    // SQLx's SQLite worker may release its Windows file handle just after pool close.
    for attempt in 0..100 {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => return,
            Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(error) => panic!("cannot clean this test's closed database: {error}"),
        }
    }
}

#[test]
fn expired_credentials_are_an_authentication_failure_not_a_blocked_operation() {
    let request = Request {
        request_id: "auth-expired".into(),
        ipc_version: version::supported(),
        command: Command {
            name: CommandName::Login,
            parameters: None,
        },
    };
    let response = management_response(
        &request,
        Err(PortError::rejected("AUTH_EXPIRED", "credentials expired")),
    );
    assert_eq!(
        response.error.unwrap().code,
        LocalIpcErrorCode::Unauthenticated
    );
}
#[tokio::test]
async fn snapshot_failure_is_explicit_and_version_checks_precede_store_reads() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("s072-ipc-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    let db = Arc::new(Database::open(path.join("client.sqlite")).await.unwrap());
    let (commands, mut requests) = mpsc::channel::<CommandRequest>(8);
    let (forwarded, mut dispatched) = mpsc::unbounded_channel();
    // Exercise the real authenticated transport and IPC adapter independently of
    // command side effects. This explicit executor probe proves exact forwarding;
    // product rule persistence is covered by the native exclusions acceptance.
    let executor = tokio::spawn(async move {
        while let Some(command) = requests.recv().await {
            forwarded.send(command.request.clone()).unwrap();
            let _ = command.reply.send(Response::error(
                command.request.request_id,
                ErrorBody {
                    code: LocalIpcErrorCode::Blocked,
                    message: "S196 executor dispatch probe".into(),
                    retryable: false,
                },
            ));
        }
    });
    let (_connection, connections) = watch::channel(vec![ConnectionInfo {
        server_id: "11".repeat(16),
        server_url: "https://sync.example".into(),
        ca_file: None,
        authenticated: false,
        username: None,
    }]);
    let (_problems, problems) = watch::channel(Vec::new());
    let (_offline, offline) = watch::channel(std::collections::HashSet::new());
    let server = start(db.clone(), commands, connections, problems, offline).unwrap();
    let endpoint = Endpoint::current("vaultsyncd-agent").unwrap();
    let mut request = Request {
        request_id: "actual-snapshot".into(),
        ipc_version: version::supported(),
        command: Command {
            name: CommandName::GetStatus,
            parameters: None,
        },
    };
    let value = endpoint.request(&request).await.unwrap().result.unwrap();
    assert_eq!(value.bindings, Some(vec![]));
    assert!(value.snapshot_version.is_some());
    assert_eq!(value.ipc_protocol, version::supported());
    let mut current: Vec<Request> = serde_json::from_str(include_str!(
        "../../../api/examples/local-ipc.management.json"
    ))
    .unwrap();
    current.push(Request {
        request_id: "prepare-maintenance".into(),
        ipc_version: version::supported(),
        command: Command {
            name: CommandName::PrepareMaintenance,
            parameters: Some(CommandParameters {
                destination: Some("C:/S196/recovery".into()),
                ..Default::default()
            }),
        },
    });
    // Also cover explicit server-scoped operations added in the old 2.10 minor.
    for name in [
        CommandName::GetConnection,
        CommandName::Login,
        CommandName::Logout,
        CommandName::ListVaults,
        CommandName::CreateBinding,
    ] {
        let mut scoped = current
            .iter()
            .find(|r| r.command.name == name)
            .unwrap()
            .clone();
        scoped.request_id.push_str("-server-scoped");
        scoped.command.parameters.get_or_insert_default().server_id = Some("66".repeat(16));
        current.push(scoped);
    }
    let mut extra = current
        .iter()
        .find(|r| r.command.name == CommandName::SetRules)
        .unwrap()
        .clone();
    extra.request_id = "set-extra-text".into();
    extra.command.parameters.as_mut().unwrap().rules =
        Some("# 额外规则\n草稿/**\n!~$保留.docx\n".into());
    current.push(extra);
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../../api/local-ipc.schema.json")).unwrap();
    let expected: std::collections::HashSet<_> = schema["$defs"]["CommandName"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    let actual: std::collections::HashSet<_> = current
        .iter()
        .map(|r| {
            serde_json::to_value(r.command.name)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        actual, expected,
        "every current command must traverse the current adapter"
    );
    for current_request in &current {
        assert_eq!(current_request.ipc_version, version::supported());
        vaultsyncd_local_ipc::commands::validate(&current_request.command).unwrap();
        let response = endpoint.request(current_request).await.unwrap();
        response.validate().unwrap();
        assert_eq!(response.request_id, current_request.request_id);
        if matches!(
            current_request.command.name,
            CommandName::GetStatus | CommandName::SubscribeStatus
        ) {
            assert_eq!(response.kind, ResponseKind::Ok);
            assert_eq!(response.result.unwrap().ipc_protocol, version::supported());
        } else {
            let error = response.error.unwrap();
            assert_eq!(error.code, LocalIpcErrorCode::Blocked);
            assert_eq!(error.message, "S196 executor dispatch probe");
            let forwarded = dispatched.try_recv().unwrap();
            assert_eq!(
                serde_json::to_value(forwarded).unwrap(),
                serde_json::to_value(current_request).unwrap()
            );
        }
    }
    sqlx::query("DROP TABLE bindings")
        .execute(db.pool())
        .await
        .unwrap();
    let response = endpoint.request(&request).await.unwrap();
    assert!(response.result.is_none());
    assert_eq!(response.error.unwrap().code, LocalIpcErrorCode::Unavailable);
    // All 2.x commands, including empty/text rule writes, must be refused before
    // store reads or executor dispatch, not interpreted under the new defaults.
    for current_request in &current {
        for minor in [0, 6, 11] {
            let mut old = current_request.clone();
            old.ipc_version = IpcVersion { major: 2, minor };
            let response = endpoint.request(&old).await.unwrap();
            assert_eq!(response.request_id, old.request_id);
            assert!(response.result.is_none());
            let error = response.error.unwrap();
            assert_eq!(error.code, LocalIpcErrorCode::UnsupportedVersion);
            assert!(!error.retryable);
        }
    }
    for incompatible in [
        IpcVersion {
            major: 1,
            minor: 11,
        },
        IpcVersion {
            major: version::SUPPORTED_MAJOR + 1,
            minor: 0,
        },
        IpcVersion {
            major: version::SUPPORTED_MAJOR,
            minor: version::SUPPORTED_MINOR + 1,
        },
    ] {
        request.ipc_version = incompatible;
        request.command = Command {
            name: CommandName::SetRules,
            parameters: None,
        };
        let error = endpoint.request(&request).await.unwrap().error.unwrap();
        assert_eq!(error.code, LocalIpcErrorCode::UnsupportedVersion);
        assert!(!error.retryable);
    }
    request.ipc_version = version::supported();
    request.command = Command {
        name: CommandName::SetRules,
        parameters: None,
    };
    assert_eq!(
        endpoint
            .request(&request)
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        LocalIpcErrorCode::InvalidArgument
    );
    assert!(
        dispatched.try_recv().is_err(),
        "incompatible or invalid requests reached the executor"
    );
    server.shutdown().await.unwrap();
    executor.await.unwrap();
    db.pool().close().await;
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn storage_exhaustion_has_actionable_redacted_gui_cli_error() {
    let request = Request {
        request_id: "s105-capacity".into(),
        ipc_version: version::supported(),
        command: Command {
            name: CommandName::ReleaseEntry,
            parameters: None,
        },
    };
    let response = management_response(
        &request,
        Err(PortError::local(
            vaultsyncd_transfer::ports::LocalErrorKind::NoSpace,
            "private path and content must not escape",
        )),
    );
    let error = response.error.unwrap();
    assert_eq!(error.code, LocalIpcErrorCode::Blocked);
    assert!(error.message.contains("空间不足"));
    assert!(error.message.contains("INSUFFICIENT_STORAGE"));
    assert!(!error.message.contains("private path"));
}

#[tokio::test]
async fn control_changes_during_status_read_do_not_restore_detached_management_rows() {
    use vaultsyncd_agent_core::bindings::control::{BindingControls, ExecutionGate};
    use vaultsyncd_client_store::bindings::{NewBinding, SyncMode, insert_binding};
    use vaultsyncd_domain::{
        ids::{DeviceId, ServerId, VaultId},
        version::ServerEpoch,
    };
    let path = std::env::temp_dir().join(format!(
        "s231-control-snapshot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir(&path).unwrap();
    let database = path.join("client.sqlite");
    let db = Arc::new(Database::open(&database).await.unwrap());
    let binding = BindingId::from_bytes([31; 16]);
    let mut tx = db.begin_write().await.unwrap();
    insert_binding(
        &mut tx,
        &NewBinding {
            id: binding,
            server: ServerId::from_bytes([32; 16]),
            device: DeviceId::from_bytes([33; 16]),
            vault: VaultId::from_bytes([34; 16]),
            local_path: "C:/s231/local".into(),
            cache_path: "C:/s231/cache".into(),
            mode: SyncMode::OnDemand,
            rules_version: "1".into(),
            server_epoch: ServerEpoch::new(1).unwrap(),
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let controls = BindingControls::new(db.clone());
    // Model an authenticated mutation committing after GetStatus has read the management
    // list, but before its per-binding transactional state read. Both control projections
    // must agree, including the Resume direction rather than only monotone detachment.
    for paused in [true, false] {
        let mut info = crate::management::binding_list(&db).await.unwrap();
        assert_ne!(info[0].paused, paused);
        controls.set_paused(binding, paused).await.unwrap();
        let states = binding_statuses(&db, &mut info, &Default::default())
            .await
            .unwrap();
        assert_eq!(states[0].paused, paused);
        assert_eq!(info[0].paused, states[0].paused);
        assert_eq!(info[0].detached, states[0].detached);
    }
    let mut info = crate::management::binding_list(&db).await.unwrap();
    assert!(!info[0].detached);
    controls.unbind_keep_data(binding).await.unwrap();
    let states = binding_statuses(&db, &mut info, &Default::default())
        .await
        .unwrap();
    assert!(states[0].detached && states[0].paused);
    assert!(info[0].detached && info[0].paused);
    assert!(!states[0].synchronized);
    drop(controls);
    db.pool().close().await;
    drop(db);
    // Reopen the actual SQLite file and adopt startup rules, rather than relying on an
    // in-memory flag. Detached recovery state survives; Resume cannot revive the row.
    let db = Arc::new(Database::open(&database).await.unwrap());
    let mut legacy: crate::config::AgentConfig = serde_json::from_value(serde_json::json!({
        "server": { "id": "20".repeat(16), "url": "https://example.invalid" },
        "device_id": "21".repeat(16),
        "state_dir": "C:/s231/state",
        "poll_millis": 2000,
        "bindings": [{
            "id": binding.to_string(), "vault_id": "22".repeat(16),
            "path": "C:/s231/local", "cache": "C:/s231/cache", "mode": "on_demand",
        }],
    }))
    .unwrap();
    legacy.validate().unwrap();
    let assignments = crate::management::load_bindings(&db, &mut legacy)
        .await
        .unwrap();
    assert_eq!(legacy.bindings.len(), 1);
    assert_eq!(
        assignments.get(&binding),
        Some(&ServerId::from_bytes([32; 16]))
    );
    let controls = BindingControls::new(db.clone());
    controls.adopt_default_policy(binding).await.unwrap();
    assert_eq!(
        controls.execution_gate(binding, None).await.unwrap(),
        ExecutionGate::Unbound,
    );
    assert_eq!(
        controls
            .set_paused(binding, false)
            .await
            .unwrap_err()
            .code(),
        Some("BINDING_UNBOUND"),
    );
    let info = crate::management::binding_list(&db).await.unwrap();
    assert_eq!(info.len(), 1, "the recovery record remains durable");
    assert!(info[0].detached && info[0].paused);
    drop(controls);
    db.pool().close().await;
    drop(db);
    for attempt in 0..100 {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => return,
            Err(_) if attempt < 99 => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(error) => panic!("cannot clean this test's closed database: {error}"),
        }
    }
}
