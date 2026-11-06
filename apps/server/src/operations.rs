//! 操作端点：`POST /v1/roots/{root}/operations`、`GET /v1/roots/{root}/operations/{op}`（design
//! §15.2—15.4、I01、I06）。
//!
//! 规则都在 [`vaultsyncd_server_core::api::operations`] 里；本模块只做三件事：
//!
//! - **翻译**：把 wire 的身份/计数文本解析成核心的强类型请求（解析失败即 400，不猜默认值）；
//! - **真实幂等**：操作记录读写走 `server-store::operations`（`operations` 表），同一
//!   `(root, device_id, op_id)` 与同一语义摘要就是重放，不同摘要就是 `OP_ID_REUSED`；`server_epoch`
//!   与 Root 当前 epoch 的比对来自 `roots` 行；
//! - **回答**：`committed` 是终态 200，`rejected` 是稳定的 409 加错误信封，**`pending` 是 202** 且
//!   响应体里不出现 `revision`/`root_seq`/`version_id`/`entry_id`——已受理不等于已同步（I01）。
//!
//! # 本版的行为边界（S047）
//!
//! 命令体的执行链路（put/create/move/delete/restore 的提交协调与其存储/工作目录适配器）还没有装配，
//! 那属服务端整合闭环（S052）。因此**新命令一律显式 503 `SERVICE_NOT_READY`**：不登记 `operations`
//! 行、不追加事件批次、不返回 202 的“已受理”假象。已有的操作记录（`pending`/`committed`/
//! `rejected`）照真实语义回答——重试、响应丢失与结果查询在本版就是完整可用的。
//!
//! 失败映射（design §15.4）：凭据 401 `AUTH_EXPIRED`、越权 403 `ACCESS_DENIED`、形状 400
//! `INVALID_REQUEST`、同 OpId 不同请求 409 `OP_ID_REUSED`、过期基线/子树 409、旧 epoch 410
//! `EPOCH_CHANGED`、名称/条目种类 422、Root 不可写与存储故障 503、空间不足 507。

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};
use vaultsyncd_domain::ids::{DeviceId, EntryId, OpId, RootId, UploadId, UserId, VersionId};
use vaultsyncd_protocol::generated::{
    ConflictPolicy as WirePolicy, ErrorCode, OperationKind as WireKind,
    OperationRequest as WireRequest, OperationResult, OperationState as WireState,
};
use vaultsyncd_server_core::api::operations::{
    self, ConflictPolicy, OperationFailure, OperationKind, OperationRequest, RecordedOperation,
    RecordedOutcome, RootFacts,
};
use vaultsyncd_server_core::objects::{ContentDigest, ExpectedContent};
use vaultsyncd_server_store::operations as store_operations;
use vaultsyncd_server_store::roots::{self as store_roots, RootHealth};

use crate::auth::AuthContext;
use crate::executor::{ExecutorContext, PutCommand};
use crate::uploads::{Caller, FromIdentity, bad_request, denied, error_response, parse_identity};
use vaultsyncd_server_store::uploads as store_uploads;

/// 操作端点共享的进程内状态。
pub struct OperationsContext {
    auth: Arc<AuthContext>,
    executor: Arc<ExecutorContext>,
}

impl OperationsContext {
    /// 以认证上下文与提交执行器装配；操作的存储读写走同一个数据库。
    #[must_use]
    pub fn new(auth: Arc<AuthContext>, executor: Arc<ExecutorContext>) -> Self {
        Self { auth, executor }
    }
}

