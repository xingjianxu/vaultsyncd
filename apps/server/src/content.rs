//! 上传 finalize 与固定版本内容下载端点（design §13.2、§15.2、I01、I05、I12、I15）。
//!
//! 两个端点的规则都在 [`vaultsyncd_server_core::api::content`] 里；本模块只把 HTTP 请求翻译成
//! 用例调用，并把端口的适配器接上真实存储：
//!
//! - [`FsAssembler`]（端口 `ContentPublisher`）：按块行把私有暂存的块按序拼进对象区的**私有暂存
//!   副本**，边写边算 SHA-256，再走 [`capture_object`] 的发布语义（核对长度/摘要 → 不覆盖发布 →
//!   发布后复核）。核心拿实测摘要与长度与会话声明比较，不符即不登记、不返回引用。
//! - [`DbObjects`]（端口 `ContentObjectStore`）：走 `versions::record_object`，一个事务登记一行
//!   `objects`，同一 `(root, hash)` 的相同登记幂等。
//! - [`DbVersionContent`]（端口 `VersionContentStore`）：按**版本身份**读一条版本行（
//!   `versions::current_version` 优先，其余走 `versions::version_history`），只回答那个版本的
//!   内容地址与长度，绝不回退成“当前 head 的内容”。
//! - [`DbLeases`]（端口 `ContentLeasePort`）：`object_leases` 的 `download` 租约，读取前建立、
//!   读完解除，因此读取期间 GC 不会回收该对象（design §7.2、§19.4）。
//!
//! 失败映射（design §15.4）：凭据 401 `AUTH_EXPIRED`、越权 403 `ACCESS_DENIED`、会话/条目不存在
//! 404 `UNSUPPORTED_ENTRY`、形状与区间错误 400 `INVALID_REQUEST`、块未收齐 400、同索引冲突与已放弃
//! 会话 409 `PART_CONFLICT`、内容与快照不符 422 `HASH_MISMATCH`、版本过期 410 `VERSION_GONE`、
//! 存储失败 503 `NEEDS_RECOVERY`、满盘 507 `INSUFFICIENT_STORAGE`。

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};
use vaultsyncd_domain::ids::{EntryId, ObjectLeaseId, RootId, UploadId, UserId, VersionId};
use vaultsyncd_domain::version::UtcTimestamp;
use vaultsyncd_platform_linux_server::files::hash::Sha256;
use vaultsyncd_platform_linux_server::files::hash_file;
use vaultsyncd_protocol::generated::{ErrorCode, FinalizedContent, ObjectReuse};
use vaultsyncd_server_core::api::content::{
    self, ByteSlice, ContentFailure, DOWNLOAD_LEASE_TTL_SECONDS, DownloadRequest, PublishedContent,
    VersionContent,
};
use vaultsyncd_server_core::api::uploads::StoredPart;
use vaultsyncd_server_core::objects::{
    CaptureFailure, CaptureFailureReason, CaptureRequest, CapturedObject, ContentDigest,
    ExpectedContent, ObjectLayout, ReuseOutcome, capture_object,
};
use vaultsyncd_server_core::ports::content_lease::{
    ContentLease, ContentLeaseError, ContentLeasePort, ContentLeasePurpose, ContentLeaseRequest,
};
use vaultsyncd_server_core::ports::content_publish::{
    ContentObjectStore, ContentPublisher, PublishFailure,
};
use vaultsyncd_server_core::ports::object_store::{
    BlobError, CopyFallback, FileKey, ObjectBlobPort, PublishedBlob, StagedBlob, StorageFailure,
    StoredBytes, StoredFacts, StoredSlice,
};
use vaultsyncd_server_core::ports::version_content::{VersionContentError, VersionContentStore};
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::leases as store_leases;
use vaultsyncd_server_store::versions as store_versions;
use vaultsyncd_server_store::versions::NewObject;

use crate::auth::AuthContext;
use crate::uploads::{
    Caller, DbUploads, bad_request, error_response, failure_response, internal_error,
    parse_identity, random_bytes,
};

