//! domain 类型与 wire 文本之间的显式转换（`docs/design.md` §5.12）。
//!
//! 生成代码只描述 wire 形态（字符串、整数、闭集枚举），不认识
//! [`ServerEpoch`]、[`ServerId`] 这些受约束的 domain 类型；两侧之间的每一步转换都写在这里，
//! 方向明确、错误明确，不做 `From`/`Into` 隐式互转，避免把“格式不对”的 wire 文本悄悄送进
//! 领域逻辑。
//!
//! 当前契约里需要转换的 domain 值只有服务器身份与 epoch；新增带 domain 语义的字段时在这里补
//! 一对 `*_to_wire` / `*_from_wire`，并在 `crates/protocol/tests/contract.rs` 补往返用例。

use vaultsyncd_domain::ids::{IdParseError, ServerId};
use vaultsyncd_domain::version::{BoundError, ServerEpoch};

/// domain epoch → wire 十进制文本。
pub fn server_epoch_to_wire(epoch: ServerEpoch) -> String {
    epoch.to_decimal()
}

/// wire 十进制文本 → domain epoch；非规范文本或超出 64 位有符号范围时报错。
pub fn server_epoch_from_wire(text: &str) -> Result<ServerEpoch, BoundError> {
    ServerEpoch::from_decimal(text)
}

/// domain 服务器身份 → wire 文本（32 个小写十六进制字符）。
pub fn server_id_to_wire(id: ServerId) -> String {
    id.to_hex()
}

/// wire 文本 → domain 服务器身份；长度或字符不符时报错。
pub fn server_id_from_wire(text: &str) -> Result<ServerId, IdParseError> {
    ServerId::from_hex(text)
}
