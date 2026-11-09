//! S014 文件发布原型命令：受限目录句柄下的读取、暂存、刷盘与受控替换，输出稳定的
//! `key=value` 行，供 `tests/platform/linux/run-file-publish.sh` 断言。
//!
//! 这是原型诊断入口，不是产品 CLI（产品入口在 `apps/server` 与管理面，按 design §5.3/§5.7
//! 由后续卡实现）。角色路径在启动时 `canonicalize` 一次（模拟注册期处理）；运行期所有文件操作
//! 都以该目录句柄为边界按相对名称进行。
//!
//! 退出码：`0` 成功；`1` I/O 错误；`2` 用法错误；`3` 拒绝执行（逃逸/符号链接/前提不成立/
//! 跨文件系统重命名），此时目标未被修改。

use std::path::Path;
use std::process::ExitCode;

use vaultsyncd_platform_linux_server::files::{
    self, ExpectedTarget, FileIdentity, PublishError, ResolveMode, RestrictedDir,
    validate_relative_path,
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
        "identity" => identity(rest, resolve_mode),
        "hash" => hash(rest, resolve_mode),
        "capture" => capture(rest, resolve_mode),
        "publish" => publish(rest, resolve_mode),
        "rename" => rename(rest, resolve_mode),
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
        "用法：file-publish <子命令> [--resolve auto|openat2|walk]\n\
         \x20 identity VAULT REL\n\
         \x20 hash VAULT REL\n\
         \x20 capture --source-vault DIR --source REL --object-vault DIR --object REL\n\
         \x20 publish --source-vault DIR --source REL --target-vault DIR --target-dir REL \
         --target-name NAME (--expect absent | --expect-from FILE) [--race-hook CMD]\n\
         \x20 rename --from-vault DIR --from REL --to-vault DIR --to REL [--noreplace]"
    );
    ExitCode::from(EXIT_USAGE)
}

fn fail(stage: &str, message: impl std::fmt::Display) -> ExitCode {
    println!("decision=failed\nreason={stage}\ndetail={message}");
    eprintln!("file-publish: {stage}: {message}");
    ExitCode::from(EXIT_ERROR)
}

/// 角色目录在其生命周期内是边界；这里一次 `canonicalize` 只模拟注册期处理，运行期不再用路径。
fn open_vault(path: &Path, mode: ResolveMode, label: &str) -> Result<RestrictedDir, ExitCode> {
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        fail(
            "canonicalize_failed",
            format!("{}：{error}", path.display()),
        )
    })?;
    RestrictedDir::open(&canonical, mode).map_err(|error| {
        println!("decision=failed\nreason={}\ndetail={error}", error.code());
        eprintln!("file-publish: {label} 打开失败：{error}");
        ExitCode::from(EXIT_ERROR)
    })
}

fn identity(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let (vault, relative) = match raw {
        [vault, relative] => (vault, relative),
        _ => return Err(usage("identity 需要 VAULT 与 REL 两个参数")),
    };
    let dir = open_vault(Path::new(vault), resolve_mode, "VAULT")?;
    let opened = dir
        .open_path(Path::new(relative))
        .map_err(|error| print_refused("identity", &error.to_string(), error.code()))?;
    print!("{}", opened.identity.to_spec());
    println!("resolve={}", dir.mechanism());
    Ok(())
}

fn hash(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let (vault, relative) = match raw {
        [vault, relative] => (vault, relative),
        _ => return Err(usage("hash 需要 VAULT 与 REL 两个参数")),
    };
    let dir = open_vault(Path::new(vault), resolve_mode, "VAULT")?;
    let opened = dir
        .open_read(Path::new(relative))
        .map_err(|error| print_refused("hash", &error.to_string(), error.code()))?;
    let (digest, bytes) = files::hash_file(&opened.file)
        .map_err(|error| fail("hash_failed", format!("{relative}: {error}")))?;
    println!("decision=hashed");
    println!("sha256={}", files::hash::hex(&digest));
    println!("bytes={bytes}");
    println!("resolve={}", dir.mechanism());
    Ok(())
}