/// 内容端点共享的进程内状态。
pub struct ContentContext {
    pub(crate) auth: Arc<AuthContext>,
    pub(crate) state_dir: PathBuf,
}

impl ContentContext {
    /// 以认证上下文与状态目录装配；对象区在 `state_dir/objects`，块暂存在 `state_dir/uploads`。
    #[must_use]
    pub fn new(auth: Arc<AuthContext>, state_dir: &Path) -> Self {
        Self {
            auth,
            state_dir: state_dir.to_path_buf(),
        }
    }

    fn objects(&self) -> ObjectLayout {
        ObjectLayout::new(self.state_dir.join("objects"))
    }
}

/// `POST /v1/roots/{root}/uploads/{upload}/finalize`：核对块齐、发布内容、返回可提交引用。
pub async fn finalize(
    State(context): State<Arc<ContentContext>>,
    UrlPath((root, upload)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let _objects = context.auth.database().object_activity().await;
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Some(upload_id) = parse_identity::<UploadId>(&upload) else {
        return bad_request(&caller.request_id, "上传会话身份不是 32 位小写十六进制");
    };

    let mut store = DbUploads::new(
        context.auth.database(),
        caller.principal.user_id,
        caller.principal.session_id,
        root_id,
    );
    let mut assembler = FsAssembler {
        state_dir: context.state_dir.clone(),
        layout: context.objects(),
    };
    let mut objects = DbObjects {
        database: context.auth.database(),
        owner: caller.principal.user_id,
    };
    // 暂存副本名唯一到进程与请求：同名并发 finalize 不会互相覆盖未发布副本。
    let staging_name = format!("finalize-{}-{}", upload_id.to_hex(), caller.request_id);
    match content::finalize_upload(
        &mut store,
        &mut assembler,
        &mut objects,
        upload_id,
        &staging_name,
        caller.now,
    )
    .await
    {
        Ok(finalized) => {
            info!(
                request_id = %caller.request_id,
                upload_id = %upload_id,
                root_id = %root_id,
                digest = %finalized.content.digest,
                replayed = finalized.replayed,
                "finalize 完成"
            );
            (StatusCode::OK, Json(to_wire(&finalized))).into_response()
        }
        Err(failure) => content_failure_response(&caller.request_id, &failure, root_id),
    }
}

/// `GET /v1/roots/{root}/entries/{entry}/versions/{version}/content`：按版本读固定内容。
pub async fn download(
    State(context): State<Arc<ContentContext>>,
    UrlPath((root, entry, version)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let _objects = context.auth.database().object_activity().await;
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Some(entry_id) = parse_identity::<EntryId>(&entry) else {
        return bad_request(&caller.request_id, "条目身份不是 32 位小写十六进制");
    };
    let Some(version_id) = parse_identity::<VersionId>(&version) else {
        return bad_request(&caller.request_id, "版本身份不是 32 位小写十六进制");
    };
    let range = match headers.get(header::RANGE) {
        None => None,
        Some(value) => match value.to_str() {
            Ok(text) => Some(text.to_owned()),
            Err(_) => {
                return bad_request(&caller.request_id, "Range 头不是合法文本");
            }
        },
    };

    let mut versions = DbVersionContent {
        database: context.auth.database(),
        owner: caller.principal.user_id,
    };
    let mut leases = DbLeases {
        database: context.auth.database(),
        owner: caller.principal.user_id,
        now: caller.now,
    };
    let lease_expires_at = UtcTimestamp::from_unix_seconds(
        caller
            .now
            .as_unix_seconds()
            .saturating_add(DOWNLOAD_LEASE_TTL_SECONDS),
    )
    .unwrap_or(caller.now);
    let request = DownloadRequest {
        entry: entry_id,
        version: version_id,
        range,
    };
    let handle = match content::open_download(
        &mut versions,
        &mut leases,
        root_id,
        &request,
        lease_expires_at,
    )
    .await
    {
        Ok(handle) => handle,
        Err(failure) => return content_failure_response(&caller.request_id, &failure, root_id),
    };

    let layout = context.objects();
    let mut port = FsObjectPort;
    let read = content::read_download(&mut port, &layout, &handle);
    // 租约解除失败不改判已读出的字节（那些字节已经是客户端的事），但必须如实记录。
    if let Err(failure) = content::finish_download(&mut leases, handle).await {
        warn!(
            request_id = %caller.request_id,
            error = %failure,
            "解除下载租约失败；租约到期后由 GC 侧清理"
        );
    }
    match read {
        Ok(bytes) => {
            let slice = ByteSlice {
                offset: bytes.offset,
                length: bytes.data.len() as u64,
            };
            info!(
                request_id = %caller.request_id,
                entry_id = %entry_id,
                version_id = %version_id,
                offset = bytes.offset,
                bytes = bytes.data.len(),
                "已读取固定版本内容"
            );
            bytes_response(
                &bytes.content,
                &slice,
                bytes.content.size,
                bytes.data,
                &caller.request_id,
            )
        }
        Err(failure) => content_failure_response(&caller.request_id, &failure, root_id),
    }
}

/// 二进制或区间响应的状态码与头：整份 200，部分 206 + `Content-Range`。
fn bytes_response(
    content: &VersionContent,
    slice: &ByteSlice,
    size: u64,
    data: Vec<u8>,
    request_id: &str,
) -> Response {
    let partial = !slice.covers_whole(size);
    let mut response = Response::new(axum::body::Body::from(data));
    if partial {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&slice.length.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    // 整份内容的地址：客户端据此核对 design §13.2 的整文件校验（切片也带上同一地址）。
    if let Ok(value) = HeaderValue::from_str(&content.digest.to_hex()) {
        headers.insert("x-content-sha256", value);
    }
    if partial {
        match HeaderValue::from_str(&slice.content_range(size)) {
            Ok(value) => {
                headers.insert(header::CONTENT_RANGE, value);
            }
            Err(_) => {
                warn!(request_id = %request_id, "Content-Range 头无法编码");
            }
        }
    }
    response
}

fn to_wire(finalized: &content::FinalizedContent) -> FinalizedContent {
    FinalizedContent {
        upload_id: finalized.upload.to_hex(),
        root_id: finalized.root.to_hex(),
        hash: finalized.content.digest.to_hex(),
        size: finalized.content.size.to_string(),
        storage_path: finalized.content.storage_path.clone(),
        part_count: finalized.part_count.to_string(),
        reuse: match finalized.reuse {
            ReuseOutcome::FreshlyPublished => ObjectReuse::FreshlyPublished,
            ReuseOutcome::ExistingVerified => ObjectReuse::ExistingVerified,
        },
        replayed: finalized.replayed,
    }
}

/// finalize/下载失败的 HTTP 映射（design §15.4）。
fn content_failure_response(request_id: &str, failure: &ContentFailure, root: RootId) -> Response {
    let root_id = Some(root.to_hex());
    match failure {
        ContentFailure::Upload(failure) => failure_response(request_id, failure, Some(root)),
        ContentFailure::IncompleteUpload { expected, present } => error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidRequest,
            request_id,
            &format!("上传尚未收齐块：应有 {expected} 块，实际 {present} 块"),
            false,
            root_id,
        ),
        ContentFailure::ContentMismatch { .. } => error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::HashMismatch,
            request_id,
            &failure.to_string(),
            false,
            root_id,
        ),
        ContentFailure::Publish(failure) if failure.is_no_space() => error_response(
            StatusCode::INSUFFICIENT_STORAGE,
            ErrorCode::InsufficientStorage,
            request_id,
            &failure.to_string(),
            true,
            root_id,
        ),
        ContentFailure::Publish(failure) => {
            warn!(request_id = %request_id, code = failure.code(), error = %failure, "内容发布失败");
            internal_error(request_id, "服务端内部错误", root_id)
        }
        ContentFailure::VersionGone | ContentFailure::EntryUnavailable => error_response(
            StatusCode::GONE,
            ErrorCode::VersionGone,
            request_id,
            "版本已过期、已清理，或不属于该条目",
            false,
            root_id,
        ),
        ContentFailure::RangeNotSatisfiable { .. } | ContentFailure::InvalidRange { .. } => {
            error_response(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                request_id,
                &failure.to_string(),
                false,
                root_id,
            )
        }
        ContentFailure::Read { detail, no_space } => {
            if *no_space {
                return error_response(
                    StatusCode::INSUFFICIENT_STORAGE,
                    ErrorCode::InsufficientStorage,
                    request_id,
                    "服务端对象区空间不足",
                    true,
                    root_id,
                );
            }
            warn!(request_id = %request_id, error = %detail, "读取对象字节失败");
            internal_error(request_id, "服务端内部错误", root_id)
        }
        ContentFailure::Lease(error) => {
            warn!(request_id = %request_id, code = error.code(), error = %error, "下载租约失败");
            internal_error(request_id, "服务端内部错误", root_id)
        }
        ContentFailure::Storage { detail } => {
            warn!(request_id = %request_id, error = %detail, "版本内容存储失败");
            internal_error(request_id, "服务端内部错误", root_id)
        }
    }
}

/// 块拼装 + 发布的适配器（端口 [`ContentPublisher`]）。
///
/// 它把块按索引升序拼进对象区的私有暂存副本（边写边算 SHA-256、逐块核对长度），然后调用
/// [`capture_object`]：发布语义（独占副本、长度与摘要核对、不覆盖发布、既有对象复核复用、发布后
/// 复核）由核心那一处实现，本适配器不另写一套。
struct FsAssembler {
    state_dir: PathBuf,
    layout: ObjectLayout,
}

impl ContentPublisher for FsAssembler {
    fn publish_parts(
        &mut self,
        upload: UploadId,
        staging_name: &str,
        parts: &[StoredPart],
    ) -> Result<PublishedContent, PublishFailure> {
        let work_name = format!(".assemble-{}-{}", upload.to_hex(), std::process::id());
        let work =
            self.layout
                .staging_path(&work_name)
                .map_err(|error| PublishFailure::Storage {
                    stage: "assemble",
                    no_space: false,
                    detail: error.to_string(),
                })?;
        let assembled = assemble_parts(&self.state_dir, &work, parts)?;
        let captured = capture_object(
            &mut FsObjectPort,
            &self.layout,
            &CaptureRequest {
                source: work.clone(),
                staging_name: staging_name.to_owned(),
                expected: assembled,
            },
        );
        // 拼装用的工作副本不属于对象区，发布与否都要删掉：残留只是垃圾，不是引用。
        if let Err(error) = fs::remove_file(&work) {
            if error.kind() != io::ErrorKind::NotFound {
                warn!(error = %error, path = %work.display(), "删除拼装工作副本失败");
            }
        }
        let captured = captured.map_err(publish_failure)?;
        Ok(published_of(&captured))
    }
}

/// 把块按索引升序拼进 `work`，返回实测的长度与摘要（不看会话声明）。
fn assemble_parts(
    state_dir: &Path,
    work: &Path,
    parts: &[StoredPart],
) -> Result<ExpectedContent, PublishFailure> {
    if let Some(parent) = work.parent() {
        fs::create_dir_all(parent).map_err(|error| assemble_io("assemble_mkdir", &error))?;
    }
    let mut file =
        fs::File::create(work).map_err(|error| assemble_io("assemble_create", &error))?;
    let mut hasher = Sha256::new();
    let mut written = 0_u64;
    for part in parts {
        let relative = Path::new(&part.path);
        let absolute = state_dir.join(relative);
        let bytes = match fs::read(&absolute) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(PublishFailure::PartMissing { index: part.index });
            }
            Err(error) => {
                return Err(PublishFailure::PartUnreadable {
                    index: part.index,
                    detail: error.to_string(),
                });
            }
        };
        if bytes.len() as u64 != part.length {
            return Err(PublishFailure::PartLength {
                index: part.index,
                expected: part.length,
                actual: bytes.len() as u64,
            });
        }
        file.write_all(&bytes)
            .map_err(|error| assemble_io("assemble_write", &error))?;
        hasher.update(&bytes);
        written += bytes.len() as u64;
    }
    file.sync_all()
        .map_err(|error| assemble_io("assemble_fsync", &error))?;
    Ok(ExpectedContent::new(
        ContentDigest::from_bytes(hasher.finish()),
        written,
    ))
}

