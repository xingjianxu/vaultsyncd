//! S104 专用真实服务进程。只在显式 test-faults 构建中存在。
//! 文件控制桥限定为此进程配置的私有目录，无产品 HTTP/IPC 故障路由。
use std::{io::Write, path::PathBuf};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_server::{bootstrap, config::Config};

#[tokio::main(worker_threads = 4)]
async fn main() {
    let mut args = std::env::args_os().skip(1);
    let config = Config::load(PathBuf::from(args.next().expect("config"))).unwrap();
    let control = PathBuf::from(args.next().expect("private control directory"));
    vaultsyncd_server::test_faults::configure(control.clone());
    let service = bootstrap::prepare(config.clone()).await.unwrap();
    let db = service.fault_database();
    tokio::spawn(async move {
        loop {
            let request = control.join("gc-request");
            if let Ok(raw) = std::fs::read_to_string(&request) {
                std::fs::remove_file(request).unwrap();
                let now = UtcTimestamp::from_unix_seconds(raw.parse().unwrap()).unwrap();
                std::fs::write(
                    control.join("gc-started"),
                    b"waiting for shared object gate",
                )
                .unwrap();
                let result = vaultsyncd_server::gc::run_once(&db, &config.state_dir, now).await;
                let report = match result {
                    Ok(r) => format!(
                        "ok objects={} parts={} temporary={}",
                        r.objects, r.parts, r.temporary_files
                    ),
                    Err(e) => format!("error {e}"),
                };
                std::fs::write(control.join("gc-result"), report).unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
    println!("LISTEN {}", service.local_addr().unwrap());
    std::io::stdout().flush().unwrap();
    service.serve(std::future::pending::<()>()).await.unwrap();
}
