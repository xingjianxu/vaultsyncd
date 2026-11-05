//! `vaultsyncd` 服务端进程的库部分：配置、日志、生命周期、HTTP 路由与依赖组装。
//!
//! 二进制入口（`src/main.rs`）只做参数解析与退出码映射，其余都在这里，便于测试直接组装。
//!
//! 平台边界（design §4.2、§17.2）：服务端**只在 Linux 上构建与运行**，不挂载 FUSE 视图、不链接
//! Windows 适配层，也不需要 `/dev/fuse` 或特权容器。非 Linux 目标在编译期就被拒绝，避免出现
//! “能编出来但从没被验证过”的服务端构建。

#![forbid(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!(
    "vaultsyncd 服务端只在 Linux 上构建与运行（docs/design.md §4.2、§17.2）；\
     不要在 Windows/macOS 目标上编译 apps/server"
);

pub mod auth;
pub mod bootstrap;
pub mod config;
pub mod http;
pub mod lifecycle;
pub mod logging;
pub mod observer;
pub mod recovery;
pub mod roots;
pub mod uploads;

/// 退出码：正常停止。
pub const EXIT_OK: u8 = 0;
/// 退出码：运行期失败（数据库、监听或服务循环出错）。
pub const EXIT_FAILURE: u8 = 1;
/// 退出码：用法或配置错误（配置读不出、不合法）。
pub const EXIT_CONFIG: u8 = 2;