/// `POST /v1/roots/{root}/operations`：受理或重放一次修改命令。
pub async fn submit(
    State(context): State<Arc<OperationsContext>>,
    UrlPath(root): UrlPath<String>,
    headers: HeaderMap,
    body: Result<Json<WireRequest>, JsonRejection>,
) -> Response {
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return bad_request(
                &caller.request_id,
                &format!("请求体不是合法的操作请求：{rejection}"),
            );
        }
    };
    let request = match parse_request(&body) {
        Ok(request) => request,
        Err(failure) => return failure_response(&caller.request_id, &failure, root_id),
    };
    let facts = match root_facts(
        &context,
        caller.principal.user_id,
        root_id,
        &caller.request_id,
    )
    .await
    {
        Ok(facts) => facts,
        Err(response) => return response,
    };
    // epoch 与 Root 可用性先于命令形状，也先于任何操作记录读写（design §15.4）。
    if let Err(failure) = operations::admit(facts, &request) {
        return failure_response(&caller.request_id, &failure, root_id);
    }

    let key = store_operations::OperationKey {
        root: root_id,
        device: request.device,
        op: request.op,
    };
    let fields = request.digest_fields();
    let pairs: Vec<(&str, &[u8])> = fields
        .iter()
        .map(|(name, value)| (*name, value.as_bytes()))
        .collect();
    let digest = match store_operations::request_digest(&pairs) {
        Ok(digest) => digest,
        Err(error) => {
            warn!(request_id = %caller.request_id, error = %error, "操作请求摘要失败");
            return internal_error(&caller.request_id, "服务端内部错误", Some(root_id));
        }
    };

    let existing = {
        let mut conn = match context.auth.database().pool().acquire().await {
            Ok(conn) => conn,
            Err(error) => {
                warn!(request_id = %caller.request_id, error = %error, "读取操作记录失败");
                return internal_error(&caller.request_id, "服务端内部错误", Some(root_id));
            }
        };
        match store_operations::operation_in_scope(&mut conn, caller.principal.user_id, &key).await
        {
            Ok(record) => record,
            Err(error) => {
                warn!(request_id = %caller.request_id, error = %error, "读取操作记录失败");
                return internal_error(&caller.request_id, "服务端内部错误", Some(root_id));
            }
        }
    };

    if let Some(record) = existing {
        let recorded = match recorded_operation(&record) {
            Ok(recorded) => recorded,
            Err(failure) => return failure_response(&caller.request_id, &failure, root_id),
        };
        return match operations::decide_replay(&recorded, digest.as_bytes()) {
            Ok(outcome) => {
                info!(
                    request_id = %caller.request_id,
                    op = %request.op,
                    status = outcome.http_status(),
                    "操作请求命中已记录的结果"
                );
                result_response(&caller.request_id, request.op, outcome, Some(true), root_id)
            }
            Err(failure) => failure_response(&caller.request_id, &failure, root_id),
        };
    }

    // 新命令：内容 put 走真实执行链路（意图 → 发布 → 元数据事务）；其余命令的适配器尚未落地，
    // 继续显式 503 `SERVICE_NOT_READY`，不写任何状态、不伪造成功。
    if request.kind != OperationKind::PutContent {
        warn!(
            request_id = %caller.request_id,
            root = %root_id.to_hex(),
            kind = request.kind.as_text(),
            "该命令的存储/工作目录适配器尚未装配，命令未被受理"
        );
        return failure_response(
            &caller.request_id,
            &OperationFailure::ExecutorUnavailable,
            root_id,
        );
    }
    let Some(entry) = request.entry else {
        return failure_response(
            &caller.request_id,
            &OperationFailure::InvalidRequest {
                detail: "put_content 缺少 entry_id".to_owned(),
            },
            root_id,
        );
    };
    let (Some(revision), Some(upload)) = (request.base_revision, request.upload) else {
        return failure_response(
            &caller.request_id,
            &OperationFailure::InvalidRequest {
                detail: "put_content 缺少 base_revision/upload_id".to_owned(),
            },
            root_id,
        );
    };
    let content = match finalized_content(&context, caller.principal.user_id, root_id, upload).await
    {
        Ok(content) => content,
        Err(failure) => return failure_response(&caller.request_id, &failure, root_id),
    };
    let at = match crate::executor::server_now() {
        Ok(at) => at,
        Err(_) => return internal_error(&caller.request_id, "系统时钟不可用", Some(root_id)),
    };
    let command = PutCommand {
        device: request.device,
        op: request.op,
        entry,
        expected_epoch: request.expected_epoch,
        base_revision: revision,
        content,
        local_generation: request.local_generation,
    };
    match context
        .executor
        .execute_put(caller.principal.user_id, key, &command, at)
        .await
    {
        Ok(committed) => {
            info!(
                request_id = %caller.request_id,
                op = %request.op,
                revision = %committed.record.revision,
                root_seq = %committed.record.seq,
                version = %committed.record.version,
                "操作已提交"
            );
            let result = OperationResult {
                op_id: request.op.to_hex(),
                state: WireState::Committed,
                replayed: Some(false),
                conflict: None,
                echo_local_generation: request.local_generation.map(|value| value.to_string()),
                entry_id: Some(entry.to_hex()),
                error_code: None,
                error_message: None,
                revision: Some(committed.record.revision.to_string()),
                root_seq: Some(committed.record.seq.to_string()),
                version_id: Some(committed.record.version.to_hex()),
            };
            (StatusCode::OK, Json(result)).into_response()
        }
        Err(failure) => failure_response(&caller.request_id, &failure, root_id),
    }
}

