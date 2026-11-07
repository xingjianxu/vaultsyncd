//! vaultsyncd 的任务编排入口（docs/design.md §5.2）。
//!
//! 构建、生成、检查与发布逻辑集中在 `tools/xtask`，CI 与平台薄脚本只调用
//! `cargo xtask <子命令>`，不各自复制一套命令。子命令契约与退出码见
//! `docs/architecture/interfaces.md`。
//!
//! 已实现环境信息、既有检查与协议生成（S024）；产品打包尚未实现，对应子命令明确报
//! 未实现并以非零退出，不做假动作。

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

mod boundaries;
mod generate;
mod release;

/// 子命令成功。
pub(crate) const EXIT_OK: u8 = 0;
/// 检查失败、工具缺失或运行环境不满足前置条件。
pub(crate) const EXIT_FAILURE: u8 = 1;
/// 用法错误：未知子命令或多余参数。
pub(crate) const EXIT_USAGE: u8 = 2;
/// 子命令已规划但本步尚未实现。
const EXIT_UNIMPLEMENTED: u8 = 3;

/// 帮助文本。子命令清单、行为与退出码在此集中说明。
const USAGE: &str = "\
vaultsyncd 任务编排（xtask）。用法：cargo xtask <子命令>

子命令：
  env       打印工具链、workspace 成员与关键路径（只读诊断，退出码 0）
  check     先核对三项目标依赖方向、前端消费者边界与生成一致性，再依次运行
            cargo fmt --check、
            cargo clippy --locked --all-targets -- -D warnings、
            cargo test --locked、pnpm --recursive run typecheck、pnpm --recursive --if-present run test
  ci        执行 tools/ci/run.py 的单一平台 job 命令目录；--plan 只列命令不算验证
  check-boundaries  按 Linux x64/ARM64、Windows x64 metadata 检查依赖方向、生成一致与 pnpm 消费者
  generate  从 api/openapi.yaml 生成远端 Rust DTO 与 TypeScript 客户端，从
            api/local-ipc.schema.json 生成本机 IPC Rust DTO 与 TypeScript 类型；
            `--check` 只比对不写入，用于 CI 与“重新生成无 diff”验收
  version   检查产品版本镜像；--sync 从 Cargo workspace.version 同步JSON（不改协议）
  release   build/assemble/verify 真实产物记录与同提交清单
  package   产品打包（尚未实现）
  help      显示本帮助

退出码：0 成功；1 检查失败或前置条件不满足；2 用法错误；3 子命令尚未实现；78 CI 平台资源缺失（blocked）。
命令契约与实现状态见 docs/architecture/interfaces.md。
";

/// `check` 顺序执行的一步。
struct Step {
    /// 步骤名，用于打印和失败摘要。
    name: &'static str,
    /// 可执行文件。
    program: &'static str,
    /// 参数列表。
    args: &'static [&'static str],
}

/// `check` 的步骤表：只编排已有检查，顺序从快到慢。
const CHECK_STEPS: &[Step] = &[
    Step {
        name: "rust-fmt",
        program: "cargo",
        args: &["fmt", "--check"],
    },
    Step {
        name: "rust-clippy",
        program: "cargo",
        args: &[
            "clippy",
            "--locked",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    },
    Step {
        name: "rust-test",
        program: "cargo",
        args: &["test", "--locked"],
    },
    Step {
        name: "frontend-typecheck",
        program: "pnpm",
        args: &["--recursive", "run", "typecheck"],
    },
    Step {
        name: "frontend-contract-tests",
        program: "pnpm",
        args: &["--recursive", "--if-present", "run", "test"],
    },
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("xtask: 缺少子命令");
        eprint!("{USAGE}");
        return ExitCode::from(EXIT_USAGE);
    };
    let rest: Vec<String> = args.collect();

    let code = match command.as_str() {
        "env" => run_env(&rest),
        "check" => run_check(&rest),
        "ci" => run_ci(&rest),
        "check-boundaries" => boundaries::run(&rest),
        "generate" => generate::run(&rest),
        "version" => release::run_version(&rest),
        "release" => release::run(&rest),
        "package" => run_unimplemented("package", &rest),
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            EXIT_OK
        }
        unknown => {
            eprintln!("xtask: 未知子命令 `{unknown}`");
            eprint!("{USAGE}");
            EXIT_USAGE
        }
    };
    ExitCode::from(code)
}

