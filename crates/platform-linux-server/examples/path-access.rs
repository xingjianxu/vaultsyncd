//! S032 安全路径访问诊断入口：Root 句柄下的列举/打开/创建、挂载边界与身份复核、目录重叠检查，
//! 输出稳定的 `key=value` 行，供 `tests/platform/linux/run-path-access.sh` 断言。
//!
//! 这是诊断入口，不是产品 CLI（产品入口在 `apps/server` 与管理面，按 design §5.3/§5.7 由后续卡
//! 实现）。角色路径由 [`RootHandle::open`] 探测并打开一次；之后所有访问都以该句柄为边界按相对
//! 名称进行，并核对对象的挂载 ID 仍是 Root 的挂载 ID。
//!
//! 退出码：`0` 成功；`1` I/O/探测错误；`2` 用法错误；`3` 拒绝执行（逃逸、符号链接、跨挂载、
//! 权限不足、身份漂移、重叠），此时没有改动目标目录。

use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::ExitCode;

use vaultsyncd_platform_linux_server::files::{
    FileIdentity, ResolveMode, RootDrift, RootHandle, RootSnapshot, RootSpecVerifyError,
};
use vaultsyncd_platform_linux_server::mounts::{
    DirectoryRecord, DirectoryRole, find_overlaps, probe_path,
};

const EXIT_OK: u8 = 0;
const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REFUSED: u8 = 3;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (args, resolve_mode) = match take_resolve_mode(&args) {
        Ok(value) => value,
        Err(message) => return usage(&message),
    };
    let Some((subcommand, rest)) = args.split_first() else {
        return usage("缺少子命令");
    };
    let result = match subcommand.as_str() {
        "list" => list(rest, resolve_mode),
        "open" => open(rest, resolve_mode),
        "create" => create(rest, resolve_mode),
        "snapshot" => snapshot(rest, resolve_mode),
        "verify" => verify(rest, resolve_mode),
        "overlap" => overlap(rest),
        other => return usage(&format!("未知子命令 '{other}'")),
    };
    match result {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(code) => code,
    }
}

/// 解析全局 `--resolve auto|openat2|walk`（可出现在任意位置，默认 `auto`）。
fn take_resolve_mode(args: &[String]) -> Result<(Vec<String>, ResolveMode), String> {
    let mut remaining = Vec::new();
    let mut mode = ResolveMode::Auto;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--resolve" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--resolve 缺少取值（auto|openat2|walk）".to_owned())?;
            mode = ResolveMode::parse(value)
                .ok_or_else(|| format!("未知的 --resolve 取值 '{value}'（auto|openat2|walk）"))?;
            index += 2;
            continue;
        }
        remaining.push(args[index].clone());
        index += 1;
    }
    Ok((remaining, mode))
}

fn usage(message: &str) -> ExitCode {
    eprintln!("{message}");
    eprintln!(
        "用法：path-access <子命令> [--resolve auto|openat2|walk]\n\
         \x20 list ROOT REL            （REL 为空串表示 Root 自身）\n\
         \x20 open ROOT REL\n\
         \x20 create ROOT REL [--mode OCTAL]\n\
         \x20 snapshot ROOT\n\
         \x20 verify --expect-from FILE ROOT\n\
         \x20 overlap (--root DIR | --internal DIR)..."
    );
    ExitCode::from(EXIT_USAGE)
}

fn fail(stage: &str, message: impl std::fmt::Display) -> ExitCode {
    println!("decision=failed\nreason={stage}\ndetail={message}");
    eprintln!("path-access: {stage}: {message}");
    ExitCode::from(EXIT_ERROR)
}

fn refused(stage: &str, reason: &str, detail: impl std::fmt::Display) -> ExitCode {
    println!("decision=refused\nreason={reason}\ndetail={detail}");
    eprintln!("path-access: {stage}: {reason}: {detail}");
    ExitCode::from(EXIT_REFUSED)
}

fn refused_drift(drift: RootDrift) -> ExitCode {
    println!("decision=refused");
    println!("reason={}", drift.code());
    println!("design_error={}", drift.design_error_code());
    ExitCode::from(EXIT_REFUSED)
}

fn open_root(path: &str, mode: ResolveMode) -> Result<RootHandle, ExitCode> {
    RootHandle::open(Path::new(path), mode).map_err(|error| {
        fail(
            error.code(),
            format!("{}：{error}", Path::new(path).display()),
        )
    })
}

fn list(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let (root, relative) = match raw {
        [root, relative] => (root, relative),
        _ => return Err(usage("list 需要 ROOT 与 REL 两个参数（REL 可为空串）")),
    };
    let handle = open_root(root, resolve_mode)?;
    let entries = handle
        .list_dir(Path::new(relative))
        .map_err(|error| refused("list", error.code(), error.to_string()))?;
    println!("decision=listed");
    println!("root={}", handle.path().display());
    println!("relative={relative}");
    println!("resolve={}", handle.mechanism());
    println!("root_mount_id={}", handle.snapshot().mount_id);
    println!("entries={}", entries.len());
    for entry in &entries {
        println!(
            "entry={} kind={} inode={}",
            entry.name.to_string_lossy(),
            entry.kind.as_str(),
            entry.inode
        );
    }
    Ok(())
}

