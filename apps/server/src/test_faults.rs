//! 真实持久化动作之间的测试停车点；默认构建编译为空函数，无环境变量/网络入口。

#[cfg(feature = "test-faults")]
static CONTROL: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// 仅测试 helper 调用，目录由其父进程在独占 run_id 内建立。
#[cfg(feature = "test-faults")]
pub fn configure(directory: std::path::PathBuf) {
    assert!(directory.is_absolute() && directory.is_dir());
    CONTROL.set(directory).expect("configure only once");
}

/// 故障父进程观察已刷盘的 marker 后 SIGKILL；也可显式续行以测试并发排队。
#[inline]
pub(crate) fn checkpoint(_point: &str) {
    #[cfg(feature = "test-faults")]
    {
        use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, time::Duration};
        let Some(dir) = CONTROL.get() else { return };
        if !std::fs::read_to_string(dir.join("armed"))
            .ok()
            .is_some_and(|s| s.lines().any(|p| p == _point))
        {
            return;
        }
        let marker = dir.join(format!("{_point}.reached"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(marker)
            .expect("exclusive fault marker");
        writeln!(file, "{} {_point}", std::process::id()).expect("fault marker");
        file.sync_all().expect("durable fault marker");
        std::fs::File::open(dir).unwrap().sync_all().unwrap();
        for _ in 0..6000 {
            if dir.join(format!("{_point}.continue")).exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // 父进程消失也不能把一次故障注入伪装为成功。
        std::process::abort();
    }
}
