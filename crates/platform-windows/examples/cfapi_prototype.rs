//! `cfapi-prototype`：CFAPI 原型命令（S010 注册/占位/清理，S011 读取/取消/离线）。
//!
//! 用法（Windows 客体上运行；`expect` 与 `--help` 在任意平台可用）：
//!
//! ```text
//! cfapi-prototype create  [--root C:\vaultsyncd-test]
//! cfapi-prototype inspect [--root C:\vaultsyncd-test] [--allow-hydrated]
//! cfapi-prototype cleanup [--root C:\vaultsyncd-test]
//! cfapi-prototype read    --name <entry> [--root <path>] [--offset N --length M]
//!                         [--source available|unavailable] [--latency-ms N]
//!                         [--timeout-ms N] [--chunk-delay-ms N]
//!                         [--expect success|failure]
//! cfapi-prototype cancel  --name <entry> [--root <path>] [--cancel-after-ms N]
//!                         [--chunk-delay-ms N]
//! cfapi-prototype hydrate --name <entry> [--root <path>] [--offset N --length M]
//!                         [--cancel-after-ms N] [--chunk-delay-ms N]
//!                         [--require-cancel-callback]
//! cfapi-prototype expect  [--name <entry> --offset N --length M]
//!                                   # 打印固定内容版本的条目/长度/整份 SHA-256；
//!                                   # 给 --name 与范围时打印该区间的 SHA-256
//! ```
//!
//! 所有子命令只作用于 `--root` 指定的专用 NTFS 目录（默认 `C:\vaultsyncd-test`）。`create`
//! 注册同步根并创建固定占位集合（目录 + 12,345 B + 1 MiB 两个文件），`inspect` 只读核验
//! 占位状态、逻辑大小、本地数据量与 `FileIdentity`，`cleanup` 只删除本工具创建的占位符、
//! 保留用户内容并注销同步根；`read`/`cancel` 在本进程内提供内容供给回调并做读取验证。
//!
//! 退出码：0 成功；1 失败（含阶段与 HRESULT）；2 用法错误；3 `inspect` 发现同步根未注册
//! （cleanup 之后的预期状态，用它与真正的失败区分开）。
//!
//! 构建与客体执行步骤见 `docs/operations/windows-lab.md` §16、§17。

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
    Read,
    Cancel,
    Hydrate,
    Expect,
}

impl Command {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "create" => Some(Self::Create),
            "inspect" => Some(Self::Inspect),
            "cleanup" => Some(Self::Cleanup),
            "read" => Some(Self::Read),
            "cancel" => Some(Self::Cancel),
            "hydrate" => Some(Self::Hydrate),
            "expect" => Some(Self::Expect),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Inspect => "inspect",
            Self::Cleanup => "cleanup",
            Self::Read => "read",
            Self::Cancel => "cancel",
            Self::Hydrate => "hydrate",
            Self::Expect => "expect",
        }
    }
}

/// 解析后的调用参数。
#[derive(Clone, Debug)]
struct Cli {
    command: Command,
    root: PathBuf,
    name: Option<String>,
    offset: Option<u64>,
    length: Option<u64>,
    source: Option<String>,
    latency_ms: Option<u64>,
    timeout_ms: Option<u64>,
    chunk_delay_ms: Option<u64>,
    expect: Option<String>,
    cancel_after_ms: Option<u64>,
    disconnect_after_ms: Option<u64>,
    allow_hydrated: bool,
    require_cancel_callback: bool,
}

