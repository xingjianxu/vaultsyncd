//! 上传端点：`POST/GET /v1/vaults/{vault}/uploads…`（design §13.1—13.2、§15.2）。
//!
//! 本模块只做“HTTP → 用例 → 存储/暂存”的翻译：会话边界、块校验顺序与同索引重传语义都在
//! [`vaultsyncd_server_core::api::uploads`] 里；这里负责
//!
//! - 授权：先 [`crate::auth::principal_from`] 认出调用者，再 [`crate::auth::authorize_vault`]
//!   确认 Vault 属于它（I15：每个资源请求都做，不泄露 Vault 是否存在）；
//! - 跨会话：上传会话与签发它的登录会话绑定，别的登录会话即使同一个 owner 也拿不到（404，
//!   与“不存在”同一个回答）；
//! - 载荷上限：块请求体最多读到策略允许的最大块大小 + 1 字节，超出立即拒绝，不把任意大的载荷
//!   读进内存；
//! - 持久化：块字节先由 [`FsStaging`] 写入内部暂存目录并 `fsync` 文件与目录，再登记块行
//!   （核心规定的顺序）；暂存目录是 `state_dir/uploads/<upload_id>/`，不落在 Vault 工作目录里。
//!
//! 失败映射（design §15.4）：凭据问题 401 `AUTH_EXPIRED`、越权 403 `ACCESS_DENIED`、会话不存在
//! 404 `UNSUPPORTED_ENTRY`（“请求的对象不在你的世界里”，不区分是否存在）、形状/越界/超载荷
//! 400 `INVALID_REQUEST`、同索引不同字节 409 `PART_CONFLICT`、摘要不符 422 `HASH_MISMATCH`、
//! 存储失败 503 `NEEDS_RECOVERY`、满盘 507 `INSUFFICIENT_STORAGE`。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};
use vaultsyncd_domain::ids::{
    DeviceId, EntryId, OpId, SessionId, SnapshotId, UploadId, UserId, VaultId, VersionId,
};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_protocol::generated::{
    ErrorCode, MissingRange, ReceivedPart, UploadPartAck, UploadRequest, UploadSession,
    UploadState, UploadStatus,
};
use vaultsyncd_server_core::api::uploads::{
    self, PartWrite, UploadFailure, UploadPhase, UploadPolicy, UploadSession as CoreSession,
};
use vaultsyncd_server_core::objects::ContentDigest;
use vaultsyncd_server_core::ports::upload_session::{PartStaging, UploadSessionStore};
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::uploads as store_uploads;
use vaultsyncd_server_store::versions::ObjectHash;

use crate::auth::{self, AuthContext, AuthorizeError, Principal};

/// 块摘要请求头（design §15.2「二进制块、块哈希」：哈希与字节分开传，先验后存）。
pub const PART_HASH_HEADER: &str = "x-part-sha256";

/// 上传端点共享的进程内状态。
pub struct UploadsContext {
    auth: Arc<AuthContext>,
    staging: PathBuf,
    policy: UploadPolicy,
}

impl UploadsContext {
    /// 以认证上下文与状态目录装配；暂存目录是 `state_dir/uploads`。
    #[must_use]
    pub fn new(auth: Arc<AuthContext>, state_dir: &Path) -> Self {
        Self {
            auth,
            staging: state_dir.join("uploads"),
            policy: UploadPolicy::standard(),
        }
    }

    /// 当前的上传边界策略（总大小、块大小上限）。
    #[must_use]
    pub fn policy(&self) -> UploadPolicy {
        self.policy
    }
}

/// 请求的认证与授权事实（上传与内容端点共用）。
pub(crate) struct Caller {
    pub(crate) principal: Principal,
    pub(crate) request_id: String,
    pub(crate) now: UtcTimestamp,
}

impl Caller {
    // 失败类型是 axum 的 Response（处理器直接把它当返回值用）：与 `auth::principal_from` 同一约定。
    #[allow(clippy::result_large_err)]
    pub(crate) async fn resolve(
        auth_context: &AuthContext,
        headers: &HeaderMap,
        vault: &str,
    ) -> Result<(Self, VaultId), Response> {
        let principal = auth::principal_from(auth_context, headers).await?;
        let vault_id = parse_identity::<VaultId>(vault)
            .ok_or_else(|| bad_request(&next_request_id(), "Vault 身份不是 32 位小写十六进制"))?;
        match auth::authorize_vault(auth_context.database(), &principal, vault_id).await {
            Ok(_) => {}
            Err(AuthorizeError::Scope(_)) => {
                return Err(denied(&next_request_id(), "Vault 不在授权范围"));
            }
            Err(AuthorizeError::Storage(detail)) => {
                warn!(error = %detail, "读取 Vault 归属失败");
                return Err(internal_error(
                    &next_request_id(),
                    "服务端内部错误",
                    Some(vault_id.to_hex()),
                ));
            }
        }
        Ok((
            Self {
                principal,
                request_id: next_request_id(),
                now: server_now(),
            },
            vault_id,
        ))
    }
}