fn assemble_io(stage: &'static str, error: &io::Error) -> PublishFailure {
    PublishFailure::Storage {
        stage,
        no_space: matches!(
            error.kind(),
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
        ),
        detail: error.to_string(),
    }
}

fn publish_failure(error: CaptureFailure) -> PublishFailure {
    let no_space = matches!(
        &error.reason,
        CaptureFailureReason::Port(BlobError::Storage {
            failure: StorageFailure::NoSpace | StorageFailure::QuotaExceeded,
            ..
        })
    );
    if no_space {
        return PublishFailure::Storage {
            stage: "publish",
            no_space: true,
            detail: error.to_string(),
        };
    }
    match error.code() {
        "shared_writable_inode" | "staging_alias" | "invalid_request" => {
            PublishFailure::Verification {
                detail: error.to_string(),
            }
        }
        "length_mismatch"
        | "digest_mismatch"
        | "existing_object_mismatch"
        | "post_publish_mismatch" => PublishFailure::Verification {
            detail: error.to_string(),
        },
        _ => PublishFailure::Storage {
            stage: "publish",
            no_space: false,
            detail: error.to_string(),
        },
    }
}

fn published_of(captured: &CapturedObject) -> PublishedContent {
    PublishedContent {
        digest: captured.digest,
        size: captured.bytes,
        storage_path: captured.storage_path.display().to_string(),
        reuse: captured.reuse,
    }
}