/// CI 与本地共同委托给同一个命令目录，不在 YAML 复制平台命令。
fn run_ci(rest: &[String]) -> u8 {
    let root = match workspace_root() {
        Ok(root) => root,
        Err(message) => {
            eprintln!("xtask: {message}");
            return EXIT_FAILURE;
        }
    };
    match Command::new("python3")
        .arg(root.join("tools/ci/run.py"))
        .args(rest)
        .current_dir(root)
        .status()
    {
        Ok(status) => status
            .code()
            .and_then(|c| u8::try_from(c).ok())
            .unwrap_or(EXIT_FAILURE),
        Err(error) => {
            eprintln!("xtask ci: {error}");
            EXIT_FAILURE
        }
    }
}

/// `env`：探测工具链与 workspace 信息，只读，不改动任何文件。
fn run_env(rest: &[String]) -> u8 {
    if let Some(code) = reject_arguments("env", rest) {
        return code;
    }
    let root = match workspace_root() {
        Ok(root) => root,
        Err(message) => {
            eprintln!("xtask: {message}");
            return EXIT_FAILURE;
        }
    };

    println!("workspace root: {}", root.display());
    println!("工具链（探测实际安装版本，不写死）：");
    for program in ["rustc", "cargo", "node", "pnpm"] {
        println!("  {program:<5}: {}", probe_version(program, &root));
    }

    println!("workspace 成员（cargo tree --locked --workspace --depth 0）：");
    match workspace_members(&root) {
        Ok(members) => {
            for member in members {
                println!("  {member}");
            }
        }
        Err(message) => println!("  不可用（{message}）"),
    }

    println!("关键路径：");
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "pnpm-lock.yaml",
        "rust-toolchain.toml",
        "node_modules",
    ] {
        let path = root.join(relative);
        let state = if path.exists() { "存在" } else { "缺失" };
        println!("  {relative:<19}: {state}");
    }
    EXIT_OK
}

/// `check`：先确认前端依赖已安装，再逐步执行检查，最后汇总失败步骤。
fn run_check(rest: &[String]) -> u8 {
    if let Some(code) = reject_arguments("check", rest) {
        return code;
    }
    let root = match workspace_root() {
        Ok(root) => root,
        Err(message) => {
            eprintln!("xtask: {message}");
            return EXIT_FAILURE;
        }
    };

    println!("workspace root: {}", root.display());
    if let Err(message) = ensure_frontend_dependencies(&root) {
        eprintln!("xtask: {message}");
        return EXIT_FAILURE;
    }

    // 协议生成检查排在最前：它只读契约与生成文件，失败说明生成代码与 api/openapi.yaml 不同步，
    // 此时继续跑编译没有意义（编译的是旧类型）。CI 因此不需要额外的“重新生成”步骤。
    let total = CHECK_STEPS.len() + 1;
    let mut failures: Vec<String> = Vec::new();
    println!("[1/{total}] boundaries-and-protocol: 目标依赖、两份契约与前端消费者边界");
    match boundaries::verify(&root) {
        Ok(()) => println!("      -> 退出码 0"),
        Err(message) => {
            println!("      -> 失败：{message}");
            failures.push("boundaries-and-protocol（依赖或契约边界失败）".to_string());
        }
    }

    for (index, step) in CHECK_STEPS.iter().enumerate() {
        println!(
            "[{}/{total}] {}: {} {}",
            index + 2,
            step.name,
            step.program,
            step.args.join(" ")
        );
        let status = Command::new(step.program)
            .args(step.args)
            .current_dir(&root)
            .status();
        // 子进程直接继承终端，诊断信息即时可见；这里只补充退出码与失败归属。
        match status {
            Ok(status) if status.success() => println!("      -> 退出码 0"),
            Ok(status) => {
                let code = describe_exit_code(&status);
                println!("      -> 退出码 {code}（失败）");
                failures.push(format!("{}（退出码 {code}）", step.name));
            }
            Err(error) => {
                println!("      -> 无法执行 `{}`：{error}", step.program);
                failures.push(format!(
                    "{}（无法执行 `{}`: {error}）",
                    step.name, step.program
                ));
            }
        }
    }

    println!();
    if failures.is_empty() {
        println!("全部 {total} 步通过");
        EXIT_OK
    } else {
        let failed = failures.len();
        println!("{failed}/{total} 步失败：");
        for failure in &failures {
            println!("  - {failure}");
        }
        EXIT_FAILURE
    }
}

