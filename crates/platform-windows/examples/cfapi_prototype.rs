//! `cfapi-prototype`：S010 的 CFAPI 原型命令（`create` / `inspect` / `cleanup`）。
//!
//! 用法（在 Windows 客体上运行，产物由宿主交叉编译后传输）：
//!
//! ```text
//! cfapi-prototype create  [--root C:\vaultsyncd-test]
//! cfapi-prototype inspect [--root C:\vaultsyncd-test]
//! cfapi-prototype cleanup [--root C:\vaultsyncd-test]
//! ```
//!
//! 三个子命令都只作用于 `--root` 指定的专用 NTFS 目录：`create` 注册同步根并创建固定条目
//! 集合（目录 + 12,345 B + 1 MiB 两个占位文件），`inspect` 只读核验占位状态、逻辑大小与
//! `FileIdentity`，`cleanup` 只删除本工具创建的占位符、保留用户内容并注销同步根。
//!
//! 退出码：0 成功；1 失败（含阶段与 HRESULT）；2 用法错误；3 `inspect` 发现同步根未注册
//! （cleanup 之后的预期状态，用它与真正的失败区分开）。
//!
//! 构建与客体执行步骤见 `docs/operations/windows-lab.md` §16。

use std::path::PathBuf;
use std::process::ExitCode;

/// `inspect` 发现同步根未注册时的退出码（不代表命令本身失败）。
#[cfg(windows)]
const EXIT_NOT_REGISTERED: u8 = 3;

/// 默认同步根路径：客体本地 NTFS 目录，不用共享文件夹（实施手册 §2.6）。
const DEFAULT_ROOT: &str = r"C:\vaultsyncd-test";

/// 子命令。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Create,
    Inspect,
    Cleanup,
}

/// 解析后的调用参数。
#[derive(Clone, Debug)]
struct Cli {
    command: Command,
    root: PathBuf,
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut iter = args.iter();
    let command = match iter.next().map(String::as_str) {
        Some("create") => Command::Create,
        Some("inspect") => Command::Inspect,
        Some("cleanup") => Command::Cleanup,
        Some(other) => return Err(format!("未知子命令 {other}（应为 create/inspect/cleanup）")),
        None => return Err("缺少子命令（create/inspect/cleanup）".to_string()),
    };
    let mut root = PathBuf::from(DEFAULT_ROOT);
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--root" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--root 需要一个路径参数".to_string())?;
                root = PathBuf::from(value);
            }
            other => return Err(format!("未知参数 {other}（只支持 --root <path>）")),
        }
    }
    Ok(Cli { command, root })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("cfapi-prototype: {message}");
            return ExitCode::from(2);
        }
    };
    println!("command={:?} root={}", cli.command, cli.root.display());

    #[cfg(windows)]
    {
        run(cli)
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 构建只用于确认宿主侧可编译；真正的验收必须到客体执行。
        eprintln!(
            "cfapi-prototype: 目标平台不是 Windows（root={}）；请用 `nix develop .#windows` 交叉构建后在客体运行，见 docs/operations/windows-lab.md §16",
            cli.root.display()
        );
        ExitCode::FAILURE
    }
}

#[cfg(windows)]
fn run(cli: Cli) -> ExitCode {
    use vaultsyncd_platform_windows::cfapi::prototype::{Inspection, cleanup, create, inspect};

    let outcome = match cli.command {
        Command::Create => create(&cli.root).map(|()| ExitCode::SUCCESS),
        Command::Cleanup => cleanup(&cli.root).map(|()| ExitCode::SUCCESS),
        Command::Inspect => inspect(&cli.root).map(|inspection| match inspection {
            Inspection::Registered => ExitCode::SUCCESS,
            Inspection::NotRegistered(hr) => {
                println!("inspect: sync root 未注册（hresult={hr}），符合 cleanup 之后的预期");
                ExitCode::from(EXIT_NOT_REGISTERED)
            }
        }),
    };
    match outcome {
        Ok(code) => {
            println!("cfapi_prototype: OK");
            code
        }
        Err(error) => {
            eprintln!("cfapi_prototype: FAILED {error}");
            ExitCode::FAILURE
        }
    }
}