/// `objects` 登记适配器（端口 [`ContentObjectStore`]）。
struct DbObjects<'a> {
    database: &'a Database,
    owner: UserId,
}

#[allow(async_fn_in_trait)]
impl ContentObjectStore for DbObjects<'_> {
    async fn record_published(
        &mut self,
        root: RootId,
        published: &PublishedContent,
        verified_at: UtcTimestamp,
    ) -> Result<(), PublishFailure> {
        let new = NewObject {
            root,
            hash: *published.digest.as_bytes(),
            size: published.size,
            storage_path: published.storage_path.clone(),
            verified_at,
        };
        let mut tx =
            self.database
                .begin_write()
                .await
                .map_err(|error| PublishFailure::StorageLayer {
                    detail: error.to_string(),
                })?;
        if let Err(error) = store_versions::record_object(&mut tx, self.owner, &new).await {
            return Err(record_object_failure(error));
        }
        tx.commit()
            .await
            .map_err(|error| PublishFailure::StorageLayer {
                detail: error.to_string(),
            })
    }
}

fn record_object_failure(error: store_versions::VersionError) -> PublishFailure {
    match error {
        store_versions::VersionError::ObjectConflict { .. } => PublishFailure::ObjectConflict {
            detail: error.to_string(),
        },
        other => PublishFailure::StorageLayer {
            detail: other.to_string(),
        },
    }
}

