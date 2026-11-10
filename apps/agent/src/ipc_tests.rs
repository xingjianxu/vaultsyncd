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
    let (_connection, connection) = watch::channel(ConnectionInfo {
        server_id: "02".repeat(16),
        server_url: "https://example.invalid".into(),
        ca_file: None,
        authenticated: false,
        username: None,
    });
    let (_problems, problems) = watch::channel(Vec::new());
    let (updates, _) = watch::channel(0);
    let (_offline, offline) = watch::channel(std::collections::HashSet::new());
    let hub = Hub {
        state: Mutex::new((0, None)),
        updates,
        instance: "test".into(),
        connection,
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
    let (commands, _requests) = mpsc::channel(8);
    let (_connection, connection) = watch::channel(ConnectionInfo {
        server_id: "11".repeat(16),
        server_url: "https://sync.example".into(),
        ca_file: None,
        authenticated: false,
        username: None,
    });
    let (_problems, problems) = watch::channel(Vec::new());
    let (_offline, offline) = watch::channel(std::collections::HashSet::new());
    let server = start(db.clone(), commands, connection, problems, offline).unwrap();
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
    sqlx::query("DROP TABLE bindings")
        .execute(db.pool())
        .await
        .unwrap();
    let response = endpoint.request(&request).await.unwrap();
    assert!(response.result.is_none());
    assert_eq!(response.error.unwrap().code, LocalIpcErrorCode::Unavailable);
    request.ipc_version.major += 1;
    assert_eq!(
        endpoint
            .request(&request)
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    request.ipc_version.major = 1;
    request.ipc_version.minor = 1;
    request.command.name = CommandName::ListBindings;
    assert_eq!(
        endpoint
            .request(&request)
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    request.ipc_version.minor = 2;
    request.command.name = CommandName::ListFiles;
    request.command.parameters = Some(CommandParameters {
        binding_id: Some("11".repeat(16)),
        ..Default::default()
    });
    assert_eq!(
        endpoint
            .request(&request)
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    request.ipc_version.minor = 3;
    request.command.name = CommandName::ClaimNotification;
    request.command.parameters = Some(CommandParameters {
        notification_key: Some("vault:11111111111111111111111111111111:MOUNT_FAILED".into()),
        notification_generation: Some("1".into()),
        ..Default::default()
    });
    assert_eq!(
        endpoint
            .request(&request)
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    request.ipc_version.minor = 5;
    for name in [CommandName::GetRules, CommandName::SetRules] {
        request.command.name = name;
        request.command.parameters = None;
        assert_eq!(
            endpoint
                .request(&request)
                .await
                .unwrap()
                .error
                .unwrap()
                .code,
            LocalIpcErrorCode::UnsupportedVersion
        );
    }
    request.ipc_version = version::supported();
    request.ipc_version.minor = 7;
    request.command.name = CommandName::ListActivity;
    request.command.parameters = None;
    assert_eq!(
        endpoint
            .request(&request)
            .await
            .unwrap()
            .error
            .unwrap()
            .code,
        LocalIpcErrorCode::UnsupportedVersion
    );
    request.ipc_version.minor = 8;
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
    server.shutdown().await.unwrap();
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