/// `POST /v1/vaults/{vault}/uploads`：创建上传会话。
pub async fn begin(
    State(context): State<Arc<UploadsContext>>,
    UrlPath((vault,)): UrlPath<(String,)>,
    headers: HeaderMap,
    Json(body): Json<UploadRequest>,
) -> Response {
    let _objects = context.auth.database().object_activity().await;
    let (caller, vault_id) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Some(total_size) = parse_decimal(&body.total_size) else {
        return bad_request(&caller.request_id, "总大小不是规范十进制文本");
    };
    let Some(chunk_size) = parse_decimal(&body.chunk_size) else {
        return bad_request(&caller.request_id, "块大小不是规范十进制文本");
    };
    let Ok(hash) = ContentDigest::from_hex(&body.hash) else {
        return bad_request(&caller.request_id, "总哈希必须是 64 位小写十六进制");
    };
    let request = uploads::BeginRequest {
        total_size,
        hash,
        chunk_size,
    };
    let plan = match uploads::plan_begin(context.policy, &request) {
        Ok(plan) => plan,
        Err(failure) => return failure_response(&caller.request_id, &failure, Some(vault_id)),
    };

    let new = store_uploads::NewUpload {
        id: UploadId::from_bytes(random_bytes()),
        vault: vault_id,
        session: caller.principal.session_id,
        expected_size: total_size,
        hash: *hash.as_bytes(),
        chunk_size: plan.chunk_size,
        // 会话有效期 24 小时：过期会话由后续清理卡按 `expires_at` 处理，本卡不做自动清理。
        expires_at: UtcTimestamp::from_unix_seconds(
            caller.now.as_unix_seconds().saturating_add(24 * 60 * 60),
        )
        .unwrap_or(caller.now),
    };
    let mut tx = match context.auth.database().begin_write().await {
        Ok(tx) => tx,
        Err(error) => {
            warn!(error = %error, "创建上传会话时打不开写事务");
            return internal_error(
                &caller.request_id,
                "服务端内部错误",
                Some(vault_id.to_hex()),
            );
        }
    };
    match store_uploads::begin_upload(&mut tx, caller.principal.user_id, &new).await {
        Ok(record) => match tx.commit().await {
            Ok(()) => {
                info!(
                    request_id = %caller.request_id,
                    upload_id = %record.id,
                    vault_id = %vault_id,
                    "已创建上传会话"
                );
                let session = to_wire_session(&record, plan.chunk_size);
                (StatusCode::CREATED, Json(session)).into_response()
            }
            Err(error) => {
                warn!(error = %error, "提交上传会话失败");
                internal_error(
                    &caller.request_id,
                    "服务端内部错误",
                    Some(vault_id.to_hex()),
                )
            }
        },
        Err(error) => {
            let failure = store_failure(error);
            failure_response(&caller.request_id, &failure, Some(vault_id))
        }
    }
}

