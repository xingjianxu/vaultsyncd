//! 本机命令处理器的最小骨架（`docs/design.md` §15.5）：“GUI 和 CLI 调用同一命令处理器”。
//!
//! S025 只实现 `get_status`：成功应答给出 agent 版本与它支持的 IPC 协议版本；版本不兼容时返回
//! 协议级错误。更细的同步状态与其余命令在各自功能卡实现，但都走同一个 [`Service::handle`]
//! 入口，界面不另写一套业务判断。
//!
//! 不变量：
//!
//! - 应答与请求用 `request_id` 配对，agent 原样回显，不据此授权。
//! - `kind` 与负载必须一致：`ok` 只带 `result`，`error` 只带 `error`；构造只经 [`Response::ok`]
//!   与 [`Response::error`]，[`Response::validate`] 供解码测试复核。
//! - 错误说明是可直接显示的文本，不含内部绝对路径或凭据。

use core::fmt;

use crate::generated::{
    CommandName, ErrorBody, GetStatusResult, LocalIpcErrorCode, Request, Response, ResponseKind,
};
use crate::version;

/// 命令处理器：持有 agent 身份，把已解码的请求变成应答。
#[derive(Debug, Clone)]
pub struct Service {
    /// agent 二进制版本，由应用入口在启动时传入。
    agent_version: String,
}

impl Service {
    /// 用 agent 二进制版本构造处理器。
    pub fn new(agent_version: impl Into<String>) -> Self {
        Self {
            agent_version: agent_version.into(),
        }
    }

    /// 构造时传入的 agent 版本。
    pub fn agent_version(&self) -> &str {
        &self.agent_version
    }

    /// 处理一条已通过解码的请求。
    pub fn handle(&self, request: &Request) -> Response {
        if !version::is_compatible(&request.ipc_version) {
            let message = format!(
                "本机 IPC 协议版本 {}.{} 不受支持（agent 支持 {}.{}）",
                request.ipc_version.major,
                request.ipc_version.minor,
                version::SUPPORTED_MAJOR,
                version::SUPPORTED_MINOR
            );
            return Response::error(
                request.request_id.clone(),
                ErrorBody {
                    code: LocalIpcErrorCode::UnsupportedVersion,
                    message,
                    retryable: false,
                },
            );
        }
        match request.command.name {
            CommandName::GetStatus => Response::ok(
                request.request_id.clone(),
                GetStatusResult {
                    agent_version: self.agent_version.clone(),
                    ipc_protocol: version::supported(),
                },
            ),
        }
    }
}

impl Response {
    /// 成功应答：`kind = ok`，只带结果。
    pub fn ok(request_id: impl Into<String>, result: GetStatusResult) -> Self {
        Self {
            request_id: request_id.into(),
            kind: ResponseKind::Ok,
            result: Some(result),
            error: None,
        }
    }

    /// 失败应答：`kind = error`，只带错误主体。
    pub fn error(request_id: impl Into<String>, error: ErrorBody) -> Self {
        Self {
            request_id: request_id.into(),
            kind: ResponseKind::Error,
            result: None,
            error: Some(error),
        }
    }

    /// 复核 `kind` 与负载一致；解码自 wire 的应答在信任前应调用。
    pub fn validate(&self) -> Result<(), ResponseShapeError> {
        match self.kind {
            ResponseKind::Ok if self.result.is_some() && self.error.is_none() => Ok(()),
            ResponseKind::Error if self.error.is_some() && self.result.is_none() => Ok(()),
            ResponseKind::Ok | ResponseKind::Error => Err(ResponseShapeError::PayloadMismatch {
                kind: self.kind,
                has_result: self.result.is_some(),
                has_error: self.error.is_some(),
            }),
        }
    }
}

/// 应答的 `kind` 与负载不一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseShapeError {
    /// `kind` 与 `result`/`error` 的存在性不符。
    PayloadMismatch {
        /// 应答声明的种类。
        kind: ResponseKind,
        /// 是否携带结果。
        has_result: bool,
        /// 是否携带错误。
        has_error: bool,
    },
}

impl fmt::Display for ResponseShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadMismatch {
                kind,
                has_result,
                has_error,
            } => write!(
                f,
                "应答种类 {kind:?} 与负载不符（result: {has_result}，error: {has_error}）"
            ),
        }
    }
}

impl core::error::Error for ResponseShapeError {}