fn capture(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let parsed = ParsedArgs::parse(
        raw,
        &["--source-vault", "--source", "--object-vault", "--object"],
        &[],
    )?;
    let source_root = parsed.required("--source-vault")?;
    let source_rel = parsed.required("--source")?;
    let object_root = parsed.required("--object-vault")?;
    let object_rel = parsed.required("--object")?;

    let source_dir = open_vault(Path::new(&source_root), resolve_mode, "source-vault")?;
    let object_dir = open_vault(Path::new(&object_root), resolve_mode, "object-vault")?;
    let relative = Path::new(&source_rel);
    let opened = source_dir
        .open_read(relative)
        .map_err(|error| print_refused("capture", &error.to_string(), error.code()))?;
    let outcome = files::capture_object(
        &opened.file,
        &opened.identity,
        &object_dir,
        Path::new(&object_rel),
    )
    .map_err(print_publish_error)?;
    print!("{}", files::render_outcome(&outcome));
    println!("source_root={}", source_dir.path().display());
    println!("object_root={}", object_dir.path().display());
    println!("object={object_rel}");
    println!("candidate=none");
    Ok(())
}

fn publish(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let parsed = ParsedArgs::parse(
        raw,
        &[
            "--source-vault",
            "--source",
            "--target-vault",
            "--target-dir",
            "--target-name",
            "--expect",
            "--expect-from",
            "--race-hook",
        ],
        &[],
    )?;
    let source_root = parsed.required("--source-vault")?;
    let source_rel = parsed.required("--source")?;
    let target_vault = parsed.required("--target-vault")?;
    let target_dir_rel = parsed.required("--target-dir")?;
    let target_name = parsed.required("--target-name")?;

    let expect_from = parsed.optional("--expect-from");
    let expect = parsed.optional("--expect");
    let expected = match (expect.as_deref(), expect_from) {
        (Some("absent"), None) => ExpectedTarget::Absent,
        (None, Some(path)) => ExpectedTarget::Present(read_identity_file(&path)?),
        (Some(other), None) => {
            return Err(usage(&format!(
                "--expect 只接受 absent（当前为 '{other}'）；要按身份核对请用 --expect-from"
            )));
        }
        _ => {
            return Err(usage(
                "publish 需要且只需要一个前提：--expect absent 或 --expect-from FILE",
            ));
        }
    };
    validate_relative_path(Path::new(&target_name))
        .map_err(|error| print_refused("publish", &error.to_string(), "invalid_relative_path"))?;

    let source_dir = open_vault(Path::new(&source_root), resolve_mode, "source-vault")?;
    let target_vault_handle = open_vault(Path::new(&target_vault), resolve_mode, "target-vault")?;
    let target_dir = target_vault_handle
        .child_dir(Path::new(&target_dir_rel))
        .map_err(|error| print_refused("publish", &error.to_string(), error.code()))?;
    let relative = Path::new(&source_rel);
    let opened = source_dir
        .open_read(relative)
        .map_err(|error| print_refused("publish", &error.to_string(), error.code()))?;
    // 两步发布：产品侧会在这两步之间持久化 fs_intent；原型命令允许在这里注入一次**真实的**
    // 外部写入（`--race-hook`），用来验证“替换前复核失败就暂停”。它不是产品的写入路径。
    let candidate = files::stage_for_publish(
        &opened.file,
        &opened.identity,
        &target_dir,
        Path::new(&target_name),
        &expected,
    )
    .map_err(print_publish_error)?;
    if let Some(command) = parsed.optional("--race-hook") {
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .status()
            .map_err(|error| fail("race_hook_failed", format!("{command}: {error}")))?;
        println!("race_hook={command}");
        println!("race_hook_exit={}", status.code().unwrap_or(-1));
    }
    let outcome = files::commit_staged(candidate, Path::new(&target_name), files::WORK_FILE_MODE)
        .map_err(print_publish_error)?;
    print!("{}", files::render_outcome(&outcome));
    println!("source_root={}", source_dir.path().display());
    println!("target_dir={}", target_dir.path().display());
    println!("target_name={target_name}");
    println!("candidate=none");
    Ok(())
}