fn parse_u64(flag: &str, value: Option<&String>) -> Result<u64, String> {
    let text = value.ok_or_else(|| format!("{flag} 需要一个数值参数"))?;
    text.parse::<u64>()
        .map_err(|_| format!("{flag} 需要一个非负整数，收到 {text}"))
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut iter = args.iter();
    let command = match iter.next().map(String::as_str) {
        Some(text) => Command::parse(text).ok_or_else(|| {
            format!("未知子命令 {text}（应为 create/inspect/cleanup/read/cancel/hydrate/expect）")
        })?,
        None => {
            return Err("缺少子命令（create/inspect/cleanup/read/cancel/expect）".to_string());
        }
    };
    let mut cli = Cli {
        command,
        root: PathBuf::from(DEFAULT_ROOT),
        name: None,
        offset: None,
        length: None,
        source: None,
        latency_ms: None,
        timeout_ms: None,
        chunk_delay_ms: None,
        expect: None,
        cancel_after_ms: None,
        disconnect_after_ms: None,
        allow_hydrated: false,
        require_cancel_callback: false,
    };
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--root" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--root 需要一个路径参数".to_string())?;
                cli.root = PathBuf::from(value);
            }
            "--name" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--name 需要一个条目名".to_string())?;
                cli.name = Some(value.clone());
            }
            "--offset" => cli.offset = Some(parse_u64("--offset", iter.next())?),
            "--length" => cli.length = Some(parse_u64("--length", iter.next())?),
            "--latency-ms" => cli.latency_ms = Some(parse_u64("--latency-ms", iter.next())?),
            "--timeout-ms" => cli.timeout_ms = Some(parse_u64("--timeout-ms", iter.next())?),
            "--chunk-delay-ms" => {
                cli.chunk_delay_ms = Some(parse_u64("--chunk-delay-ms", iter.next())?)
            }
            "--cancel-after-ms" => {
                cli.cancel_after_ms = Some(parse_u64("--cancel-after-ms", iter.next())?)
            }
            "--disconnect-after-ms" => {
                cli.disconnect_after_ms = Some(parse_u64("--disconnect-after-ms", iter.next())?)
            }
            "--source" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--source 需要 available 或 unavailable".to_string())?;
                cli.source = Some(value.clone());
            }
            "--expect" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--expect 需要 success 或 failure".to_string())?;
                cli.expect = Some(value.clone());
            }
            "--allow-hydrated" => cli.allow_hydrated = true,
            "--require-cancel-callback" => cli.require_cancel_callback = true,
            other => return Err(format!("未知参数 {other}")),
        }
    }
    Ok(cli)
}

/// `--name` 必填的子命令。
fn require_name(cli: &Cli) -> Result<&str, String> {
    cli.name.as_deref().ok_or_else(|| {
        format!(
            "{} 需要 --name <entry>（原型条目见 expect 输出）",
            cli.command.name()
        )
    })
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
    println!("command={} root={}", cli.command.name(), cli.root.display());

    match cli.command {
        Command::Expect => expect(&cli),
        Command::Read | Command::Cancel | Command::Hydrate => match require_name(&cli) {
            Ok(_) => run_platform(cli),
            Err(message) => {
                eprintln!("cfapi-prototype: {message}");
                ExitCode::from(2)
            }
        },
        _ => run_platform(cli),
    }
}

