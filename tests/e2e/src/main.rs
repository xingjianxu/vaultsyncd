//! S074 跨进程场景编排器。
//!
//! 运行环境：Windows 11 客体（真实 CFAPI/WinCred/named pipe） + 宿主 Linux 服务端（真实
//! HTTPS 反向路径、SQLite、btrfs 工作目录）。`tests/platform/windows/run-native-e2e.sh`
//! 负责启动服务端与 SSH 回环隧道、交叉编译并传输 agent 与本程序；本程序只负责：
//!
//! 1. 读取并校验 `tests/scenarios/three-clients.json`；
//! 2. 为每个客户端建立独立 DeviceId/状态目录/同步根/缓存与 agent 进程；
//! 3. 经本机 TLS 入口与 named pipe IPC 驱动真实产品进程，执行 T01/T02/T03/T09/T11 场景；
//! 4. 逐份核对输入内容在“当前/冲突/历史”中的位置，并核对各端收敛；
//! 5. 输出包含提交、服务端/系统版本与拓扑声明的 JSON 报告。
//!
//! 资源现实：仓库只有一台 Windows 客体（第二台按计划属 S107）。三个“客户端”是同一实机上的
//! 三个**隔离身份/环境**（独立 DeviceId、状态目录、client.sqlite、同步根、agent 进程），
//! agent 因产品“每用户会话单实例”不变量而**顺序**运行；报告显式记录这一拓扑，不冒充三台实机。

#![cfg_attr(windows, allow(unsafe_code))]
// 非 Windows 宿主只编译场景校验与代理规则测试；编排器专用代码在该配置下是刻意未用的。
#![cfg_attr(not(windows), allow(dead_code))]

mod proxy;
mod scenario;

#[cfg(windows)]
mod run;

use std::path::PathBuf;

/// 解析后的命令行。
pub struct Options {
    /// 宿主生成的 fixture（服务端地址、Vault、owner 会话、种子哈希）。
    pub fixture: PathBuf,
    /// 数据驱动场景文件。
    pub scenario: PathBuf,
    /// 产品 agent 可执行文件。
    pub agent: PathBuf,
    /// 本程序独占的客体工作目录。
    pub driver: PathBuf,
    /// JSON 报告输出路径。
    pub report: PathBuf,
    /// 只执行指定的 block（可重复；缺省执行全部）。
    pub blocks: Vec<String>,
    /// S102：旧设备故障矩阵，不执行 S074 的场景块。
    pub faults: bool,
}

fn main() {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!(
                "usage: vaultsyncd-e2e --fixture <file> --scenario <file> --agent <exe> --driver <dir> --report <file> [--block <id>]...\n{message}"
            );
            std::process::exit(2);
        }
    };
    // 场景校验在任何平台都一样，避免客体上才暴露数据错误。
    let scenario = match scenario::Scenario::load(&options.scenario) {
        Ok(scenario) => scenario,
        Err(message) => {
            eprintln!("scenario-invalid: {message}");
            std::process::exit(2);
        }
    };
    #[cfg(windows)]
    {
        let code = run::execute(&options, &scenario);
        std::process::exit(code);
    }
    #[cfg(not(windows))]
    {
        let _ = (&options.agent, &options.driver, &options.report);
        eprintln!(
            "vaultsyncd-e2e 只能在 Windows 客体上执行真实客户端进程；场景 {} 已通过校验。宿主侧请使用 `cargo test -p vaultsyncd-e2e`。",
            scenario.id
        );
        std::process::exit(3);
    }
}

fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut fixture = None;
    let mut scenario = None;
    let mut agent = None;
    let mut driver = None;
    let mut report = None;
    let mut blocks = Vec::new();
    let mut faults = false;
    let mut args = args;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("{name} 缺少参数值"))
        };
        match arg.as_str() {
            "--fixture" => fixture = Some(PathBuf::from(value("--fixture")?)),
            "--scenario" => scenario = Some(PathBuf::from(value("--scenario")?)),
            "--agent" => agent = Some(PathBuf::from(value("--agent")?)),
            "--driver" => driver = Some(PathBuf::from(value("--driver")?)),
            "--report" => report = Some(PathBuf::from(value("--report")?)),
            "--block" => blocks.push(value("--block")?),
            "--faults" => faults = true,
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(Options {
        fixture: fixture.ok_or("缺少 --fixture")?,
        scenario: scenario.ok_or("缺少 --scenario")?,
        agent: agent.ok_or("缺少 --agent")?,
        driver: driver.ok_or("缺少 --driver")?,
        report: report.ok_or("缺少 --report")?,
        blocks,
        faults,
    })
}