/// 取一次已 finalize 的内容引用（`uploads.state = finalized` 的摘要与长度）。
///
/// 未 finalize 的会话不能提交：没有固定的内容地址就没有可发布的对象。会话不存在/越权/未收齐
/// 一律按 `INVALID_REQUEST` 回答，不猜默认内容。
#[allow(clippy::result_large_err)]
async fn finalized_content(
    context: &Arc<OperationsContext>,
    owner: UserId,
    root: RootId,
    upload: UploadId,
) -> Result<ExpectedContent, OperationFailure> {
    let mut conn = match context.auth.database().pool().acquire().await {
        Ok(conn) => conn,
        Err(error) => {
            return Err(OperationFailure::Storage {
                detail: error.to_string(),
            });
        }
    };
    let record = store_uploads::upload_in_scope(&mut conn, owner, upload)
        .await
        .map_err(|error| OperationFailure::Storage {
            detail: error.to_string(),
        })?
        .ok_or_else(|| OperationFailure::InvalidRequest {
            detail: "上传会话不存在或不属于该账号".to_owned(),
        })?;
    if record.root != root {
        return Err(OperationFailure::InvalidRequest {
            detail: "上传会话不属于该 Root".to_owned(),
        });
    }
    if record.state != store_uploads::UploadState::Finalized {
        return Err(OperationFailure::InvalidRequest {
            detail: "上传会话尚未 finalize，不能提交".to_owned(),
        });
    }
    Ok(ExpectedContent::new(
        ContentDigest::from_bytes(record.hash),
        record.expected_size,
    ))
}

