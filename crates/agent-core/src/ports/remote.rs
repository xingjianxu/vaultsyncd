//! 远端端口（design §5.5、§15.2、§15.3）。
//!
//! 远端只通过三件事进入核心：**提交一次带 `OpId` 的操作**、**按 `OpId` 查询已提交结果**（响应
//! 丢失后不能换一个新操作重来，I06），以及**取绑定作用域的传输端点**（上传会话、固定版本内容，
//! 由 S054 的 `vaultsyncd_transfer` 端口定义）。核心不拼 URL、不读凭据、不解析错误信封：这些都属于
//! `crates/http-client` 的适配器，错误已按 [`PortError`] 分类。
//!
//! # 不变量
//!
//! - **`pending` 不等于已同步**（I01）：提交结果必须原样带回服务端状态，由执行器决定是否出队；
//!   适配器不得把 202 `pending` 包装成成功。
//! - **重试复用同一个 `OpId`**：`submit` 的请求由调用方构造并保留 `op_id`；`operation` 用同一个
//!   `OpId` 查询，服务端据此返回已记录结果而不是第二次执行（design §15.3）。
//! - **传输端点按绑定取**：上传与内容接口都绑定一个 Root（design §15.2）；端口返回 trait object，
//!   核心的传输执行器直接使用，不关心 HTTP 细节。
//! - **身份只由领域类型表达**：请求里的 `op_id`/`root` 等文本由适配器从领域身份编码；端口参数用
//!   [`OpId`]/[`BindingId`]，避免核心自己拼十六进制。

use std::sync::Arc;

use vaultsyncd_domain::ids::{BindingId, OpId};
use vaultsyncd_protocol::generated::{OperationRequest, OperationResult};
use vaultsyncd_transfer::ports::{PortFuture, UploadTarget, VersionedContent};

/// 远端操作与传输端口。
pub trait RemotePort: Send + Sync {
    /// 提交一次操作；同一 `(root, device, op_id)` 重复提交返回已记录结果（[`OperationResult`]）。
    fn submit(
        &self,
        request: &OperationRequest,
    ) -> PortFuture<'_, Result<OperationResult, vaultsyncd_transfer::ports::PortError>>;

    /// 按 `OpId` 查询已提交操作的当前结论；服务端没有该记录时报 `Rejected`（不猜结果）。
    fn operation(
        &self,
        binding: BindingId,
        op: OpId,
    ) -> PortFuture<'_, Result<OperationResult, vaultsyncd_transfer::ports::PortError>>;

    /// 取该绑定的上传会话端点（`crates/http-client::uploads` 的适配器）。
    fn upload(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn UploadTarget>, vaultsyncd_transfer::ports::PortError>>;

    /// 取该绑定的固定版本内容端点（`crates/http-client::content` 的适配器）。
    fn content(
        &self,
        binding: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn VersionedContent>, vaultsyncd_transfer::ports::PortError>>;
}
