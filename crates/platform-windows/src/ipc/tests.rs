use super::*;
use vaultsyncd_local_ipc::{
    generated::{Command, CommandName},
    service::Service,
    version,
};
fn endpoint() -> Endpoint {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    Endpoint::current(&format!(
        "s071-native-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
    .unwrap()
}
fn request(id: &str) -> Request {
    Request {
        request_id: id.into(),
        ipc_version: version::supported(),
        command: Command {
            name: CommandName::GetStatus,
            parameters: None,
        },
    }
}
fn serve(endpoint: Endpoint) -> Server {
    Server::bind(endpoint, |r| async move {
        Ok(Service::new("native-s071").handle(&r))
    })
    .unwrap()
}
async fn closed(pipe: &mut NamedPipeClient) {
    let mut byte = [0];
    let read = tokio::time::timeout(IO_TIMEOUT + Duration::from_secs(2), pipe.read(&mut byte))
        .await
        .expect("connection must close within its deadline");
    assert!(
        matches!(read, Ok(0)) || read.is_err(),
        "malformed request must not receive success"
    );
}
#[tokio::test]
async fn same_user_partial_frames_pipeline_and_disconnect_are_isolated() {
    let ep = endpoint();
    let server = serve(ep.clone());
    assert!(
        Server::bind(ep.clone(), |r| async move {
            Ok(Service::new("squatter").handle(&r))
        })
        .is_err()
    );
    let mut pipe = ep.connect().await.unwrap();
    for id in ["split", "pipeline-1", "pipeline-2"] {
        let frame = framing::encode(&serde_json::to_vec(&request(id)).unwrap()).unwrap();
        let mut remaining = frame.as_slice();
        if id == "split" {
            // Force the native server to see both a partial header and a partial body,
            // rather than letting the OS coalesce all tiny writes before it is scheduled.
            for end in [2, 6] {
                let start = frame.len() - remaining.len();
                pipe.write_all(&frame[start..end]).await.unwrap();
                remaining = &frame[end..];
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert_eq!(
                    pipe.try_read(&mut [0]).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
            }
        }
        for byte in remaining {
            pipe.write_all(&[*byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    }
    for id in ["split", "pipeline-1", "pipeline-2"] {
        let bytes = deadline(read_frame(&mut pipe)).await.unwrap();
        let response: Response = serde_json::from_slice(&bytes).unwrap();
        response.validate().unwrap();
        assert_eq!(response.request_id, id);
        assert_eq!(response.result.unwrap().agent_version, "native-s071");
    }
    drop(pipe);
    let mut tasks = JoinSet::new();
    for i in 0..MAX_CONNECTIONS {
        let ep = ep.clone();
        tasks.spawn(async move {
            ep.request(&request(&format!("concurrent-{i}")))
                .await
                .unwrap()
        });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap().result.is_some());
    }
    assert!(
        ep.request(&request("after-disconnect"))
            .await
            .unwrap()
            .result
            .is_some()
    );
    server.shutdown().await.unwrap();
    let restart = serve(ep);
    restart.shutdown().await.unwrap();
    println!(
        "check=pass native_same_user_partial_pipeline_concurrent_disconnect_exclusive_restart"
    );
}
#[tokio::test]
async fn verified_request_checks_actual_peer_before_running_handler() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let ep = endpoint();
    let handled = Arc::new(AtomicU32::new(0));
    let count = handled.clone();
    let server = Server::bind(ep.clone(), move |r| {
        count.fetch_add(1, Ordering::SeqCst);
        async move { Ok(Service::new("native-s109").handle(&r)) }
    })
    .unwrap();
    let executable = std::env::current_exe().unwrap();
    // Use a different real executable, so rejection proves the peer image check ran rather
    // than merely failing to canonicalize a nonexistent expected path.
    let other = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32")
        .join("cmd.exe");
    assert!(other.is_file());
    assert_ne!(
        std::fs::canonicalize(&executable).unwrap(),
        std::fs::canonicalize(&other).unwrap()
    );
    let error = ep
        .request_verified(&other, &request("wrong-executable"))
        .await
        .err()
        .expect("a different existing executable must not authorize the request");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    let mut wrong_user = ep.clone();
    wrong_user.sid = "S-1-5-7".into();
    let error = wrong_user
        .request_verified(&executable, &request("wrong-user"))
        .await
        .err()
        .expect("a different SID must not authorize the request");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    let (response, process) = ep
        .request_verified(&executable, &request("verified"))
        .await
        .unwrap();
    assert_eq!(response.request_id, "verified");
    assert_eq!(response.result.unwrap().agent_version, "native-s109");
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    assert!(
        !process.wait(0).unwrap(),
        "the pinned test process is alive"
    );
    server.shutdown().await.unwrap();
    println!(
        "check=pass native_verified_request_same_executable_allowed_other_image_and_sid_denied_before_handler"
    );
}
#[tokio::test]
async fn authenticated_session_request_pins_peer_without_an_expected_program_path() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let ep = endpoint();
    let handled = Arc::new(AtomicU32::new(0));
    let count = handled.clone();
    let server = Server::bind(ep.clone(), move |r| {
        count.fetch_add(1, Ordering::SeqCst);
        async move { Ok(Service::new("native-s185").handle(&r)) }
    })
    .unwrap();
    let mut wrong_user = ep.clone();
    wrong_user.sid = "S-1-5-7".into();
    let error = wrong_user
        .request_authenticated(&request("wrong-session-user"))
        .await
        .err()
        .expect("another user cannot send an authenticated session request");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    // This peer is the native test executable, not a sibling vaultsyncd-agent.exe. No program
    // directory/hash/provenance is needed for same-user session control.
    let (response, process) = ep
        .request_authenticated(&request("authenticated-session"))
        .await
        .unwrap();
    assert_eq!(response.request_id, "authenticated-session");
    assert_eq!(response.result.unwrap().agent_version, "native-s185");
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    assert!(!process.wait(0).unwrap());
    server.shutdown().await.unwrap();
    let replacement = serve(ep.clone());
    // Closing/rebinding the endpoint does not invalidate or replace the original peer handle.
    assert!(!process.wait(0).unwrap());
    assert!(
        ep.request(&request("replacement"))
            .await
            .unwrap()
            .result
            .is_some()
    );
    replacement.shutdown().await.unwrap();
}
#[tokio::test]
async fn malformed_oversize_unknown_and_truncated_frames_close_only_client() {
    let ep = endpoint();
    let server = serve(ep.clone());
    for bytes in [
        vec![0, 0, 0, 0],
        ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes().to_vec(),
        framing::encode(b"{bad-json").unwrap(),
        framing::encode(serde_json::json!({"request_id":"bad","ipc_version":{"major":version::SUPPORTED_MAJOR,"minor":version::SUPPORTED_MINOR},"command":{"name":"erase_everything"}}).to_string().as_bytes()).unwrap(),
    ] {
        let mut pipe = ep.connect().await.unwrap();
        pipe.write_all(&bytes).await.unwrap();
        closed(&mut pipe).await;
    }
    for bytes in [vec![0, 0], vec![0, 0, 0, 8, b'{']] {
        let mut pipe = ep.connect().await.unwrap();
        pipe.write_all(&bytes).await.unwrap();
        drop(pipe);
    }
    let response = ep.request(&request("still-alive")).await.unwrap();
    assert!(response.result.is_some());
    let mut incompatible = request("version-error");
    incompatible.ipc_version.major += 1;
    assert!(ep.request(&incompatible).await.unwrap().error.is_some());
    server.shutdown().await.unwrap();
    println!("check=pass native_bad_frames_unknown_command_version_error_disconnect_survived");
}
#[tokio::test]
async fn idle_connections_deadline_capacity_and_shutdown_are_bounded() {
    let ep = endpoint();
    let server = serve(ep.clone());
    let mut idle = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        idle.push(ep.connect().await.unwrap());
    }
    // Allow all accepted handles to enter the bounded connection set.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut excess = ep.connect().await.unwrap();
    closed(&mut excess).await;
    // A slow partial frame has the same whole-frame deadline as an idle connection.
    idle[0].write_all(&[0]).await.unwrap();
    closed(&mut idle[0]).await;
    drop(idle);
    assert!(
        ep.request(&request("after-timeout"))
            .await
            .unwrap()
            .result
            .is_some()
    );
    let mut partial = ep.connect().await.unwrap();
    partial.write_all(&[0, 0]).await.unwrap();
    let start = std::time::Instant::now();
    server.shutdown().await.unwrap();
    closed(&mut partial).await;
    assert!(start.elapsed() < Duration::from_secs(2));
    println!("check=pass native_capacity_whole_frame_deadline_shutdown_cancel");
}
#[tokio::test]
async fn handler_and_unread_response_have_deadlines() {
    let ep = endpoint();
    let server = Server::bind(ep.clone(), |r| async move {
        if r.request_id == "hang" {
            std::future::pending::<()>().await;
        }
        let mut response = Service::new("native-s071").handle(&r);
        if r.request_id == "unread" {
            response.result.as_mut().unwrap().agent_version = "x".repeat(900_000);
        }
        Ok(response)
    })
    .unwrap();
    let mut pipe = ep.connect().await.unwrap();
    write_json(&mut pipe, &request("hang")).await.unwrap();
    closed(&mut pipe).await;
    let mut unread = ep.connect().await.unwrap();
    // Windows may buffer one complete large write despite the configured pipe quota.
    // Pipeline enough requests to force write backpressure rather than assuming the quota.
    for _ in 0..32 {
        write_json(&mut unread, &request("unread")).await.unwrap();
    }
    tokio::time::sleep(IO_TIMEOUT + Duration::from_secs(1)).await;
    // Drain the bounded buffered prefix after the blocked response has been cancelled.
    let mut bytes = Vec::new();
    // Either EOF or broken-pipe is a native disconnect, but a still-pending read fails.
    let _ = tokio::time::timeout(Duration::from_secs(2), unread.read_to_end(&mut bytes))
        .await
        .expect("unread response must be disconnected");
    assert!(
        bytes.len() < 32 * 900_000,
        "unread response pipeline must be cut off"
    );
    assert!(
        ep.request(&request("after-hangs"))
            .await
            .unwrap()
            .result
            .is_some()
    );
    server.shutdown().await.unwrap();
    println!("check=pass native_handler_and_unread_write_deadlines");
}
#[tokio::test]
async fn native_acl_other_local_user_and_token_identity_checks() {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn LogonUserW(
            user: *const u16,
            domain: *const u16,
            password: *const u16,
            kind: u32,
            provider: u32,
            token: *mut *mut c_void,
        ) -> i32;
        fn ImpersonateLoggedOnUser(token: *mut c_void) -> i32;
        fn RevertToSelf() -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    let user = std::env::var("VAULTSYNCD_IPC_TEST_USER")
        .expect("native ACL test requires an isolated second local user");
    let password = std::env::var("VAULTSYNCD_IPC_TEST_PASSWORD")
        .expect("native ACL test requires synthetic user credentials");
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut token = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            LogonUserW(
                wide(&user).as_ptr(),
                wide(".").as_ptr(),
                wide(&password).as_ptr(),
                2,
                0,
                &mut token,
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    let ep = endpoint();
    let server = serve(ep.clone());
    assert_ne!(unsafe { ImpersonateLoggedOnUser(token) }, 0);
    // No await, assertion or panic until identity has been restored.
    let denied = ClientOptions::new().open(ep.name());
    let reverted = unsafe { RevertToSelf() };
    if reverted == 0 {
        std::process::abort();
    }
    unsafe {
        CloseHandle(token);
    }
    assert_eq!(denied.unwrap_err().raw_os_error(), Some(5));
    // The server-side token check must also reject mismatches even if the ACL was bypassed.
    let probe = endpoint();
    let listener = probe.listener(true).unwrap();
    let mut client = probe.connect().await.unwrap();
    listener.connect().await.unwrap();
    client.write_all(&[42]).await.unwrap();
    let mut byte = [0];
    listener.readable().await.unwrap();
    assert_eq!(listener.try_read(&mut byte).unwrap(), 1);
    assert_eq!(
        security::check_client(&listener, "S-1-5-7")
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    security::check_client(&listener, &probe.sid).unwrap();
    // Native consumers reject a server whose user differs from the endpoint identity.
    let mut wrong = ep.clone();
    wrong.sid = "S-1-5-7".into();
    assert_eq!(
        wrong.connect().await.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(
        ep.request(&request("owner-after-denial"))
            .await
            .unwrap()
            .result
            .is_some()
    );
    server.shutdown().await.unwrap();
    println!(
        "check=pass native_other_local_user_acl_denied_client_token_checked_server_token_checked"
    );
}
