//! S073 acceptance: the real `vaultsyncd-cli.exe` against the real Windows agent.
//!
//! Every assertion below drives the shipped CLI process. The test never opens client.sqlite and
//! uses process handle inspection to prove the CLI holds no such handle while a management
//! request is in flight.
#![cfg(windows)]
#![allow(unsafe_code)]

use rand_core::{OsRng, RngCore};
use std::{
    ffi::c_void,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[allow(dead_code)]
#[path = "../../agent/src/credentials.rs"]
mod credentials;

const USER: &str = "s052-owner";
const PASSWORD: &str = "S052-Correct-Horse-Battery-9!";

fn identity() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!(
                "CLI did not emit JSON ({error}): stdout={:?} stderr={:?}",
                self.stdout, self.stderr
            )
        })
    }
}

fn run_cli(cli: &Path, args: &[&str], input: Option<&[u8]>) -> Run {
    let mut child = Command::new(cli)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn ok(run: Run) -> serde_json::Value {
    let value = run.json();
    assert_eq!(
        run.code, 0,
        "CLI must succeed: stdout={:?} stderr={:?}",
        run.stdout, run.stderr
    );
    assert_eq!(value["ok"], true, "{value}");
    assert!(
        run.stderr.is_empty(),
        "JSON mode wrote stderr: {:?}",
        run.stderr
    );
    value
}

fn failed(run: Run, exit: u8, code: &str) -> serde_json::Value {
    let value = run.json();
    assert_eq!(
        run.code,
        i32::from(exit),
        "stable exit code: stdout={:?} stderr={:?}",
        run.stdout,
        run.stderr
    );
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["code"], code, "{value}");
    assert_eq!(value["exit_code"], exit, "{value}");
    assert!(!value.to_string().contains(PASSWORD));
    value
}

fn binding_status(value: &serde_json::Value) -> Vec<serde_json::Value> {
    value["result"]["bindings"]
        .as_array()
        .expect("real agent binding status")
        .clone()
}

async fn cli_status(cli: &Path) -> serde_json::Value {
    let cli = cli.to_path_buf();
    tokio::task::spawn_blocking(move || run_cli(&cli, &["--json", "status"], None))
        .await
        .unwrap()
        .json()
}

