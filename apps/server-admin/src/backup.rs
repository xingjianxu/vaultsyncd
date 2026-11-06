//! 在线备份仅经同UID socket；本地.vsbk验证只读取导出文件，不打开在线数据库。
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vaultsyncd_domain::ids::BackupLeaseId;
use vaultsyncd_platform_linux_server::ipc;
use vaultsyncd_server_core::{
    backup::export::{ArchiveVerifier, CHUNK},
    objects::ContentDigest,
};

pub const USAGE: &str = "backup prepare [--id ID] --socket PATH [--json]\nbackup status ID --socket PATH [--json]\nbackup export ID --output ABSOLUTE.vsbk --socket PATH [--json]\nbackup verify ABSOLUTE.vsbk [--json]\nbackup verify-source ID --socket PATH [--json]\nbackup release ID --socket PATH [--json]\n导出不覆盖既有文件。exported_unverified不是完整性验收；verify逐项重读后才写verified收据。prepare/verify-source/release断线后可用status查询，后台任务仍保留。";
struct Args {
    op: String,
    operand: Option<String>,
    socket: Option<PathBuf>,
    output: Option<PathBuf>,
    id: Option<String>,
    json: bool,
}
fn parse(args: Vec<OsString>) -> io::Result<Args> {
    let mut tokens = args.into_iter();
    if tokens.next().as_deref() != Some(std::ffi::OsStr::new("backup")) {
        return Err(bad("expected backup"));
    }
    let op = tokens
        .next()
        .ok_or_else(|| bad("missing backup operation"))?
        .into_string()
        .map_err(|_| bad("invalid UTF-8"))?;
    let mut out = Args {
        op,
        operand: None,
        socket: None,
        output: None,
        id: None,
        json: false,
    };
    while let Some(token) = tokens.next() {
        let text = token.into_string().map_err(|_| bad("invalid UTF-8"))?;
        match text.as_str() {
            "--socket" if out.socket.is_none() => {
                out.socket = Some(PathBuf::from(
                    tokens.next().ok_or_else(|| bad("missing socket"))?,
                ))
            }
            "--output" if out.output.is_none() => {
                out.output = Some(PathBuf::from(
                    tokens.next().ok_or_else(|| bad("missing output"))?,
                ))
            }
            "--id" if out.id.is_none() => {
                out.id = Some(
                    tokens
                        .next()
                        .ok_or_else(|| bad("missing ID"))?
                        .into_string()
                        .map_err(|_| bad("invalid ID"))?,
                )
            }
            "--json" if !out.json => out.json = true,
            s if !s.starts_with('-') && out.operand.is_none() => out.operand = Some(s.to_owned()),
            _ => return Err(bad("unknown or duplicate backup argument")),
        }
    }
    if ![
        "prepare",
        "status",
        "export",
        "verify",
        "verify-source",
        "release",
    ]
    .contains(&out.op.as_str())
    {
        return Err(bad("unknown backup operation"));
    }
    if out.op == "verify" {
        if out.operand.is_none() || out.socket.is_some() || out.output.is_some() || out.id.is_some()
        {
            return Err(bad("verify requires only an absolute archive path"));
        }
    } else {
        if out.socket.as_ref().is_none_or(|p| !p.is_absolute()) {
            return Err(bad("online backup needs absolute --socket"));
        }
        if out.op == "prepare" {
            if out.operand.is_some() || out.output.is_some() {
                return Err(bad("prepare takes optional --id only"));
            }
        } else if out.operand.is_none()
            || out.id.is_some()
            || ((out.op == "export") != out.output.is_some())
        {
            return Err(bad("backup arguments do not match operation"));
        }
    }
    if out.output.as_ref().is_some_and(|p| !p.is_absolute()) {
        return Err(bad("output must be absolute"));
    }
    Ok(out)
}
fn bad(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s)
}
fn failure(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn identity(s: &str) -> io::Result<BackupLeaseId> {
    BackupLeaseId::from_hex(s)
        .ok()
        .filter(|id| id.to_hex() == s)
        .ok_or_else(|| bad("ID must be 32 lowercase hex digits"))
}
async fn request(socket: &Path, op: &str, id: BackupLeaseId) -> io::Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = ipc::connect(socket).await?;
        let command = ["v1", "backup", op, &id.to_hex()];
        ipc::write_frame(&mut stream, &serde_json::to_vec(&command).map_err(failure)?).await?;
        let value: Value =
            serde_json::from_slice(&ipc::read_frame(&mut stream).await?).map_err(failure)?;
        if !value["status"]
            .as_u64()
            .is_some_and(|s| (200..300).contains(&s))
        {
            return Err(failure(format!("backup {}: {}", id, value["error"])));
        }
        Ok(value["result"].clone())
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("backup {} result unknown; query status", id),
        )
    })?
}
async fn wait(socket: &Path, id: BackupLeaseId, op: &str) -> io::Result<Value> {
    let started = Instant::now();
    loop {
        let value = request(socket, "status", id).await?;
        let done = match op {
            "prepare" => value["phase"] == "ready",
            "verify" => value["integrity"] == "verified",
            "release" => value["phase"] == "released",
            _ => false,
        };
        if !value["job_pending"]
            .as_bool()
            .ok_or_else(|| failure("invalid backup status"))?
        {
            return if done {
                Ok(value)
            } else {
                Err(failure(format!("backup {id} failed: {value}")))
            };
        }
        if started.elapsed() > Duration::from_secs(600) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("backup {id} still pending; query status"),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
fn receipt_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".verification.json");
    PathBuf::from(name)
}
fn receipt(path: &Path, value: &Value, initial: bool) -> io::Result<()> {
    use std::io::Write;
    let target = receipt_path(path);
    let tmp = if initial {
        target.clone()
    } else {
        let mut n = target.as_os_str().to_os_string();
        n.push(format!(".{}.tmp", std::process::id()));
        PathBuf::from(n)
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(failure)?)?;
    file.sync_all()?;
    if !initial {
        std::fs::rename(&tmp, &target)?;
    }
    std::fs::File::open(
        target
            .parent()
            .ok_or_else(|| bad("receipt parent missing"))?,
    )?
    .sync_all()
}
async fn export(socket: &Path, id: BackupLeaseId, path: &Path) -> io::Result<Value> {
    let mut stream = ipc::connect(socket).await?;
    ipc::write_frame(
        &mut stream,
        &serde_json::to_vec(&["v1", "backup", "export", &id.to_hex()]).map_err(failure)?,
    )
    .await?;
    let head = tokio::time::timeout(Duration::from_secs(15), ipc::read_frame(&mut stream))
        .await
        .map_err(|_| failure("export handshake deadline"))??;
    let head: Value = serde_json::from_slice(&head).map_err(failure)?;
    if head["status"] != 200 {
        return Err(failure(format!("export denied: {}", head["error"])));
    }
    if head["result"]["backup_id"] != id.to_hex() {
        return Err(failure("export backup identity mismatch"));
    }
    let hash = ContentDigest::from_hex(
        head["result"]["manifest_hash"]
            .as_str()
            .ok_or_else(|| failure("manifest hash missing"))?,
    )
    .map_err(failure)?;
    if receipt_path(path).try_exists()? {
        return Err(bad("receipt exists; use a new output path"));
    }
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let mut output = tokio::fs::File::from_std(output);
    let mut status = json!({"format_version":1,"state":"incomplete","backup_id":id.to_hex(),"manifest_hash":hash.to_hex(),"reason":"export in progress"});
    receipt(path, &status, true)?;
    let outcome = async {
        let mut verifier = ArchiveVerifier::new(*hash.as_bytes());
        let mut chunk = vec![0; CHUNK];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(45), stream.read(&mut chunk))
                .await
                .map_err(|_| failure("export idle deadline"))??;
            if n == 0 {
                break;
            }
            verifier.push(&chunk[..n]).map_err(failure)?;
            output.write_all(&chunk[..n]).await?;
        }
        let index = verifier.finish().map_err(failure)?;
        if index.id != id {
            return Err(failure("archive identity mismatch"));
        }
        output.sync_all().await?;
        Ok(index)
    }
    .await;
    match outcome {
        Ok(index) => {
            status["state"] = "exported_unverified".into();
            status["reason"] = "durable export; run verify".into();
            status["server_id"] = index.server.to_hex().into();
            status["checked_files"] = index.items.len().to_string().into();
            receipt(path, &status, false)?;
            Ok(status)
        }
        Err(error) => {
            status["reason"] = error.to_string().into();
            receipt(path, &status, false)?;
            Err(error)
        }
    }
}
async fn verify(path: &Path) -> io::Result<Value> {
    if !path.is_absolute() {
        return Err(bad("verify path must be absolute"));
    }
    let mut status: Value =
        serde_json::from_slice(&std::fs::read(receipt_path(path))?).map_err(failure)?;
    if status["format_version"] != 1 {
        return Err(bad("unsupported export receipt"));
    }
    let id = identity(
        status["backup_id"]
            .as_str()
            .ok_or_else(|| bad("receipt ID missing"))?,
    )?;
    let expected = ContentDigest::from_hex(
        status["manifest_hash"]
            .as_str()
            .ok_or_else(|| bad("receipt manifest hash missing"))?,
    )
    .map_err(failure)?;
    // 显式verify开始先撤销旧成功收据：取消或SIGKILL不能留下本次未读完的verified。
    status["state"] = "incomplete".into();
    status["reason"] = "verification in progress".into();
    receipt(path, &status, false)?;
    let outcome = async {
        let mut file = tokio::fs::File::open(path).await?;
        let mut verifier = ArchiveVerifier::new(*expected.as_bytes());
        let mut chunk = vec![0; CHUNK];
        loop {
            let n = file.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            verifier.push(&chunk[..n]).map_err(failure)?;
        }
        let index = verifier.finish().map_err(failure)?;
        if index.id != id {
            return Err(failure("receipt/archive backup ID mismatch"));
        }
        Ok(index)
    }
    .await;
    match outcome {
        Ok(index) => {
            status["state"] = "verified".into();
            status["reason"] = "all durable archive bytes and terminal marker verified".into();
            status["checked_files"] = index.items.len().to_string().into();
            receipt(path, &status, false)?;
            Ok(status)
        }
        Err(error) => {
            status["state"] = "incomplete".into();
            status["reason"] = error.to_string().into();
            receipt(path, &status, false)?;
            Err(error)
        }
    }
}
/// 所有密码/在线DB参数均拒绝；export与verify可清晰区分。
pub async fn run(args: Vec<OsString>) -> u8 {
    let args = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return 2;
        }
    };
    let outcome = async {
        if args.op == "verify" {
            return verify(Path::new(
                args.operand
                    .as_deref()
                    .ok_or_else(|| bad("archive missing"))?,
            ))
            .await;
        }
        let socket = args.socket.as_ref().ok_or_else(|| bad("socket missing"))?;
        let id = if args.op == "prepare" {
            match &args.id {
                Some(id) => identity(id)?,
                None => {
                    use rand_core::RngCore;
                    let mut bytes = [0; 16];
                    rand_core::OsRng.fill_bytes(&mut bytes);
                    BackupLeaseId::from_bytes(bytes)
                }
            }
        } else {
            identity(
                args.operand
                    .as_deref()
                    .ok_or_else(|| bad("backup ID missing"))?,
            )?
        };
        match args.op.as_str() {
            "status" => request(socket, "status", id).await,
            "export" => {
                export(
                    socket,
                    id,
                    args.output.as_ref().ok_or_else(|| bad("output missing"))?,
                )
                .await
            }
            "prepare" | "release" | "verify-source" => {
                let op = if args.op == "verify-source" {
                    "verify"
                } else {
                    &args.op
                };
                request(socket, op, id).await?;
                wait(socket, id, op).await
            }
            _ => Err(bad("unknown operation")),
        }
    }
    .await;
    match outcome {
        Ok(value) => {
            if args.json {
                println!("{value}")
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).expect("JSON value")
                )
            }
            0
        }
        Err(error) => {
            if args.json {
                println!(
                    "{}",
                    json!({"state":"incomplete","error":error.to_string()})
                )
            } else {
                eprintln!("backup failed: {error}")
            }
            1
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }
    #[test]
    fn strict_backup_arguments_separate_offline_verify_and_online_commands() {
        assert!(parse(args(&["backup", "verify", "/tmp/a.vsbk"])).is_ok());
        assert!(parse(args(&["backup", "prepare", "--socket", "/run/admin"])).is_ok());
        for a in [
            vec!["backup", "export", "id", "--socket", "/s"],
            vec!["backup", "verify", "/a", "--socket", "/s"],
            vec![
                "backup",
                "prepare",
                "--socket",
                "/s",
                "--password",
                "secret",
            ],
            vec!["backup", "release", "id", "--socket", "relative"],
        ] {
            assert!(parse(args(&a)).is_err());
        }
    }
}
