//! S037 崩溃注入子进程：跑一次提交，在指定持久化边界挂起，等父进程强杀（T23）。
//!
//! 用法：`crash-child <装置目录> <边界短名>`。子进程只驱动提交；父进程负责准备装置、等到边界后
//! `SIGKILL`，再用同一份磁盘证据执行启动恢复并断言结果。

use std::process::ExitCode;

use vaultsyncd_server_tests::{Boundary, StoreHandle, run_commit};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(dir), Some(boundary)) = (args.next(), args.next()) else {
        eprintln!("用法：crash-child <装置目录> <边界短名>");
        return ExitCode::from(2);
    };
    let boundary = match Boundary::parse(&boundary) {
        Ok(boundary) => boundary,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let handle = match StoreHandle::open(&dir, Some(boundary)) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("打不开装置目录 {dir:?}：{error}");
            return ExitCode::from(1);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("无法建立 runtime：{error}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(run_commit(&handle)) {
        Ok(committed) => {
            println!(
                "committed version {} revision {}",
                committed.record.version, committed.record.revision
            );
            ExitCode::SUCCESS
        }
        Err(code) => {
            // 提交失败是合法结果（不是测试失败）：父进程按 code 断言“没有假成功”。
            println!("failed {code}");
            ExitCode::SUCCESS
        }
    }
}
