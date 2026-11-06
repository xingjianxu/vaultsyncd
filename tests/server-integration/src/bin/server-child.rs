//! 真实服务端子进程：给“强杀后重启补完”验收用（design §21.2、T23）。
//!
//! 用法：`server-child <配置文件>`。启动后在 stdout 打印一行 `LISTEN <addr>`，然后一直服务到被信号终止；
//! 父进程可以在任意持久化边界 `SIGKILL` 它，再用同一份磁盘证据重启。

use std::io::Write;
use std::process::ExitCode;

use vaultsyncd_server::bootstrap;
use vaultsyncd_server::config::Config;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("用法：server-child <配置文件>");
        return ExitCode::from(2);
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("无法建立 runtime：{error}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(async move {
        let config = match Config::load(std::path::Path::new(&path)) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("配置不可用：{error}");
                return ExitCode::from(2);
            }
        };
        let service = match bootstrap::prepare(config).await {
            Ok(service) => service,
            Err(error) => {
                eprintln!("启动失败：{error}");
                return ExitCode::from(1);
            }
        };
        let addr = match service.local_addr() {
            Ok(addr) => addr,
            Err(error) => {
                eprintln!("取不到监听地址：{error}");
                return ExitCode::from(1);
            }
        };
        println!("LISTEN {addr}");
        let _ = std::io::stdout().flush();
        match service.serve(std::future::pending::<()>()).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("运行期失败：{error}");
                ExitCode::from(1)
            }
        }
    })
}
