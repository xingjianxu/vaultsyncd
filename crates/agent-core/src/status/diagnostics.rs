//! 诊断输出只接受类型化白名单，不接受token、路径、URL、内容或自由detail。
use vaultsyncd_domain::{
    error::ErrorCode,
    ids::{BindingId, OpId},
};
/// 安全诊断记录。
pub struct Diagnostic {
    /// 操作身份。
    pub op: Option<OpId>,
    /// 绑定身份。
    pub binding: Option<BindingId>,
    /// 稳定错误码。
    pub code: ErrorCode,
    /// 耗时毫秒。
    pub duration_ms: u64,
}
/// 仅追加整条UTF-8记录；上限不足时停止，永不截断多字节字符。
pub fn bounded(records: &[Diagnostic], max_bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        let line = format!(
            "op={} binding={} error={} duration_ms={}\n",
            r.op.map(|x| x.to_string()).unwrap_or_default(),
            r.binding.map(|x| x.to_string()).unwrap_or_default(),
            r.code,
            r.duration_ms
        );
        if line.len() > max_bytes.saturating_sub(out.len()) {
            break;
        }
        out.extend_from_slice(line.as_bytes());
    }
    out
}