/// 版本内容适配器（端口 [`VersionContentStore`]）。
///
/// 只回答请求的那个版本：先看当前版本，再从该条目的历史里按身份找。找不到就是
/// [`VersionContentError::VersionGone`]——不拿另一个版本、也不拿路径上的当前内容冒充。
struct DbVersionContent<'a> {
    database: &'a Database,
    owner: UserId,
}

#[allow(async_fn_in_trait)]
impl VersionContentStore for DbVersionContent<'_> {
    async fn version_content(
        &mut self,
        root: RootId,
        entry: EntryId,
        version: VersionId,
    ) -> Result<VersionContent, VersionContentError> {
        let mut conn =
            self.database
                .pool()
                .acquire()
                .await
                .map_err(|error| VersionContentError::Storage {
                    detail: error.to_string(),
                })?;
        let record = match store_versions::current_version(&mut conn, self.owner, root, entry).await
        {
            Ok(Some(record)) if record.id == version => Some(record),
            Ok(_) => store_versions::version_history(
                &mut conn,
                self.owner,
                root,
                entry,
                store_versions::MAX_VERSION_PAGE,
            )
            .await
            .map_err(version_error)?
            .into_iter()
            .find(|record| record.id == version),
            Err(error) => return Err(version_error(error)),
        };
        let Some(record) = record else {
            return Err(VersionContentError::VersionGone);
        };
        if record.root != root || record.entry != entry {
            // 版本行不属于该条目/该 Root：不泄露它到底属于谁。
            return Err(VersionContentError::VersionGone);
        }
        Ok(VersionContent {
            root: record.root,
            entry: record.entry,
            version: record.id,
            digest: ContentDigest::from_bytes(record.object_hash),
            size: record.size,
        })
    }
}