/// 打印固定内容版本：条目名、长度与整份内容的 SHA-256。
///
/// 宿主与客体都能运行（不依赖 Windows）：宿主侧用它与客体 `Get-FileHash` 的结果比对，
/// 两端哈希一致才算“读到的字节就是声明的固定版本内容”。带 `--name` 与范围时只打印该区间
/// 的 SHA-256，作为范围读取的宿主侧期望值。
fn expect(cli: &Cli) -> ExitCode {
    use vaultsyncd_platform_windows::cfapi::content;

    println!("content_version seed=0x{:016X}", content::CONTENT_SEED);
    if let (Some(name), Some(offset), Some(length)) = (cli.name.as_deref(), cli.offset, cli.length)
    {
        let Some(size) = content::entry_size(name) else {
            eprintln!("cfapi-prototype: 未知条目 {name}");
            return ExitCode::from(2);
        };
        let Ok((offset, length)) = content::check_range(
            i64::try_from(offset).unwrap_or(i64::MAX),
            i64::try_from(length).unwrap_or(i64::MAX),
            size,
        ) else {
            eprintln!("cfapi-prototype: 区间非法（size={size}）");
            return ExitCode::from(2);
        };
        let mut bytes = vec![0u8; usize::try_from(length).unwrap_or(0)];
        if let Err(error) = content::fill_range(name, offset, &mut bytes) {
            eprintln!("cfapi-prototype: 生成区间内容失败：{error}");
            return ExitCode::FAILURE;
        }
        println!(
            "expect_range name={name} offset={offset} length={length} sha256={}",
            content::sha256_hex(&bytes)
        );
        return ExitCode::SUCCESS;
    }
    for (name, size) in content::entries() {
        match content::expected_digest(name) {
            Some(digest) => println!("expect_entry name={name} size={size} sha256={digest}"),
            None => {
                eprintln!("cfapi-prototype: 无法计算 {name} 的期望哈希");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(windows)]
fn run_platform(cli: Cli) -> ExitCode {
    let outcome = run_windows(cli);
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

#[cfg(not(windows))]
fn run_platform(cli: Cli) -> ExitCode {
    // 非 Windows 构建只用于确认宿主侧可编译；真正的验收必须到客体执行。
    eprintln!(
        "cfapi-prototype: 目标平台不是 Windows（root={}）；请用 `nix develop .#windows` 交叉构建后在客体运行，见 docs/operations/windows-lab.md §16",
        cli.root.display()
    );
    ExitCode::FAILURE
}

#[cfg(windows)]
fn run_windows(cli: Cli) -> Result<ExitCode, vaultsyncd_platform_windows::cfapi::StageError> {
    use vaultsyncd_platform_windows::cfapi::prototype::{
        Inspection, cleanup, create, inspect_with,
    };
    use vaultsyncd_platform_windows::cfapi::provider::{
        ReadExpectation, ReadMode, run_cancel, run_hydrate, run_read,
    };

    let config = provider_config(&cli).map_err(|message| {
        vaultsyncd_platform_windows::cfapi::StageError::new(
            "args",
            vaultsyncd_platform_windows::cfapi::HResult::new(0x8007_0057u32 as i32),
            message,
        )
    })?;
    match cli.command {
        Command::Create => create(&cli.root).map(|()| ExitCode::SUCCESS),
        Command::Cleanup => cleanup(&cli.root).map(|()| ExitCode::SUCCESS),
        Command::Inspect => {
            inspect_with(&cli.root, cli.allow_hydrated).map(|inspection| match inspection {
                Inspection::Registered => ExitCode::SUCCESS,
                Inspection::NotRegistered(hr) => {
                    println!("inspect: sync root 未注册（hresult={hr}），符合 cleanup 之后的预期");
                    ExitCode::from(EXIT_NOT_REGISTERED)
                }
            })
        }
        Command::Read => {
            let name = cli.name.as_deref().unwrap_or_default().to_string();
            let mode = match (cli.offset, cli.length) {
                (None, None) => ReadMode::Full,
                (Some(offset), Some(length)) => ReadMode::Range { offset, length },
                _ => {
                    return Err(vaultsyncd_platform_windows::cfapi::StageError::new(
                        "args",
                        vaultsyncd_platform_windows::cfapi::HResult::new(0x8007_0057u32 as i32),
                        "--offset 与 --length 必须同时给出".to_string(),
                    ));
                }
            };
            let expectation = match cli.expect.as_deref() {
                None | Some("success") => ReadExpectation::Success,
                Some("failure") => ReadExpectation::Failure,
                Some(other) => {
                    return Err(vaultsyncd_platform_windows::cfapi::StageError::new(
                        "args",
                        vaultsyncd_platform_windows::cfapi::HResult::new(0x8007_0057u32 as i32),
                        format!("--expect 只接受 success 或 failure，收到 {other}"),
                    ));
                }
            };
            run_read(&cli.root, &name, mode, config, expectation).map(|()| ExitCode::SUCCESS)
        }
        Command::Cancel => {
            let name = cli.name.as_deref().unwrap_or_default().to_string();
            let cancel_after_ms = cli.cancel_after_ms.unwrap_or(150);
            run_cancel(
                &cli.root,
                &name,
                config,
                cancel_after_ms,
                cli.require_cancel_callback,
            )
            .map(|()| ExitCode::SUCCESS)
        }
        Command::Hydrate => {
            let name = cli.name.as_deref().unwrap_or_default().to_string();
            let mode = match (cli.offset, cli.length) {
                (None, None) => ReadMode::Full,
                (Some(offset), Some(length)) => ReadMode::Range { offset, length },
                _ => {
                    return Err(vaultsyncd_platform_windows::cfapi::StageError::new(
                        "args",
                        vaultsyncd_platform_windows::cfapi::HResult::new(0x8007_0057u32 as i32),
                        "--offset 与 --length 必须同时给出".to_string(),
                    ));
                }
            };
            run_hydrate(
                &cli.root,
                &name,
                mode,
                config,
                cli.cancel_after_ms,
                cli.disconnect_after_ms,
            )
            .map(|_| ExitCode::SUCCESS)
        }
        Command::Expect => Ok(ExitCode::SUCCESS),
    }
}

/// 把命令行参数合成为内容源配置；非法取值直接报错。
#[cfg(windows)]
fn provider_config(
    cli: &Cli,
) -> Result<vaultsyncd_platform_windows::cfapi::provider::ProviderConfig, String> {
    use vaultsyncd_platform_windows::cfapi::provider::{ProviderConfig, SourceAvailability};

    let source = match cli.source.as_deref() {
        None | Some("available") => SourceAvailability::Available,
        Some("unavailable") => SourceAvailability::Unavailable,
        Some(other) => {
            return Err(format!(
                "--source 只接受 available 或 unavailable，收到 {other}"
            ));
        }
    };
    let mut config = ProviderConfig {
        source,
        ..ProviderConfig::default()
    };
    if let Some(value) = cli.latency_ms {
        config.latency_ms = value;
    }
    if let Some(value) = cli.timeout_ms {
        config.timeout_ms = value;
    }
    if let Some(value) = cli.chunk_delay_ms {
        config.chunk_delay_ms = value;
    }
    Ok(config)
}
