//! Console diagnostics are intentional; the double-click cmd keeps failures visible.
#[cfg(windows)]
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        if args.len() != 2 {
            return Err("usage: vaultsyncd-zip start|stop|health".into());
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(vaultsyncd_agent::portable::run(&args[1]))
    })();
    if let Err(error) = result {
        eprintln!("ZIP operation failed: {error}");
        std::process::exit(1);
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("vaultsyncd ZIP requires Windows 11 x64");
    std::process::exit(3);
}
