//! FFI 生命周期记录：每一步都有 `stage`、参数摘要与 `HRESULT`，失败不吞错。
//!
//! 原型的每次注册/连接/创建/查询/断开/注销都调用 [`record`]，把一行 `stage=... key=value
//! ... hresult=0x........` 写到标准输出。失败路径返回 [`StageError`]，其中保留失败阶段与
//! 原始码，由命令入口输出到标准错误并以非零退出码结束——不允许“打印后当成功”。
//!
//! 本模块不依赖 Windows：记录格式与 HRESULT 判定都是纯逻辑，可在宿主测试。

use std::fmt;

/// Win32 `HRESULT` 的包装；负值表示失败（`SUCCEEDED` 语义）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HResult(i32);

impl HResult {
    /// 成功码 `S_OK`。
    pub const S_OK: Self = Self(0);

    /// 包装原始码（CFAPI 返回值就是 `HRESULT`）。
    pub const fn new(code: i32) -> Self {
        Self(code)
    }

    /// 原始码。
    pub const fn code(self) -> i32 {
        self.0
    }

    /// `SUCCEEDED(hr)`。
    pub const fn succeeded(self) -> bool {
        self.0 >= 0
    }
}

impl fmt::Display for HResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}", self.0)
    }
}

/// 带阶段的 CFAPI 失败：阶段名 + 原始 `HRESULT` + 可读细节。
///
/// 阶段名与码必须原样出现在日志里，排障时才能区分“注册失败”“连接失败”或“某个占位创建失败”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageError {
    stage: &'static str,
    hr: HResult,
    detail: String,
}

impl StageError {
    /// 构造失败记录。
    pub fn new(stage: &'static str, hr: HResult, detail: impl Into<String>) -> Self {
        Self {
            stage,
            hr,
            detail: detail.into(),
        }
    }

    /// 失败发生的阶段名（与日志的 `stage=` 一致）。
    pub const fn stage(&self) -> &'static str {
        self.stage
    }

    /// 原始 `HRESULT`。
    pub const fn hr(&self) -> HResult {
        self.hr
    }

    /// 可读细节。
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for StageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stage={} hresult={} {}",
            self.stage, self.hr, self.detail
        )
    }
}

impl std::error::Error for StageError {}

/// 把一条生命周期记录格式化为单行：`stage=<stage> <key>=<value>... hresult=<hr>`。
///
/// 参数摘要只放路径、名称、大小、身份长度、连接键这类可公开的事实：不放凭据、令牌或
/// 服务器地址（design §14.2 对 `FileIdentity` 的同类要求也适用于日志）。
pub fn format_record(stage: &str, fields: &[(&str, String)], hr: HResult) -> String {
    let mut line = format!("stage={stage}");
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    line.push_str(" hresult=");
    line.push_str(&hr.to_string());
    line
}

/// 打印一条生命周期记录到标准输出。
pub fn record(stage: &str, fields: &[(&str, String)], hr: HResult) {
    println!("{}", format_record(stage, fields, hr));
}

#[cfg(test)]
mod tests {
    use super::{HResult, StageError, format_record};

    #[test]
    fn record_line_lists_stage_fields_and_hr() {
        let line = format_record(
            "create.placeholder",
            &[
                ("path", "vaultsyncd-proto/small-12345.bin".to_string()),
                ("logical_size", "12345".to_string()),
                ("identity_length", "48".to_string()),
            ],
            HResult::S_OK,
        );
        assert_eq!(
            line,
            "stage=create.placeholder path=vaultsyncd-proto/small-12345.bin logical_size=12345 identity_length=48 hresult=0x00000000"
        );
    }

    #[test]
    fn failure_codes_are_printed_as_negative_hr() {
        let line = format_record("register", &[], HResult::new(0x8007_00B7u32 as i32));
        assert_eq!(line, "stage=register hresult=0x800700B7");
    }

    #[test]
    fn succeeded_matches_hresult_sign() {
        assert!(HResult::S_OK.succeeded());
        assert!(HResult::new(0x0000_0001).succeeded());
        assert!(!HResult::new(0x8007_0005u32 as i32).succeeded());
        assert!(!HResult::new(0xC000_00BBu32 as i32).succeeded());
    }

    #[test]
    fn stage_error_keeps_stage_code_and_detail() {
        let error = StageError::new(
            "connect",
            HResult::new(0x8007_0005u32 as i32),
            "CfConnectSyncRoot 失败",
        );
        assert_eq!(error.stage(), "connect");
        assert_eq!(error.hr().code(), 0x8007_0005u32 as i32);
        assert_eq!(
            error.to_string(),
            "stage=connect hresult=0x80070005 CfConnectSyncRoot 失败"
        );
    }
}
