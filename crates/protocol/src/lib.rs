//! 远端 HTTP wire 契约：`api/openapi.yaml` 生成的 DTO，加 domain 类型到 wire 的显式映射。
//!
//! 边界（`docs/design.md` §5.6、§5.12）：
//!
//! - [`generated`] 是**生成代码**，唯一编辑源是 `api/openapi.yaml`，改契约后运行
//!   `cargo xtask generate`。本 crate 不手改生成文件，也不在生成文件里加业务逻辑。
//! - [`decimal`] 是 wire 长整数的 serde 适配：计数与字节数只以规范十进制**字符串**传输
//!   （design §7.1、§15.1），JSON number 会丢精度，因此反序列化直接拒绝。
//! - [`domain_map`] 是 domain 类型（[`vaultsyncd_domain::version::ServerEpoch`]、
//!   [`vaultsyncd_domain::ids::ServerId`]）与 wire 文本之间的显式转换；wire DTO 不依赖 domain
//!   类型，转换点集中在这里，避免把业务不变量散进生成代码。
//!
//! 本 crate 是纯数据层：不做 I/O、不读时钟、不取随机数，Windows 客户端与 Linux 服务端共用同一
//! 份 wire 定义，因此两侧对“未知枚举”“长整数字符串”的判定必然一致。

#![forbid(unsafe_code)]

pub mod decimal;
pub mod domain_map;
pub mod generated;