async fn await_status(cli: &Path, predicate: impl Fn(&serde_json::Value) -> bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let value = cli_status(cli).await;
        if predicate(&value) {
            return;
        }
        assert!(Instant::now() < end, "{what}: {value}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_agent(cli: &Path) {
    let end = Instant::now() + Duration::from_secs(25);
    loop {
        let value = cli_status(cli).await;
        if value["ok"] == true {
            return;
        }
        assert!(
            Instant::now() < end,
            "agent IPC never became ready: {value}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn call(cli: &Path, args: Vec<String>, input: Option<Vec<u8>>) -> Run {
    let cli = cli.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli(&cli, &borrowed, input.as_deref())
    })
    .await
    .unwrap()
}

fn cli_args(parts: &[&str]) -> Vec<String> {
    let mut args = vec!["--json".to_string()];
    args.extend(parts.iter().map(|s| (*s).to_string()));
    args
}

struct ChildGuard(Child);
impl ChildGuard {
    fn spawn(exe: &Path, config: &Path, log: &Path) -> Self {
        Self(
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
    fn kill(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

struct Cleanup {
    agent: Option<ChildGuard>,
    server: String,
    base: PathBuf,
    binding: Option<String>,
    root: Option<String>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(mut agent) = self.agent.take() {
            agent.kill();
        }
        if let Ok(vault) = credentials::CredentialVault::new(&self.server) {
            let _ = vault.erase();
        }
        if let (Some(binding), Some(root)) = (self.binding.as_ref(), self.root.as_ref()) {
            unregister(&self.base.join("root"), binding, root);
        }
        if self.base.exists() {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }
}

fn unregister(path: &Path, binding: &str, root: &str) {
    use vaultsyncd_platform_windows::cfapi::{
        adapter::{Adapter, MetadataOnly},
        catalog::Catalog,
    };
    let Ok(mut adapter) = Adapter::connect(
        path,
        Catalog::new(
            vaultsyncd_domain::ids::BindingId::from_hex(binding).unwrap(),
            vaultsyncd_domain::ids::RootId::from_hex(root).unwrap(),
        ),
        std::sync::Arc::new(MetadataOnly),
    ) else {
        return;
    };
    let _ = adapter.unregister();
}

// --- process handle evidence -----------------------------------------------------------------

#[repr(C)]
struct HandleEntry {
    object: usize,
    pid: usize,
    handle: usize,
    granted_access: u32,
    creator_backtrace_index: u16,
    object_type_index: u16,
    handle_attributes: u32,
    reserved: u32,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        information: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
    fn GetCurrentProcess() -> *mut c_void;
    fn DuplicateHandle(
        process: *mut c_void,
        source: *mut c_void,
        target_process: *mut c_void,
        target: *mut *mut c_void,
        access: u32,
        inherit: i32,
        options: u32,
    ) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetFinalPathNameByHandleW(
        handle: *mut c_void,
        path: *mut u16,
        length: u32,
        flags: u32,
    ) -> u32;
}

struct HandleSample {
    handles: usize,
    paths: Vec<String>,
}

fn sample_process(pid: u32) -> Option<HandleSample> {
    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
    let mut length = 1usize << 20;
    let entries = loop {
        let mut buffer = vec![0u8; length];
        let mut returned = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                64,
                buffer.as_mut_ptr().cast(),
                u32::try_from(length).ok()?,
                &mut returned,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            length = length.checked_mul(2)?;
            continue;
        }
        assert_eq!(status, 0, "NtQuerySystemInformation failed: {status:#x}");
        let count = usize::from_ne_bytes(buffer[0..8].try_into().unwrap());
        let size = std::mem::size_of::<HandleEntry>();
        assert!(16 + count * size <= buffer.len(), "handle table truncated");
        break (0..count)
            .map(|index| {
                let offset = 16 + index * size;
                unsafe {
                    std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<HandleEntry>())
                }
            })
            .collect::<Vec<_>>();
    };
    let process = unsafe { OpenProcess(0x40, 0, pid) };
    if process.is_null() {
        return None;
    }
    let mut handles = 0usize;
    let mut paths = Vec::new();
    for entry in entries {
        if entry.pid != pid as usize {
            continue;
        }
        let mut duplicate: *mut c_void = std::ptr::null_mut();
        let copied = unsafe {
            DuplicateHandle(
                process,
                entry.handle as *mut c_void,
                GetCurrentProcess(),
                &mut duplicate,
                0,
                0,
                2,
            )
        };
        if copied == 0 {
            continue;
        }
        handles += 1;
        let mut path = [0u16; 32768];
        let written = unsafe {
            GetFinalPathNameByHandleW(duplicate, path.as_mut_ptr(), path.len() as u32, 0)
        };
        unsafe {
            CloseHandle(duplicate);
        }
        if written > 0 && written as usize <= path.len() {
            paths.push(String::from_utf16_lossy(&path[..written as usize]));
        }
    }
    unsafe {
        CloseHandle(process);
    }
    Some(HandleSample { handles, paths })
}

async fn assert_cli_never_opens_client_sqlite(cli: &Path) {
    let status = cli_status(cli).await;
    let after = status["result"]["snapshot_version"]
        .as_str()
        .expect("snapshot version")
        .to_string();
    for _ in 0..3 {
        let mut child = Command::new(cli)
            .args([
                "--json",
                "subscribe-status",
                "--after-version",
                after.as_str(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut checks = 0usize;
        let mut handles_seen = 0usize;
        let mut paths = Vec::new();
        let end = Instant::now() + Duration::from_millis(800);
        while Instant::now() < end {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if let Some(sample) = sample_process(pid) {
                checks += 1;
                handles_seen = handles_seen.max(sample.handles);
                paths.extend(sample.paths);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let output = child.wait_with_output().unwrap();
        if checks == 0 {
            continue;
        }
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            output.status.success(),
            "subscribe stdout={stdout:?} stderr={stderr:?}"
        );
        assert!(
            handles_seen > 3,
            "handle enumeration must see the CLI process handles: {handles_seen}"
        );
        for path in &paths {
            assert!(
                !path.to_ascii_lowercase().contains("client.sqlite"),
                "CLI opened client.sqlite: {path}"
            );
        }
        println!(
            "check=pass cli_process_handles samples={checks} handles={handles_seen} files={} no_client_sqlite",
            paths.len()
        );
        return;
    }
    panic!("could not sample a live CLI management request");
}

// --- acceptance ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_cli_management_acceptance() {
    let fixture_path =
        PathBuf::from(std::env::var_os("VAULTSYNCD_PROCESS_FIXTURE").expect("server fixture"));
    let fixture: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture_path).unwrap()).unwrap();
    let agent_exe = PathBuf::from(std::env::var_os("VAULTSYNCD_AGENT_EXE").expect("agent exe"));
    let cli = PathBuf::from(std::env::var_os("VAULTSYNCD_CLI_EXE").expect("cli exe"));
    let base = fixture_path
        .parent()
        .unwrap()
        .join(format!("run-cli-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    for p in ["state", "root", "cache"] {
        std::fs::create_dir(base.join(p)).unwrap();
    }
    let server = identity();
    let binding = identity();
    let root = fixture["root"].as_str().unwrap().to_string();
    let entry = fixture["entry"]["entry_id"].as_str().unwrap().to_string();
    let payload = fixture["payload"].as_str().unwrap().as_bytes().to_vec();
    let config_path = base.join("agent.json");
    let config = serde_json::json!({
        "server": {"id": server, "url": fixture["url"], "development_http": true},
        "device_id": identity(),
        "state_dir": base.join("state"),
        "poll_millis": 250,
        "bindings": []
    });
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let agent = ChildGuard::spawn(&agent_exe, &config_path, &base.join("agent.log"));
    let mut cleanup = Cleanup {
        agent: Some(agent),
        server: server.clone(),
        base: base.clone(),
        binding: Some(binding.clone()),
        root: Some(root.clone()),
    };

    wait_agent(&cli).await;
    let initial = ok(call(&cli, cli_args(&["status"]), None).await);
    assert!(binding_status(&initial).is_empty());
    assert!(!initial.to_string().contains(PASSWORD));
    assert!(!initial.to_string().contains("access_token"));
    assert!(!initial.to_string().contains("refresh_token"));
    assert_eq!(
        initial["result"]["management"]["connection"]["authenticated"],
        false
    );

    // Unauthenticated and connection failures keep distinct stable exit codes.
    failed(
        call(&cli, cli_args(&["roots"]), None).await,
        4,
        "unauthenticated",
    );
    let wrong = call(
        &cli,
        cli_args(&["login", "--username", USER]),
        Some(b"wrong-password-not-logged\n".to_vec()),
    )
    .await;
    assert!(!wrong.stdout.contains("wrong-password-not-logged"));
    failed(wrong, 4, "unauthenticated");
    failed(
        call(
            &cli,
            cli_args(&["pause", "--binding-id", &identity()]),
            None,
        )
        .await,
        5,
        "not_found",
    );
    failed(
        call(
            &cli,
            cli_args(&["login", "--username", USER, "--password", PASSWORD]),
            None,
        )
        .await,
        2,
        "usage",
    );

    ok(call(
        &cli,
        cli_args(&[
            "set-connection",
            "--server-id",
            &server,
            "--server-url",
            fixture["url"].as_str().unwrap(),
        ]),
        None,
    )
    .await);
    let login = ok(call(
        &cli,
        cli_args(&["login", "--username", USER]),
        Some(format!("{PASSWORD}\n").into_bytes()),
    )
    .await);
    assert_eq!(
        login["result"]["management"]["connection"]["authenticated"],
        true
    );
    assert!(!login.to_string().contains(PASSWORD));
    assert!(!login.to_string().contains("access_token"));
    assert!(!login.to_string().contains("refresh_token"));

    let roots = ok(call(&cli, cli_args(&["roots"]), None).await);
    assert!(
        roots["result"]["management"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == root)
    );
    ok(call(&cli, cli_args(&["connection"]), None).await);

    ok(call(
        &cli,
        cli_args(&[
            "create-binding",
            "--binding-id",
            &binding,
            "--root-id",
            &root,
            "--local-path",
            base.join("root").to_str().unwrap(),
            "--cache-path",
            base.join("cache").to_str().unwrap(),
            "--mode",
            "on_demand",
        ]),
        None,
    )
    .await);
    await_status(
        &cli,
        |value| {
            binding_status(value)
                .first()
                .is_some_and(|b| b["metadata_only"] != "0")
        },
        "binding did not materialize metadata",
    )
    .await;
    let bindings = ok(call(&cli, cli_args(&["bindings"]), None).await);
    assert_eq!(
        bindings["result"]["management"]["bindings"][0]["binding_id"],
        binding
    );

    assert_cli_never_opens_client_sqlite(&cli).await;

    ok(call(&cli, cli_args(&["pause", "--binding-id", &binding]), None).await);
    await_status(
        &cli,
        |value| binding_status(value)[0]["paused"] == true,
        "pause was not visible",
    )
    .await;
    failed(
        call(
            &cli,
            cli_args(&["download", "--binding-id", &binding, "--entry-id", &entry]),
            None,
        )
        .await,
        5,
        "blocked",
    );
    ok(call(&cli, cli_args(&["resume", "--binding-id", &binding]), None).await);
    await_status(
        &cli,
        |value| binding_status(value)[0]["paused"] == false,
        "resume was not visible",
    )
    .await;

    ok(call(
        &cli,
        cli_args(&["download", "--binding-id", &binding, "--entry-id", &entry]),
        None,
    )
    .await);
    assert_eq!(std::fs::read(base.join("root/file.txt")).unwrap(), payload);
    failed(
        call(
            &cli,
            cli_args(&[
                "release",
                "--binding-id",
                &binding,
                "--entry-id",
                &identity(),
            ]),
            None,
        )
        .await,
        5,
        "not_found",
    );
    ok(call(
        &cli,
        cli_args(&["release", "--binding-id", &binding, "--entry-id", &entry]),
        None,
    )
    .await);
    let native = vaultsyncd_platform_windows::files::RootHandle::open(&base.join("root"))
        .unwrap()
        .inspect("file.txt")
        .unwrap();
    assert_eq!(native.cloud.unwrap().on_disk, 0);

    // Full mode requires real local bytes, so release is rejected until the user returns to
    // on-demand; that is an execution failure, not a silent success.
    ok(call(
        &cli,
        cli_args(&["mode", "--binding-id", &binding, "--mode", "full"]),
        None,
    )
    .await);
    await_status(
        &cli,
        |value| binding_status(value)[0]["mode_pending"] == false,
        "full mode did not converge",
    )
    .await;
    failed(
        call(
            &cli,
            cli_args(&["release", "--binding-id", &binding, "--entry-id", &entry]),
            None,
        )
        .await,
        5,
        "blocked",
    );
    ok(call(
        &cli,
        cli_args(&["mode", "--binding-id", &binding, "--mode", "on_demand"]),
        None,
    )
    .await);
    await_status(
        &cli,
        |value| binding_status(value)[0]["mode_pending"] == false,
        "on-demand mode did not converge",
    )
    .await;
    ok(call(
        &cli,
        cli_args(&["release", "--binding-id", &binding, "--entry-id", &entry]),
        None,
    )
    .await);

    ok(call(
        &cli,
        cli_args(&["problems", "--binding-id", &binding]),
        None,
    )
    .await);
    ok(call(
        &cli,
        cli_args(&["history", "--binding-id", &binding, "--entry-id", &entry]),
        None,
    )
    .await);
    ok(call(&cli, cli_args(&["trash", "--binding-id", &binding]), None).await);
    failed(
        call(
            &cli,
            cli_args(&["retry", "--binding-id", &binding, "--operation-id", "bad"]),
            None,
        )
        .await,
        5,
        "invalid_argument",
    );
    failed(
        call(
            &cli,
            cli_args(&[
                "restore",
                "--binding-id",
                &binding,
                "--entry-id",
                &identity(),
                "--operation-id",
                &identity(),
                "--base-revision",
                "0",
            ]),
            None,
        )
        .await,
        5,
        "conflict",
    );

    ok(call(&cli, cli_args(&["pause", "--binding-id", &binding]), None).await);
    std::fs::write(
        base.join("root/unique-unconfirmed.bin"),
        b"S073 unique local recovery bytes",
    )
    .unwrap();
    let export = base.join("recovery");
    let exported = ok(call(
        &cli,
        cli_args(&[
            "export-recovery",
            "--binding-id",
            &binding,
            "--destination",
            export.to_str().unwrap(),
        ]),
        None,
    )
    .await);
    assert!(
        exported["result"]["management"]["exported_files"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 1
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(export.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["files"].as_array().unwrap().iter().any(|file| {
        std::fs::read(export.join(file["name"].as_str().unwrap())).unwrap()
            == b"S073 unique local recovery bytes"
    }));
    failed(
        call(
            &cli,
            cli_args(&[
                "export-recovery",
                "--binding-id",
                &binding,
                "--destination",
                export.to_str().unwrap(),
            ]),
            None,
        )
        .await,
        5,
        "blocked",
    );

    let detached = ok(call(&cli, cli_args(&["detach", "--binding-id", &binding]), None).await);
    assert!(detached["result"]["management"]["unconfirmed"].is_array());
    assert_eq!(
        std::fs::read(base.join("root/unique-unconfirmed.bin")).unwrap(),
        b"S073 unique local recovery bytes"
    );
    failed(
        call(&cli, cli_args(&["resume", "--binding-id", &binding]), None).await,
        5,
        "blocked",
    );

    ok(call(&cli, cli_args(&["logout"]), None).await);
    failed(
        call(&cli, cli_args(&["roots"]), None).await,
        4,
        "unauthenticated",
    );
    ok(call(&cli, cli_args(&["status"]), None).await);

    cleanup.agent.as_mut().unwrap().kill();
    failed(
        call(&cli, cli_args(&["status"]), None).await,
        3,
        "agent_unavailable",
    );

    let log = std::fs::read_to_string(base.join("agent.log")).unwrap();
    assert!(!log.contains(PASSWORD));
    println!(
        "check=pass s073_real_cli_query_pause_resume_download_release_modes_history_trash_restore_export_connection_exit_codes_redaction"
    );
}