/// `GET /v1/vaults/{vault}/uploads/{upload}`：会话状态与服务端真实已收块。
pub async fn status(
    State(context): State<Arc<UploadsContext>>,
    UrlPath((vault, upload)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let _objects = context.auth.database().object_activity().await;
    let (caller, vault_id) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Some(upload_id) = parse_identity::<UploadId>(&upload) else {
        return bad_request(&caller.request_id, "上传会话身份不是 32 位小写十六进制");
    };
    let mut conn = match context.auth.database().pool().acquire().await {
        Ok(conn) => conn,
        Err(error) => {
            warn!(error = %error, "读取上传进度时取连接失败");
            return internal_error(
                &caller.request_id,
                "服务端内部错误",
                Some(vault_id.to_hex()),
            );
        }
    };
    let record = match store_uploads::upload_in_scope(
        &mut conn,
        caller.principal.user_id,
        upload_id,
    )
    .await
    {
        Ok(Some(record)) if record.vault == vault_id => record,
        Ok(_) => return not_found(&caller.request_id, vault_id),
        Err(error) => {
            let failure = store_failure(error);
            return failure_response(&caller.request_id, &failure, Some(vault_id));
        }
    };
    // 跨会话：别的登录会话（即使同一 owner）也不能读这次上传的进度。
    if record.session != caller.principal.session_id || record.expires_at <= caller.now {
        return not_found(&caller.request_id, vault_id);
    }
    let progress = match store_uploads::upload_progress(
        &mut conn,
        caller.principal.user_id,
        upload_id,
    )
    .await
    {
        Ok(progress) => progress,
        Err(error) => {
            let failure = store_failure(error);
            return failure_response(&caller.request_id, &failure, Some(vault_id));
        }
    };
    let session = to_wire_session(&record, record.chunk_size);
    let received = progress
        .received
        .iter()
        .map(|part| ReceivedPart {
            index: part.index.to_string(),
            length: part.length.to_string(),
            hash: ContentDigest::from_bytes(part.hash).to_hex(),
        })
        .collect();
    let missing = progress
        .missing
        .iter()
        .map(|range| MissingRange {
            first_index: range.first_index.to_string(),
            count: range.count.to_string(),
        })
        .collect();
    (
        StatusCode::OK,
        Json(UploadStatus {
            upload: session,
            received,
            missing,
        }),
    )
        .into_response()
}

/// `PUT /v1/vaults/{vault}/uploads/{upload}/parts/{index}`：接收一个固定块。
pub async fn put_part(
    State(context): State<Arc<UploadsContext>>,
    UrlPath((vault, upload, index)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let _objects = context.auth.database().object_activity().await;
    let (caller, vault_id) = match Caller::resolve(&context.auth, &headers, &vault).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Some(upload_id) = parse_identity::<UploadId>(&upload) else {
        return bad_request(&caller.request_id, "上传会话身份不是 32 位小写十六进制");
    };
    let Some(index) = parse_decimal(&index) else {
        return bad_request(&caller.request_id, "块索引不是规范十进制文本");
    };
    let Some(declared) = headers
        .get(PART_HASH_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| ContentDigest::from_hex(text).ok())
    else {
        return bad_request(
            &caller.request_id,
            "缺少合法的 x-part-sha256 头（64 位小写十六进制）",
        );
    };
    let limit = context.policy.max_chunk_size.saturating_add(1);
    if body.len() as u64 > limit {
        return bad_request(&caller.request_id, "块载荷超过服务端允许的最大块大小");
    }

    let mut store = DbUploads {
        database: context.auth.database(),
        owner: caller.principal.user_id,
        session: caller.principal.session_id,
        vault: vault_id,
        now: caller.now,
    };
    let mut staging = FsStaging {
        vault: context.staging.clone(),
    };
    match uploads::receive_part(&mut store, &mut staging, upload_id, index, declared, &body).await {
        Ok(receipt) => {
            info!(
                request_id = %caller.request_id,
                upload_id = %upload_id,
                index = receipt.index,
                already_present = matches!(receipt.write, PartWrite::AlreadyPresent),
                "已登记上传块"
            );
            (
                StatusCode::OK,
                Json(UploadPartAck {
                    upload_id: upload_id.to_hex(),
                    index: receipt.index.to_string(),
                    length: receipt.length.to_string(),
                    hash: receipt.hash.to_hex(),
                    state: to_wire_state(receipt.phase),
                    already_present: matches!(receipt.write, PartWrite::AlreadyPresent),
                }),
            )
                .into_response()
        }
        Err(failure) => failure_response(&caller.request_id, &failure, Some(vault_id)),
    }
}

/// 会话与块的存储适配器（端口 [`UploadSessionStore`]）。
pub(crate) struct DbUploads<'a> {
    database: &'a Database,
    owner: UserId,
    session: SessionId,
    vault: VaultId,
    now: UtcTimestamp,
}

impl<'a> DbUploads<'a> {
    /// 以调用方身份装配：所有查询都只在 `(owner, session, vault)` 的交集内成立。
    pub(crate) fn new(
        database: &'a Database,
        owner: UserId,
        session: SessionId,
        vault: VaultId,
        now: UtcTimestamp,
    ) -> Self {
        Self {
            database,
            owner,
            session,
            vault,
            now,
        }
    }
    fn to_core(&self, record: &store_uploads::UploadRecord) -> Result<CoreSession, UploadFailure> {
        if record.expires_at <= self.now {
            return Err(UploadFailure::UnknownUpload);
        }
        if record.vault != self.vault {
            return Err(UploadFailure::UnknownUpload);
        }
        if record.session != self.session {
            return Err(UploadFailure::UnknownUpload);
        }
        Ok(CoreSession {
            upload: record.id,
            vault: record.vault,
            session: record.session,
            total_size: record.expected_size,
            hash: ContentDigest::from_bytes(record.hash),
            chunk_size: record.chunk_size,
            phase: to_core_phase(record.state),
            expires_at: record.expires_at,
        })
    }
}

#[allow(async_fn_in_trait)]
impl UploadSessionStore for DbUploads<'_> {
    async fn load(&mut self, upload: UploadId) -> Result<CoreSession, UploadFailure> {
        let mut conn =
            self.database
                .pool()
                .acquire()
                .await
                .map_err(|error| UploadFailure::Storage {
                    detail: error.to_string(),
                })?;
        let record = store_uploads::upload_in_scope(&mut conn, self.owner, upload)
            .await
            .map_err(store_failure)?
            .ok_or(UploadFailure::UnknownUpload)?;
        self.to_core(&record)
    }

    async fn existing_part(
        &mut self,
        upload: UploadId,
        index: u64,
    ) -> Result<Option<uploads::StoredPart>, UploadFailure> {
        let mut conn =
            self.database
                .pool()
                .acquire()
                .await
                .map_err(|error| UploadFailure::Storage {
                    detail: error.to_string(),
                })?;
        let parts = store_uploads::parts_of(&mut conn, self.owner, upload)
            .await
            .map_err(store_failure)?;
        Ok(parts
            .into_iter()
            .find(|part| part.index == index)
            .map(|part| uploads::StoredPart {
                index: part.index,
                length: part.length,
                hash: ContentDigest::from_bytes(part.hash),
                path: part.path,
            }))
    }

    async fn record_part(
        &mut self,
        upload: UploadId,
        part: &uploads::StoredPart,
    ) -> Result<PartWrite, UploadFailure> {
        let new = store_uploads::NewUploadPart {
            index: part.index,
            length: part.length,
            hash: ObjectHash::from(*part.hash.as_bytes()),
            path: part.path.clone(),
        };
        let mut tx = self
            .database
            .begin_write()
            .await
            .map_err(|error| UploadFailure::Storage {
                detail: error.to_string(),
            })?;
        let outcome = store_uploads::record_part(&mut tx, self.owner, upload, &new)
            .await
            .map_err(store_failure)?;
        tx.commit().await.map_err(|error| UploadFailure::Storage {
            detail: error.to_string(),
        })?;
        Ok(match outcome {
            store_uploads::PartOutcome::Recorded(_) => PartWrite::Recorded,
            store_uploads::PartOutcome::AlreadyPresent(_) => PartWrite::AlreadyPresent,
        })
    }

    async fn parts(&mut self, upload: UploadId) -> Result<Vec<uploads::StoredPart>, UploadFailure> {
        let mut conn =
            self.database
                .pool()
                .acquire()
                .await
                .map_err(|error| UploadFailure::Storage {
                    detail: error.to_string(),
                })?;
        let parts = store_uploads::parts_of(&mut conn, self.owner, upload)
            .await
            .map_err(store_failure)?;
        Ok(parts
            .into_iter()
            .map(|part| uploads::StoredPart {
                index: part.index,
                length: part.length,
                hash: ContentDigest::from_bytes(part.hash),
                path: part.path,
            })
            .collect())
    }

    async fn mark_complete(&mut self, upload: UploadId) -> Result<(), UploadFailure> {
        self.transition(upload, Transition::Complete).await
    }

    async fn mark_finalized(&mut self, upload: UploadId) -> Result<(), UploadFailure> {
        self.transition(upload, Transition::Finalized).await
    }

    async fn abort(&mut self, upload: UploadId) -> Result<(), UploadFailure> {
        self.transition(upload, Transition::Aborted).await
    }
}

