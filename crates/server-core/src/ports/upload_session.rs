//! 上传会话的端口契约（design §5.5：端口定义在核心，适配器负责存取）。
//!
//! 核心只通过这两个端口看世界：
//!
//! - [`UploadSessionStore`]：读写上传会话与已收块（服务端存储层实现，`uploads`/`upload_parts`）。
//! - [`PartStaging`]：把一块字节落到私有暂存并**持久化**（平台适配器实现；核心不碰文件系统）。
//!
//! 端口的返回类型是核心自己的事实类型（[`crate::api::uploads`]），不暴露 SQLx 行或平台错误：
//! “会话/块是什么形状”只有一处定义，适配器只做翻译。

use vaultsyncd_domain::ids::UploadId;

use crate::api::uploads::{PartWrite, StoredPart, UploadFailure, UploadSession};

/// 读写上传会话与已收块的端口。
///
/// 不变量（由适配器保证）：所有读写都限定在调用方的 owner scope 内；`record_part` 在同一索引
/// 已有不同字节时返回 [`UploadFailure::PartConflict`] 且不覆盖旧记录；`load` 对“不存在”“不是你的
/// Root”“不是你的上传”统一返回 [`UploadFailure::UnknownUpload`]，不泄露存在性。
#[allow(async_fn_in_trait)]
pub trait UploadSessionStore {
    /// 读一个上传会话（含状态与块几何）。
    async fn load(&mut self, upload: UploadId) -> Result<UploadSession, UploadFailure>;

    /// 读某个索引上已登记的块。
    async fn existing_part(
        &mut self,
        upload: UploadId,
        index: u64,
    ) -> Result<Option<StoredPart>, UploadFailure>;

    /// 登记一个已经持久化的块。
    async fn record_part(
        &mut self,
        upload: UploadId,
        part: &StoredPart,
    ) -> Result<PartWrite, UploadFailure>;
}

/// 把一块字节落到私有暂存的端口。
///
/// 契约：返回前字节必须已经写全并持久化（`fsync` 文件与目录）——否则“先持久化再记完成”的顺序
/// 就没有意义，重启后可能得到一个登记过、内容却丢失的块。失败时不得留下可被登记的路径。
#[allow(async_fn_in_trait)]
pub trait PartStaging {
    /// 写入一块，返回写进存储的暂存位置（相对内部状态目录的规范路径）。
    async fn store_part(
        &mut self,
        upload: UploadId,
        index: u64,
        bytes: &[u8],
    ) -> Result<String, UploadFailure>;
}