/// `GET /v1/roots/{root}/operations/{op}`：按 OpId 取操作结果。
pub async fn status(
    State(context): State<Arc<OperationsContext>>,
    UrlPath((root, op)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Some(op_id) = parse_identity::<OpId>(&op) else {
        return bad_request(&caller.request_id, "OpId 不是 32 位小写十六进制");
    };
    let record = {
        let mut conn = match context.auth.database().pool().acquire().await {
            Ok(conn) => conn,
            Err(error) => {
                warn!(request_id = %caller.request_id, error = %error, "读取操作记录失败");
                return internal_error(&caller.request_id, "服务端内部错误", Some(root_id));
            }
        };
        match store_operations::operation_of_root_in_scope(
            &mut conn,
            caller.principal.user_id,
            root_id,
            op_id,
        )
        .await
        {
            Ok(record) => record,
            Err(error) => {
                warn!(request_id = %caller.request_id, error = %error, "读取操作记录失败");
                return internal_error(&caller.request_id, "服务端内部错误", Some(root_id));
            }
        }
    };
    let Some(record) = record else {
        return failure_response(
            &caller.request_id,
            &OperationFailure::UnknownOperation,
            root_id,
        );
    };
    let recorded = match recorded_operation(&record) {
        Ok(recorded) => recorded,
        Err(failure) => return failure_response(&caller.request_id, &failure, root_id),
    };
    result_response(&caller.request_id, op_id, &recorded.outcome, None, root_id)
}

/// 读 Root 的当前 epoch 与可写性；越权与不存在同一个 403 回答。
// 失败类型是 axum 的 Response（调用方直接把它当返回值用）：与 `auth::principal_from` 同一约定。
#[allow(clippy::result_large_err)]
async fn root_facts(
    context: &Arc<OperationsContext>,
    owner: UserId,
    root: RootId,
    request_id: &str,
) -> Result<RootFacts, Response> {
    let mut conn = match context.auth.database().pool().acquire().await {
        Ok(conn) => conn,
        Err(error) => {
            warn!(request_id = %request_id, error = %error, "读取 Root 失败");
            return Err(internal_error(request_id, "服务端内部错误", Some(root)));
        }
    };
    match store_roots::root_in_scope(&mut conn, owner, root).await {
        Ok(Some(record)) => Ok(RootFacts {
            epoch: record.epoch,
            accepts_writes: record.health == RootHealth::Healthy,
        }),
        Ok(None) => Err(denied(request_id, "Root 不在授权范围")),
        Err(error) => {
            warn!(request_id = %request_id, error = %error, "读取 Root 失败");
            Err(internal_error(request_id, "服务端内部错误", Some(root)))
        }
    }
}

/// wire 请求 → 核心请求；身份/计数文本必须是规范形式，否则 400（不猜默认值）。
fn parse_request(body: &WireRequest) -> Result<OperationRequest, OperationFailure> {
    let conflict_policy = match body.conflict_policy {
        None => ConflictPolicy::PreserveBoth,
        Some(WirePolicy::PreserveBoth) => ConflictPolicy::PreserveBoth,
    };
    Ok(OperationRequest {
        op: parse_identity::<OpId>(&body.op_id)
            .ok_or_else(|| invalid("op_id 不是 32 位小写十六进制"))?,
        device: parse_identity::<DeviceId>(&body.device_id)
            .ok_or_else(|| invalid("device_id 不是 32 位小写十六进制"))?,
        kind: match body.kind {
            WireKind::PutContent => OperationKind::PutContent,
            WireKind::Create => OperationKind::Create,
            WireKind::Move => OperationKind::Move,
            WireKind::Delete => OperationKind::Delete,
            WireKind::Restore => OperationKind::Restore,
        },
        expected_epoch: counter(&body.server_epoch, "server_epoch")?,
        entry: optional_identity::<EntryId>(body.entry_id.as_deref(), "entry_id")?,
        base_revision: optional_counter(body.base_revision.as_deref(), "base_revision")?,
        base_version: optional_identity::<VersionId>(
            body.base_version_id.as_deref(),
            "base_version_id",
        )?,
        upload: optional_identity::<UploadId>(body.upload_id.as_deref(), "upload_id")?,
        local_generation: optional_counter(body.local_generation.as_deref(), "local_generation")?,
        conflict_policy,
        parent: optional_identity::<EntryId>(body.parent_id.as_deref(), "parent_id")?,
        name: body.name.clone(),
        expected_absent: body.expected_absent,
        subtree_token: match body.subtree_token.as_deref() {
            None => None,
            Some(text) => Some(parse_token(text)?),
        },
        version: optional_identity::<VersionId>(body.version_id.as_deref(), "version_id")?,
    })
}

fn optional_identity<T>(text: Option<&str>, field: &str) -> Result<Option<T>, OperationFailure>
where
    T: FromIdentity,
{
    match text {
        None => Ok(None),
        Some(text) => parse_identity::<T>(text)
            .map(Some)
            .ok_or_else(|| invalid(format!("{field} 不是 32 位小写十六进制"))),
    }
}

fn optional_counter<T>(text: Option<&str>, field: &str) -> Result<Option<T>, OperationFailure>
where
    T: std::str::FromStr,
{
    match text {
        None => Ok(None),
        Some(text) => counter(text, field).map(Some),
    }
}

fn counter<T>(text: &str, field: &str) -> Result<T, OperationFailure>
where
    T: std::str::FromStr,
{
    text.parse()
        .map_err(|_| invalid(format!("{field} 不是规范十进制计数")))
}

/// 32 位小写十六进制的 16 字节不透明值（目录删除的子树 token）。
fn parse_token(text: &str) -> Result<[u8; 16], OperationFailure> {
    let canonical = text.len() == 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !canonical {
        return Err(invalid("subtree_token 不是 32 位小写十六进制"));
    }
    let mut bytes = [0_u8; 16];
    for (slot, chunk) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        let high = (chunk[0] as char)
            .to_digit(16)
            .expect("已校验是十六进制数字");
        let low = (chunk[1] as char)
            .to_digit(16)
            .expect("已校验是十六进制数字");
        *slot = u8::try_from(high * 16 + low).expect("两个十六进制数字不超过 255");
    }
    Ok(bytes)
}

fn invalid(detail: impl Into<String>) -> OperationFailure {
    OperationFailure::InvalidRequest {
        detail: detail.into(),
    }
}

/// 存储层的操作记录 → 核心的重放判定输入；坏行（committed 缺结果/序号）明确报存储故障。
fn recorded_operation(
    record: &store_operations::OperationRecord,
) -> Result<RecordedOperation, OperationFailure> {
    let outcome = match record.state {
        store_operations::OperationState::Committed => {
            let (Some(root_seq), Some(result_json)) = (record.root_seq, record.result_json.clone())
            else {
                return Err(OperationFailure::Storage {
                    detail: "committed 操作缺少 root_seq 或结果".to_owned(),
                });
            };
            RecordedOutcome::Committed {
                root_seq,
                result_json,
            }
        }
        store_operations::OperationState::Pending => RecordedOutcome::Pending,
        store_operations::OperationState::Rejected => RecordedOutcome::Rejected {
            result_json: record.result_json.clone(),
        },
    };
    Ok(RecordedOperation {
        request_digest: *record.digest.as_bytes(),
        outcome,
    })
}