/// 会话状态推进的三种目标（`uploads.state` 的转换规则由存储层执行）。
enum Transition {
    Complete,
    Finalized,
    Aborted,
}

impl DbUploads<'_> {
    async fn transition(&mut self, upload: UploadId, to: Transition) -> Result<(), UploadFailure> {
        let mut tx = self
            .database
            .begin_write()
            .await
            .map_err(|error| UploadFailure::Storage {
                detail: error.to_string(),
            })?;
        let outcome = match to {
            Transition::Complete => {
                store_uploads::mark_upload_complete(&mut tx, self.owner, upload).await
            }
            Transition::Finalized => {
                store_uploads::mark_upload_finalized(&mut tx, self.owner, upload).await
            }
            Transition::Aborted => store_uploads::abort_upload(&mut tx, self.owner, upload).await,
        };
        outcome.map_err(store_failure)?;
        tx.commit().await.map_err(|error| UploadFailure::Storage {
            detail: error.to_string(),
        })?;
        Ok(())
    }
}

/// 块暂存适配器（端口 [`PartStaging`]）：写入 `uploads/<upload_id>/<index>` 并持久化。
struct FsStaging {
    vault: PathBuf,
}

#[allow(async_fn_in_trait)]
impl PartStaging for FsStaging {
    async fn store_part(
        &mut self,
        upload: UploadId,
        index: u64,
        bytes: &[u8],
    ) -> Result<String, UploadFailure> {
        let directory = self.vault.join(upload.to_hex());
        let name = format!("{index:020}");
        let path = directory.join(&name);
        let temporary = directory.join(format!(".{name}.{}.tmp", std::process::id()));
        persist_part(&directory, &temporary, &path, bytes).map_err(|error| {
            if error.kind() == io::ErrorKind::StorageFull {
                UploadFailure::Storage {
                    detail: "no_space".to_owned(),
                }
            } else {
                UploadFailure::Storage {
                    detail: error.to_string(),
                }
            }
        })?;
        Ok(format!("uploads/{}/{name}", upload.to_hex()))
    }
}