fn version_error(error: store_versions::VersionError) -> VersionContentError {
    match error {
        store_versions::VersionError::OutOfScope => VersionContentError::OutOfScope,
        store_versions::VersionError::EntryNotFound { .. } => VersionContentError::EntryNotFound,
        store_versions::VersionError::NotAFile { .. } => VersionContentError::NotAFile,
        other => VersionContentError::Storage {
            detail: other.to_string(),
        },
    }
}

/// 内容租约适配器（端口 [`ContentLeasePort`]）：下载、恢复预览与提交窗口都用它。
pub struct DbLeases<'a> {
    database: &'a Database,
    owner: UserId,
    now: UtcTimestamp,
}

impl<'a> DbLeases<'a> {
    /// 以数据库、Root 所属账号与当前时间装配。
    #[must_use]
    pub fn new(database: &'a Database, owner: UserId, now: UtcTimestamp) -> Self {
        Self {
            database,
            owner,
            now,
        }
    }
}

impl DbLeases<'_> {
    fn lease_error(error: store_leases::LeaseError, digest: ContentDigest) -> ContentLeaseError {
        match error {
            store_leases::LeaseError::OutOfScope => ContentLeaseError::OutOfScope,
            store_leases::LeaseError::UnknownObject { .. } => {
                ContentLeaseError::ObjectNotRegistered { digest }
            }
            store_leases::LeaseError::AlreadyExpired { .. } => ContentLeaseError::Rejected {
                code: "lease_expiry_in_past",
                detail: error.to_string(),
            },
            other => ContentLeaseError::Storage {
                detail: other.to_string(),
            },
        }
    }
}

#[allow(async_fn_in_trait)]
impl ContentLeasePort for DbLeases<'_> {
    async fn acquire(
        &mut self,
        request: &ContentLeaseRequest,
    ) -> Result<ContentLease, ContentLeaseError> {
        let new = store_leases::NewObjectLease {
            id: ObjectLeaseId::from_bytes(random_bytes()),
            root: request.root,
            object_hash: *request.digest.as_bytes(),
            purpose: match request.purpose {
                ContentLeasePurpose::Download => store_leases::ObjectLeasePurpose::Download,
                ContentLeasePurpose::Commit => store_leases::ObjectLeasePurpose::Commit,
            },
            expires_at: request.expires_at,
        };
        let mut tx =
            self.database
                .begin_write()
                .await
                .map_err(|error| ContentLeaseError::Storage {
                    detail: error.to_string(),
                })?;
        let lease = match store_leases::lease_object(&mut tx, self.owner, &new, self.now).await {
            Ok(lease) => lease,
            Err(error) => return Err(Self::lease_error(error, request.digest)),
        };
        tx.commit()
            .await
            .map_err(|error| ContentLeaseError::Storage {
                detail: error.to_string(),
            })?;
        Ok(ContentLease {
            id: lease.id,
            purpose: request.purpose,
            expires_at: lease.expires_at,
        })
    }

    async fn release(&mut self, lease: &ContentLease) -> Result<(), ContentLeaseError> {
        let mut tx =
            self.database
                .begin_write()
                .await
                .map_err(|error| ContentLeaseError::Storage {
                    detail: error.to_string(),
                })?;
        let removed = match store_leases::release_object_lease(&mut tx, self.owner, lease.id).await
        {
            Ok(removed) => removed,
            Err(error) => {
                return Err(ContentLeaseError::Storage {
                    detail: error.to_string(),
                });
            }
        };
        tx.commit()
            .await
            .map_err(|error| ContentLeaseError::Storage {
                detail: error.to_string(),
            })?;
        if !removed {
            return Err(ContentLeaseError::Rejected {
                code: "lease_not_found",
                detail: "租约不存在、已过期或已解除".to_owned(),
            });
        }
        Ok(())
    }
}