/// `package` 的显式未实现路径：打印说明并以非零退出。
fn run_unimplemented(command: &str, rest: &[String]) -> u8 {
    if let Some(code) = reject_arguments(command, rest) {
        return code;
    }
    eprintln!(
        "xtask: `{command}` 未实现（Unsupported）：产品打包通路尚未落地，\
         本命令不执行任何动作。"
    );
    eprintln!("实现后由本子命令承担设计文档 §5.2 的生成与打包编排；当前进度见 agent/progress.md。");
    EXIT_UNIMPLEMENTED
}

/// 拒绝未文档化的多余参数，避免脚本里写错的参数被静默忽略。
fn reject_arguments(command: &str, rest: &[String]) -> Option<u8> {
    let first = rest.first()?;
    eprintln!("xtask: `{command}` 不接受参数（收到 `{first}`）");
    eprintln!("运行 `cargo xtask help` 查看可用子命令。");
    Some(EXIT_USAGE)
}

/// 由 `cargo locate-project` 定位 workspace 根目录，使子命令可从任意子目录调用。
pub(crate) fn workspace_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| format!("无法读取当前目录：{error}"))?;
    let output = capture(
        "cargo",
        &["locate-project", "--workspace", "--message-format", "plain"],
        &cwd,
    )?;
    if !output.status.success() {
        return Err(format!(
            "`cargo locate-project --workspace` 失败（退出码 {}）：{}",
            describe_exit_code(&output.status),
            stderr_text(&output)
        ));
    }
    let manifest = PathBuf::from(first_line(&output.stdout));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "cargo locate-project 未给出 workspace 清单路径".to_string())
}

/// 用 cargo 内建子命令列出成员，避免在 xtask 内解析 `cargo metadata` 的 JSON。
fn workspace_members(root: &Path) -> Result<Vec<String>, String> {
    let output = capture(
        "cargo",
        &[
            "tree",
            "--locked",
            "--workspace",
            "--depth",
            "0",
            "--prefix",
            "none",
        ],
        root,
    )?;
    if !output.status.success() {
        return Err(format!(
            "`cargo tree --locked --workspace` 退出码 {}：{}",
            describe_exit_code(&output.status),
            stderr_text(&output)
        ));
    }
    let members: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if members.is_empty() {
        return Err("cargo tree 未输出 workspace 成员".to_string());
    }
    Ok(members)
}

/// 前端检查要求依赖已安装：缺失时明确报错，不静默跳过 typecheck。
fn ensure_frontend_dependencies(root: &Path) -> Result<(), String> {
    let node_modules = root.join("node_modules");
    if node_modules.is_dir() {
        return Ok(());
    }
    Err(format!(
        "缺少 {}：前端检查不会跳过，请先在 {} 运行 `pnpm install --frozen-lockfile`",
        node_modules.display(),
        root.display()
    ))
}

/// 执行命令并收集输出；命令无法启动时返回错误说明。
fn capture(program: &str, args: &[&str], dir: &Path) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("无法执行 `{program}`：{error}"))
}

/// 打印 `program --version` 的实际输出；工具缺失时如实标记不可用。
fn probe_version(program: &str, root: &Path) -> String {
    match capture(program, &["--version"], root) {
        Ok(output) if output.status.success() => first_line(&output.stdout),
        Ok(output) => format!(
            "不可用（`{program} --version` 退出码 {}）",
            describe_exit_code(&output.status)
        ),
        Err(message) => format!("不可用（{message}）"),
    }
}

/// 取标准输出的第一行并去除首尾空白。
fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 把子进程 stderr 压成一行，便于拼进错误信息。
fn stderr_text(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(无输出)");
    line.to_string()
}

/// 退出码的可读形式；被信号终止时说明状态。
fn describe_exit_code(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => code.to_string(),
        None => status.to_string(),
    }
}
