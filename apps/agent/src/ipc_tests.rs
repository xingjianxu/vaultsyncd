//! Real native transport and SQLite errors must never become successful empty snapshots.
use super::*;
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
    });
    let (_problems, problems) = watch::channel(Vec::new());
    let server = start(db.clone(), commands, connection, problems).unwrap();
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
    server.shutdown().await.unwrap();
    db.pool().close().await;
    std::fs::remove_dir_all(path).unwrap();
}
