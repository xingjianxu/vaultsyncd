//! 认证与账号用例（design §17.1、C27、§15.2 的 `POST /v1/auth/login`、§15.4 的 401/429 语义）。
//!
//! 分工：
//!
//! - [`password`]：Argon2id 散列与校验，只做密码学，不判业务；
//! - [`account`]：账号端口契约（数数、按登录名读、写一条），适配器在服务端存储层；
//! - [`init`]：首次 owner 初始化用例——一次性、空状态、无默认密码；
//! - [`login`]：凭据校验用例——限流在前、统一失败、不泄露账号是否存在；
//! - [`throttle`]：登录失败限流（进程内、按登录名、按服务端时间）。
//!
//! 边界：本模块不做 I/O（存储经 [`account::OwnerAccounts`] 端口）、不读时钟（时间由调用方给出）、
//! 不发放会话或刷新凭据（S043）、不处理 HTTP（路由在 `apps/server`，错误映射用
//! [`login::LoginError::code`] 的稳定短名）。

#![forbid(unsafe_code)]

pub mod account;
pub mod init;
pub mod login;
pub mod password;
pub mod throttle;