fn open(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let (root, relative) = match raw {
        [root, relative] => (root, relative),
        _ => return Err(usage("open 需要 ROOT 与 REL 两个参数")),
    };
    let handle = open_root(root, resolve_mode)?;
    let opened = handle
        .open_read(Path::new(relative))
        .map_err(|error| refused("open", error.code(), error.to_string()))?;
    println!("decision=opened");
    println!("relative={relative}");
    println!("resolve={}", handle.mechanism());
    println!("root_mount_id={}", handle.snapshot().mount_id);
    print!("{}", opened.identity.to_spec());
    Ok(())
}

fn create(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let mut positional = Vec::new();
    let mut mode = 0o644u32;
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == "--mode" {
            let value = raw
                .get(index + 1)
                .ok_or_else(|| usage("--mode 缺少八进制取值"))?;
            mode = u32::from_str_radix(value.trim(), 8)
                .map_err(|_| usage(&format!("--mode 取值 '{value}' 不是八进制")))?;
            index += 2;
            continue;
        }
        positional.push(raw[index].clone());
        index += 1;
    }
    let (root, relative) = match positional.as_slice() {
        [root, relative] => (root, relative),
        _ => return Err(usage("create 需要 ROOT 与 REL 两个参数")),
    };
    let handle = open_root(root, resolve_mode)?;
    let file = handle
        .create_exclusive(Path::new(relative), mode)
        .map_err(|error| refused("create", error.code(), error.to_string()))?;
    file.sync_all()
        .map_err(|error| fail("fsync_failed", format!("{relative}: {error}")))?;
    let identity = FileIdentity::of_fd(file.as_raw_fd())
        .map_err(|error| fail("stat_failed", format!("{relative}: {error}")))?;
    println!("decision=created");
    println!("relative={relative}");
    println!("resolve={}", handle.mechanism());
    println!("root_mount_id={}", handle.snapshot().mount_id);
    println!(
        "mount_id={}",
        identity
            .mount_id
            .map_or_else(|| "none".to_owned(), |id| id.to_string())
    );
    println!("inode={}", identity.inode);
    println!("mode={:04o}", identity.permission_bits());
    println!("fsynced=true");
    Ok(())
}

fn snapshot(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let [root] = raw else {
        return Err(usage("snapshot 需要 ROOT 一个参数"));
    };
    let handle = open_root(root, resolve_mode)?;
    println!("decision=snapshot");
    println!("resolve={}", handle.mechanism());
    print!("{}", handle.snapshot().to_spec());
    Ok(())
}

fn verify(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let mut expect_from: Option<&str> = None;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == "--expect-from" {
            expect_from = Some(
                raw.get(index + 1)
                    .ok_or_else(|| usage("--expect-from 缺少文件路径"))?,
            );
            index += 2;
            continue;
        }
        positional.push(raw[index].clone());
        index += 1;
    }
    let Some(file) = expect_from else {
        return Err(usage("verify 需要 --expect-from FILE"));
    };
    let [root] = positional.as_slice() else {
        return Err(usage("verify 需要 ROOT 一个参数"));
    };
    let text = std::fs::read_to_string(file)
        .map_err(|error| fail("spec_read_failed", format!("{file}: {error}")))?;
    let recorded = RootSnapshot::parse_spec(&text)
        .map_err(|error| fail("spec_invalid", format!("{file}: {error}")))?;

    let handle = open_root(root, resolve_mode)?;
    if let Err(drift) = recorded.compare(handle.facts()) {
        return Err(refused_drift(drift));
    }
    match handle.verify() {
        Ok(()) => {
            println!("decision=verified");
            println!("verify=unchanged");
            println!("root={}", handle.path().display());
            println!("mount_id={}", handle.snapshot().mount_id);
            Ok(())
        }
        Err(RootSpecVerifyError::Drift(drift)) => Err(refused_drift(drift)),
        Err(error) => Err(fail("verify_failed", error)),
    }
}

fn overlap(raw: &[String]) -> Result<(), ExitCode> {
    let mut specs: Vec<(DirectoryRole, String)> = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        let role = match raw[index].as_str() {
            "--root" => DirectoryRole::Root,
            "--internal" => DirectoryRole::Internal,
            other => return Err(usage(&format!("未知的 overlap 参数 '{other}'"))),
        };
        let path = raw
            .get(index + 1)
            .ok_or_else(|| usage("--root/--internal 缺少目录路径"))?;
        specs.push((role, path.clone()));
        index += 2;
    }
    if specs.len() < 2 {
        return Err(usage("overlap 至少需要两个 --root/--internal 目录"));
    }

    let mut records = Vec::new();
    for (role, path) in &specs {
        let facts = probe_path(Path::new(path)).map_err(|error| {
            fail(
                "probe_failed",
                format!(
                    "{}（{}）：{error}",
                    Path::new(path).display(),
                    role.as_str()
                ),
            )
        })?;
        records.push(DirectoryRecord::new(*role, facts));
    }

    println!("records={}", records.len());
    for (position, record) in records.iter().enumerate() {
        println!(
            "record={position} role={} path={} resolved={} mount_id={} dev={}:{} inode={}",
            record.role.as_str(),
            record.facts.given_path.display(),
            record.facts.resolved_path.display(),
            record.facts.mount.mount_id,
            record.facts.device.major,
            record.facts.device.minor,
            record.facts.inode,
        );
    }

    let overlaps = find_overlaps(&records);
    println!("overlap={}", overlaps.len());
    for found in &overlaps {
        println!("{}", found.render(&records));
    }
    if overlaps.is_empty() {
        println!("decision=clean");
        Ok(())
    } else {
        println!("decision=overlapping");
        Err(ExitCode::from(EXIT_REFUSED))
    }
}