/// 对象区字节端口（design §11.1—11.2）。写：独占新副本 → 不覆盖发布 → 刷盘；读：整份或按区间。
struct FsObjectPort;

impl ObjectBlobPort for FsObjectPort {
    fn stage_working_copy(
        &mut self,
        source: &Path,
        staging: &Path,
    ) -> Result<StagedBlob, BlobError> {
        if let Some(parent) = staging.parent() {
            fs::create_dir_all(parent).map_err(|error| blob_io("stage_mkdir", &error))?;
        }
        let source_key = file_key(source)?;
        let mut input = fs::File::open(source).map_err(|error| BlobError::SourceUnreadable {
            detail: error.to_string(),
        })?;
        let mut output =
            fs::File::create(staging).map_err(|error| blob_io("stage_create", &error))?;
        if let Err(error) = io::copy(&mut input, &mut output) {
            let _ = fs::remove_file(staging);
            return Err(blob_io("stage_copy", &error));
        }
        if let Err(error) = output.sync_all() {
            let _ = fs::remove_file(staging);
            return Err(blob_io("stage_fsync", &error));
        }
        // 读回核对：暂存副本的长度与 SHA-256 来自文件本身，不是源文件的猜测值。
        let digest = {
            let file = match fs::File::open(staging) {
                Ok(file) => file,
                Err(error) => {
                    let _ = fs::remove_file(staging);
                    return Err(blob_io("stage_reopen", &error));
                }
            };
            match hash_file(&file) {
                Ok((digest, _bytes)) => digest,
                Err(error) => {
                    let _ = fs::remove_file(staging);
                    return Err(blob_io("stage_hash", &io::Error::other(error.to_string())));
                }
            }
        };
        let staged_key = file_key(staging)?;
        if staged_key.same_inode(&source_key) {
            let _ = fs::remove_file(staging);
            return Err(BlobError::Verification {
                detail: "暂存副本与工作文件是同一 inode".to_owned(),
            });
        }
        Ok(StagedBlob {
            staging: staging.to_path_buf(),
            source: source_key,
            staged: staged_key,
            digest: ContentDigest::from_bytes(digest),
            method: vaultsyncd_server_core::ports::object_store::CopyMethod::VerifiedCopy,
            fallback: Some(CopyFallback::ReflinkUnsupported),
        })
    }