fn rename(raw: &[String], resolve_mode: ResolveMode) -> Result<(), ExitCode> {
    let parsed = ParsedArgs::parse(
        raw,
        &["--from-vault", "--from", "--to-vault", "--to"],
        &["--noreplace"],
    )?;
    let from_root = parsed.required("--from-vault")?;
    let from_rel = parsed.required("--from")?;
    let to_root = parsed.required("--to-vault")?;
    let to_rel = parsed.required("--to")?;
    let noreplace = parsed.has_flag("--noreplace");

    let source_dir = open_vault(Path::new(&from_root), resolve_mode, "from-vault")?;
    let target_dir = open_vault(Path::new(&to_root), resolve_mode, "to-vault")?;
    match source_dir.rename(
        Path::new(&from_rel),
        &target_dir,
        Path::new(&to_rel),
        noreplace,
    ) {
        Ok(()) => {
            println!("decision=renamed");
            println!("noreplace={noreplace}");
            println!("from={from_rel}");
            println!("to={to_rel}");
            Ok(())
        }
        Err(error) => Err(print_refused("rename", &error.to_string(), error.code())),
    }
}

fn read_identity_file(path: &str) -> Result<FileIdentity, ExitCode> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| fail("identity_file_unreadable", format!("{path}: {error}")))?;
    files::parse_identity_spec(&text)
        .map_err(|error| usage(&format!("前提文件 {path} 无效：{error}")))
}

/// 打印“拒绝执行”（退出码 3）：目标未被修改，原因稳定可断言。
fn print_refused(stage: &str, detail: &str, code: &str) -> ExitCode {
    println!("decision=paused");
    println!("reason={code}");
    println!("detail={detail}");
    println!("stage={stage}");
    eprintln!("file-publish: {stage}: {code}: {detail}");
    ExitCode::from(EXIT_REFUSED)
}

fn print_publish_error(error: PublishError) -> ExitCode {
    let paused = error.is_refusal();
    println!("decision={}", if paused { "paused" } else { "failed" });
    println!("reason={}", error.code());
    println!("detail={error}");
    if let Some(refusal) = &error.refusal {
        println!("expected={}", refusal.expected);
        println!("observed={}", refusal.observed);
        println!("changed_fields={}", refusal.changed_fields.join(","));
    }
    match &error.candidate {
        Some(candidate) => {
            println!("candidate={}", candidate.name);
            println!("candidate_dir={}", candidate.directory.display());
            println!("candidate_sha256={}", files::hash::hex(&candidate.sha256));
        }
        None => println!("candidate=none"),
    }
    eprintln!("file-publish: publish: {}", error.reason);
    ExitCode::from(if paused { EXIT_REFUSED } else { EXIT_ERROR })
}

/// 极简参数解析：`--flag value` 与开关；未知参数直接报用法错误，不静默忽略。
struct ParsedArgs {
    values: Vec<(String, String)>,
    flags: Vec<String>,
}

impl ParsedArgs {
    fn parse(raw: &[String], with_value: &[&str], switches: &[&str]) -> Result<Self, ExitCode> {
        let mut values = Vec::new();
        let mut flags = Vec::new();
        let mut index = 0;
        while index < raw.len() {
            let key = raw[index].clone();
            if switches.iter().any(|flag| *flag == key) {
                flags.push(key);
                index += 1;
                continue;
            }
            if with_value.iter().any(|flag| *flag == key) {
                let value = raw
                    .get(index + 1)
                    .cloned()
                    .ok_or_else(|| usage(&format!("{key} 缺少取值")))?;
                values.push((key, value));
                index += 2;
                continue;
            }
            return Err(usage(&format!("未知参数 '{key}'")));
        }
        Ok(Self { values, flags })
    }

    fn optional(&self, key: &str) -> Option<String> {
        self.values
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }

    fn required(&self, key: &str) -> Result<String, ExitCode> {
        self.optional(key)
            .ok_or_else(|| usage(&format!("缺少必需参数 {key}")))
    }

    fn has_flag(&self, key: &str) -> bool {
        self.flags.iter().any(|flag| flag == key)
    }
}