/// 写临时文件 → `fsync` → 改名 → `fsync` 目录：返回成功即代表这块字节已经持久化。
fn persist_part(
    directory: &Path,
    temporary: &Path,
    final_path: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    std::fs::create_dir_all(directory)?;
    {
        use std::io::Write;
        let mut file = std::fs::File::create(temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(temporary, final_path)?;
    std::fs::File::open(directory)?.sync_all()
}

fn to_core_phase(state: store_uploads::UploadState) -> UploadPhase {
    match state {
        store_uploads::UploadState::Open => UploadPhase::Open,
        store_uploads::UploadState::Complete => UploadPhase::Complete,
        store_uploads::UploadState::Finalized => UploadPhase::Finalized,
        store_uploads::UploadState::Aborted => UploadPhase::Aborted,
    }
}

fn to_wire_state(phase: UploadPhase) -> UploadState {
    match phase {
        UploadPhase::Open => UploadState::Open,
        UploadPhase::Complete => UploadState::Complete,
        UploadPhase::Finalized => UploadState::Finalized,
        UploadPhase::Aborted => UploadState::Aborted,
    }
}

fn to_wire_session(record: &store_uploads::UploadRecord, chunk_size: u64) -> UploadSession {
    let geometry = store_uploads::ChunkGeometry::new(record.expected_size, chunk_size)
        .unwrap_or_else(|_| store_uploads::ChunkGeometry::new(0, 1).expect("块大小 1"));
    UploadSession {
        upload_id: record.id.to_hex(),
        vault_id: record.vault.to_hex(),
        total_size: record.expected_size.to_string(),
        hash: ContentDigest::from_bytes(record.hash).to_hex(),
        chunk_size: geometry.chunk_size().to_string(),
        part_count: geometry.part_count().to_string(),
        state: to_wire_state(to_core_phase(record.state)),
        expires_at: record.expires_at.as_unix_seconds().to_string(),
    }
}

fn store_failure(error: store_uploads::UploadError) -> UploadFailure {
    match error {
        store_uploads::UploadError::OutOfScope => UploadFailure::OutOfScope,
        store_uploads::UploadError::UnknownUpload { .. } => UploadFailure::UnknownUpload,
        store_uploads::UploadError::PartConflict { .. } => UploadFailure::PartConflict { index: 0 },
        store_uploads::UploadError::PartsClosed { state } => UploadFailure::PartsClosed {
            phase: to_core_phase(state),
        },
        other => UploadFailure::Storage {
            detail: other.to_string(),
        },
    }
}

pub(crate) fn failure_response(
    request_id: &str,
    failure: &UploadFailure,
    vault: Option<VaultId>,
) -> Response {
    let vault_id = vault.map(VaultId::to_hex);
    match failure {
        UploadFailure::TotalTooLarge { .. } | UploadFailure::ChunkTooLarge { .. } => {
            error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                request_id,
                &failure.to_string(),
                false,
                vault_id,
            )
        }
        UploadFailure::IndexOutOfRange { .. } | UploadFailure::LengthMismatch { .. } => {
            error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                request_id,
                &failure.to_string(),
                false,
                vault_id,
            )
        }
        UploadFailure::DigestMismatch { .. } => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::HashMismatch,
            request_id,
            &failure.to_string(),
            false,
            vault_id,
        ),
        UploadFailure::PartConflict { .. } => error_response(
            StatusCode::CONFLICT,
            ErrorCode::PartConflict,
            request_id,
            &failure.to_string(),
            false,
            vault_id,
        ),
        UploadFailure::PartsClosed { .. } => error_response(
            StatusCode::CONFLICT,
            ErrorCode::PartConflict,
            request_id,
            &failure.to_string(),
            false,
            vault_id,
        ),
        UploadFailure::UnknownUpload => not_found(
            request_id,
            vault.unwrap_or_else(|| VaultId::from_bytes([0; 16])),
        ),
        UploadFailure::OutOfScope => denied(request_id, "Vault 不在授权范围"),
        UploadFailure::MalformedSession => internal_error(request_id, "上传会话数据损坏", vault_id),
        UploadFailure::Storage { detail } if detail == "no_space" => error_response(
            StatusCode::INSUFFICIENT_STORAGE,
            ErrorCode::InsufficientStorage,
            request_id,
            "服务端暂存空间不足",
            true,
            vault_id,
        ),
        UploadFailure::Storage { detail } => {
            warn!(request_id = %request_id, error = %detail, "上传存储失败");
            internal_error(request_id, "服务端内部错误", vault_id)
        }
    }
}