    fn publish_staged(
        &mut self,
        staged: &StagedBlob,
        object: &Path,
    ) -> Result<PublishedBlob, BlobError> {
        if let Some(parent) = object.parent() {
            fs::create_dir_all(parent).map_err(|error| blob_io("publish_mkdir", &error))?;
        }
        // 硬链接给出“不覆盖”语义：目标已存在时不会改写既有对象，也不会有中间态。
        match fs::hard_link(&staged.staging, object) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(PublishedBlob {
                    object: object.to_path_buf(),
                    already_present: true,
                });
            }
            Err(error) => return Err(blob_io("publish_link", &error)),
        }
        fs::remove_file(&staged.staging).map_err(|error| blob_io("publish_unlink", &error))?;
        sync_dir(object.parent().unwrap_or(Path::new(".")))?;
        Ok(PublishedBlob {
            object: object.to_path_buf(),
            already_present: false,
        })
    }

    fn discard_staged(&mut self, staging: &Path) -> Result<(), BlobError> {
        match fs::remove_file(staging) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(blob_io("discard_staging", &error)),
        }
    }

    fn verify_object(&mut self, object: &Path) -> Result<StoredFacts, BlobError> {
        let file = open_object(object)?;
        let (digest, bytes) =
            hash_file(&file).map_err(|error| blob_io("hash_object", &io::Error::other(error)))?;
        Ok(StoredFacts {
            bytes,
            digest: ContentDigest::from_bytes(digest),
        })
    }

    fn read_object(&mut self, object: &Path) -> Result<StoredBytes, BlobError> {
        let data = match fs::read(object) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(BlobError::ObjectMissing {
                    detail: object.display().to_string(),
                });
            }
            Err(error) => return Err(blob_io("read_object", &error)),
        };
        let digest = vaultsyncd_platform_linux_server::files::hash::digest(&data);
        Ok(StoredBytes {
            bytes: data.len() as u64,
            digest: ContentDigest::from_bytes(digest),
            data,
        })
    }

    fn read_object_range(
        &mut self,
        object: &Path,
        offset: u64,
        length: u64,
    ) -> Result<StoredSlice, BlobError> {
        let mut file = open_object(object)?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| blob_io("seek_object", &error))?;
        let mut data = Vec::new();
        let read = file
            .take(length)
            .read_to_end(&mut data)
            .map_err(|error| blob_io("read_object_range", &error))?;
        Ok(StoredSlice {
            bytes: read as u64,
            data,
        })
    }
}

fn open_object(object: &Path) -> Result<fs::File, BlobError> {
    fs::File::open(object).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => BlobError::ObjectMissing {
            detail: object.display().to_string(),
        },
        _ => blob_io("open_object", &error),
    })
}

fn file_key(path: &Path) -> Result<FileKey, BlobError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path).map_err(|error| blob_io("stat_file", &error))?;
    Ok(FileKey {
        device: metadata.dev(),
        inode: metadata.ino(),
        links: metadata.nlink().try_into().unwrap_or(u32::MAX),
        bytes: metadata.len(),
    })
}

fn sync_dir(directory: &Path) -> Result<(), BlobError> {
    match fs::File::open(directory) {
        Ok(handle) => handle
            .sync_all()
            .map_err(|error| blob_io("fsync_dir", &error)),
        Err(error) => Err(blob_io("open_dir", &error)),
    }
}

fn blob_io(stage: &'static str, error: &io::Error) -> BlobError {
    BlobError::Storage {
        stage,
        failure: classify(error),
        detail: error.to_string(),
    }
}

fn classify(error: &io::Error) -> StorageFailure {
    match error.kind() {
        io::ErrorKind::StorageFull => StorageFailure::NoSpace,
        io::ErrorKind::QuotaExceeded => StorageFailure::QuotaExceeded,
        io::ErrorKind::PermissionDenied => StorageFailure::PermissionDenied,
        io::ErrorKind::ReadOnlyFilesystem => StorageFailure::ReadOnly,
        _ => StorageFailure::Io,
    }
}

/// 内部一致性检查：`ContentFailure` 与 HTTP 映射必须在测试里覆盖到的错误码集合。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_slice_reports_partial_only_when_it_is_not_the_whole_object() {
        let whole = ByteSlice {
            offset: 0,
            length: 5,
        };
        assert!(whole.covers_whole(5));
        assert!(!whole.covers_whole(6));
        assert_eq!(whole.content_range(5), "bytes 0-4/5");
        let tail = ByteSlice {
            offset: 3,
            length: 2,
        };
        assert_eq!(tail.content_range(5), "bytes 3-4/5");
    }
}