/// 已记录的结果 → 响应。`replayed` 只在提交端点上有意义（GET 是查询而不是重放）。
fn result_response(
    request_id: &str,
    op: OpId,
    outcome: &RecordedOutcome,
    replayed: Option<bool>,
    root: RootId,
) -> Response {
    match outcome {
        RecordedOutcome::Committed { result_json, .. } => {
            match serde_json::from_str::<OperationResult>(result_json) {
                Ok(mut result) => {
                    result.op_id = op.to_hex();
                    result.state = WireState::Committed;
                    result.replayed = replayed;
                    (StatusCode::OK, Json(result)).into_response()
                }
                Err(error) => {
                    warn!(request_id = %request_id, error = %error, "操作结果不是契约形状");
                    error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorCode::NeedsRecovery,
                        request_id,
                        "操作结果记录不符合契约形状，需要人工核对",
                        false,
                        Some(root.to_hex()),
                    )
                }
            }
        }
        RecordedOutcome::Pending => (
            StatusCode::ACCEPTED,
            Json(OperationResult {
                op_id: op.to_hex(),
                state: WireState::Pending,
                replayed,
                conflict: None,
                echo_local_generation: None,
                entry_id: None,
                error_code: None,
                error_message: None,
                revision: None,
                root_seq: None,
                version_id: None,
            }),
        )
            .into_response(),
        RecordedOutcome::Rejected { result_json } => {
            // 稳定拒绝：同一 OpId 的结论不再改变，客户端据此重新规划而不是重试原请求。
            let recorded = result_json
                .as_deref()
                .and_then(|text| serde_json::from_str::<OperationResult>(text).ok());
            let code = recorded
                .as_ref()
                .and_then(|result| result.error_code)
                .unwrap_or(ErrorCode::InvalidRequest);
            let message = recorded
                .as_ref()
                .and_then(|result| result.error_message.clone())
                .unwrap_or_else(|| "操作已被稳定拒绝".to_owned());
            error_response(
                StatusCode::CONFLICT,
                code,
                request_id,
                &message,
                false,
                Some(root.to_hex()),
            )
        }
    }
}

/// 核心失败 → 错误信封（状态码与错误码都来自核心的契约表，不在这里另写一套）。
fn failure_response(request_id: &str, failure: &OperationFailure, root: RootId) -> Response {
    let status =
        StatusCode::from_u16(failure.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let code = match failure.design_error_code() {
        "ROOT_UNAVAILABLE" => ErrorCode::RootUnavailable,
        "EPOCH_CHANGED" => ErrorCode::EpochChanged,
        "OP_ID_REUSED" => ErrorCode::OpIdReused,
        "BASE_MISMATCH" => ErrorCode::BaseMismatch,
        "SUBTREE_CHANGED" => ErrorCode::SubtreeChanged,
        "RECONCILE_REQUIRED" => ErrorCode::ReconcileRequired,
        "INVALID_REQUEST" => ErrorCode::InvalidRequest,
        "INVALID_NAME" => ErrorCode::InvalidName,
        "UNSUPPORTED_ENTRY" => ErrorCode::UnsupportedEntry,
        operations::EXECUTOR_UNAVAILABLE_CODE => ErrorCode::ServiceNotReady,
        "NEEDS_RECOVERY" => ErrorCode::NeedsRecovery,
        "INSUFFICIENT_STORAGE" => ErrorCode::InsufficientStorage,
        other => {
            warn!(request_id = %request_id, code = other, "未知的操作失败码");
            ErrorCode::NeedsRecovery
        }
    };
    if matches!(code, ErrorCode::ServiceNotReady) {
        info!(
            request_id = %request_id,
            code = failure.design_error_code(),
            "操作端点显式拒绝"
        );
    }
    error_response(
        status,
        code,
        request_id,
        &failure.to_string(),
        failure.is_retryable(),
        Some(root.to_hex()),
    )
}

fn internal_error(request_id: &str, message: &str, root: Option<RootId>) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::NeedsRecovery,
        request_id,
        message,
        true,
        root.map(RootId::to_hex),
    )
}