pub(crate) fn not_found(request_id: &str, vault: VaultId) -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        ErrorCode::UnsupportedEntry,
        request_id,
        "上传会话不存在或不属于当前调用者",
        false,
        Some(vault.to_hex()),
    )
}

pub(crate) fn denied(request_id: &str, message: &str) -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        ErrorCode::AccessDenied,
        request_id,
        message,
        false,
        None,
    )
}

pub(crate) fn bad_request(request_id: &str, message: &str) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        ErrorCode::InvalidRequest,
        request_id,
        message,
        false,
        None,
    )
}

pub(crate) fn internal_error(request_id: &str, message: &str, vault: Option<String>) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::NeedsRecovery,
        request_id,
        message,
        true,
        vault,
    )
}

pub(crate) fn error_response(
    status: StatusCode,
    code: ErrorCode,
    request_id: &str,
    message: &str,
    retryable: bool,
    vault_id: Option<String>,
) -> Response {
    let envelope = vaultsyncd_protocol::generated::ErrorEnvelope {
        error: vaultsyncd_protocol::generated::ErrorBody {
            code,
            details: None,
            message: message.to_owned(),
            request_id: request_id.to_owned(),
            retryable,
            vault_id,
        },
    };
    (status, Json(envelope)).into_response()
}

pub(crate) fn parse_identity<T>(text: &str) -> Option<T>
where
    T: FromIdentity,
{
    T::from_hex_text(text)
}

/// 32 位小写十六进制身份文本的解析（只接受规范形式）。
pub(crate) trait FromIdentity: Sized {
    fn from_hex_text(text: &str) -> Option<Self>;
}

macro_rules! identity_from_hex {
    ($($type:ty),*) => {$(
        impl FromIdentity for $type {
            fn from_hex_text(text: &str) -> Option<Self> {
                if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
                    return None;
                }
                let mut bytes = [0_u8; 16];
                for (slot, chunk) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
                    let high = (chunk[0] as char).to_digit(16)?;
                    let low = (chunk[1] as char).to_digit(16)?;
                    *slot = u8::try_from(high * 16 + low).ok()?;
                }
                Some(<$type>::from_bytes(bytes))
            }
        }
    )*};
}

identity_from_hex!(
    VaultId, UploadId, EntryId, VersionId, OpId, DeviceId, SnapshotId
);

pub(crate) fn parse_decimal(text: &str) -> Option<u64> {
    vaultsyncd_protocol::decimal::validate(text).ok()
}

pub(crate) fn server_now() -> UtcTimestamp {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    UtcTimestamp::from_unix_seconds(i64::try_from(seconds).unwrap_or(i64::MAX))
        .unwrap_or_else(|_| UtcTimestamp::from_unix_seconds(0).expect("0 是合法时间"))
}

fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("upload-{}-{sequence}", std::process::id())
}

pub(crate) fn random_bytes() -> [u8; 16] {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes
}
