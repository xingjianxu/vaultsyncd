//! 一致性快照、`changes` 拉取与 SSE 提示端点（design §9.3—9.4、I09）。
//!
//! 规则在 [`vaultsyncd_server_core::api::events`] 与 [`vaultsyncd_server_core::api::snapshots`]，数据读写
//! 在 `server-store::{snapshots,changes}`；本模块只做 wire 翻译、认证授权与错误映射：
//!
//! - `POST /v1/roots/{root}/snapshots`：同一写事务里读 `RootSeq` 并复制活跃条目成短期清单（201）。
//! - `GET /v1/roots/{root}/snapshots/{snapshot}/entries`：只读清单分页，快照过期/未知 410，不返回空页。
//! - `GET /v1/roots/{root}/changes?after&limit`：有序事务批次，`limit` 不切事务；窗口事实与批次在同一个
//!   读事务里取；游标早于保留窗口或晚于当前头都是 410 `CURSOR_EXPIRED`，**不是空集**。
//! - `GET /v1/roots/{root}/events`：SSE 只提示“RootSeq 变了”。流自己轮询 `roots.seq`（不依赖进程内
//!   发布者，因此无论哪个写者推进 seq 都会提示），事件不携带批次内容，也不是数据来源——丢、重、乱序都由
//!   `changes` 的游标修复。进程进入 draining 时流主动结束，不拖住优雅停止。
//!
//! 查询串只接受契约声明的参数：未知参数、重复参数、非规范十进制一律 400（不猜意图）。

mod history;
pub use history::{list_conflicts, list_history, list_recovery, list_trash};

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;
use sqlx::Connection as _;
use tracing::warn;
use vaultsyncd_domain::ids::{RootId, SnapshotId, UserId};
use vaultsyncd_domain::version::RootSeq;
use vaultsyncd_protocol::generated::{
    ChangeBatch, ChangeList, EntryKind as WireKind, ErrorBody, ErrorCode, ErrorEnvelope, SeqHint,
    Snapshot, SnapshotEntry as WireEntry, SnapshotPage,
};
use vaultsyncd_server_core::api::events::{
    self, ChangesFailure, CursorProblem, DEFAULT_EVENT_RETENTION_SECONDS, SSE_KEEPALIVE_SECONDS,
    SSE_POLL_INTERVAL_MILLIS,
};
use vaultsyncd_server_core::api::snapshots as snapshot_rules;
use vaultsyncd_server_store::changes::{self as store_changes, ChangeError};
use vaultsyncd_server_store::entries::EntryKind;
use vaultsyncd_server_store::roots as store_roots;
use vaultsyncd_server_store::snapshots::{self as store_snapshots, SnapshotError};

use crate::auth::AuthContext;
use crate::lifecycle::{Lifecycle, Phase};
use crate::uploads::{
    Caller, bad_request, denied, internal_error, parse_decimal, parse_identity, random_bytes,
};

/// 端点的可调参数：生产用 [`SyncConfig::default`]，测试可缩短轮询间隔。
#[derive(Clone, Copy, Debug)]
pub struct SyncConfig {
    /// 事件保留期（秒）：早于它的批次视为已出窗口（design §9.4，默认 90 天）。
    pub event_retention_seconds: i64,
    /// SSE 轮询 `roots.seq` 的间隔。
    pub sse_poll_interval: Duration,
    /// SSE 保活注释的间隔。
    pub sse_keepalive: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            event_retention_seconds: DEFAULT_EVENT_RETENTION_SECONDS,
            sse_poll_interval: Duration::from_millis(SSE_POLL_INTERVAL_MILLIS),
            sse_keepalive: Duration::from_secs(SSE_KEEPALIVE_SECONDS),
        }
    }
}

/// 快照、`changes` 与 SSE 端点共享的进程内状态。
pub struct SyncContext {
    auth: Arc<AuthContext>,
    lifecycle: Arc<Lifecycle>,
    config: SyncConfig,
}

impl SyncContext {
    /// 以认证上下文与生命周期装配（SSE 要在 draining 时结束）。
    #[must_use]
    pub fn new(auth: Arc<AuthContext>, lifecycle: Arc<Lifecycle>) -> Self {
        Self::with_config(auth, lifecycle, SyncConfig::default())
    }

    /// 同 [`Self::new`]，但显式给出可调参数。
    #[must_use]
    pub fn with_config(
        auth: Arc<AuthContext>,
        lifecycle: Arc<Lifecycle>,
        config: SyncConfig,
    ) -> Self {
        Self {
            auth,
            lifecycle,
            config,
        }
    }
}

/// `POST /v1/roots/{root}/snapshots`：创建一致性快照。
pub async fn create_snapshot(
    State(context): State<Arc<SyncContext>>,
    UrlPath(root): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let id = SnapshotId::from_bytes(random_bytes());
    let expires_at = snapshot_rules::snapshot_expiry(caller.now);
    let created = async {
        let mut tx = context
            .auth
            .database()
            .begin_write()
            .await
            .map_err(|error| error.to_string())?;
        let record = store_snapshots::create_snapshot(
            &mut tx,
            caller.principal.user_id,
            root_id,
            id,
            caller.now,
            expires_at,
        )
        .await
        .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok::<_, String>(record)
    }
    .await;
    match created {
        Ok(record) => (
            StatusCode::CREATED,
            Json(Snapshot {
                snapshot_id: record.id.to_hex(),
                root_id: record.root.to_hex(),
                high_water_seq: record.high_water_seq.to_decimal(),
                server_epoch: record.epoch.to_decimal(),
                entry_count: record.entry_count.to_string(),
                expires_at: record.expires_at.as_unix_seconds().to_string(),
            }),
        )
            .into_response(),
        Err(detail) => storage_failure(&caller.request_id, &detail, root_id),
    }
}

/// `GET /v1/roots/{root}/snapshots/{snapshot}/entries`：翻页读取快照清单。
pub async fn list_snapshot_entries(
    State(context): State<Arc<SyncContext>>,
    UrlPath((root, snapshot)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let Some(snapshot_id) = parse_identity::<SnapshotId>(&snapshot) else {
        return bad_request(&caller.request_id, "快照身份不是 32 位小写十六进制");
    };
    let params = match QueryParams::parse(query.as_deref(), &["cursor", "limit"]) {
        Ok(params) => params,
        Err(failure) => return failure_response(&caller.request_id, &failure, root_id),
    };
    let parsed = params.decimal("cursor").and_then(|cursor| {
        let limit = snapshot_rules::snapshot_limit(params.decimal("limit")?)?;
        Ok((cursor.unwrap_or(0), limit))
    });
    let (cursor, limit) = match parsed {
        Ok(parsed) => parsed,
        Err(failure) => return failure_response(&caller.request_id, &failure, root_id),
    };

    let mut conn = match context.auth.database().pool().acquire().await {
        Ok(conn) => conn,
        Err(error) => return storage_failure(&caller.request_id, &error.to_string(), root_id),
    };
    let page = store_snapshots::snapshot_page(
        &mut conn,
        caller.principal.user_id,
        root_id,
        snapshot_id,
        cursor,
        limit,
        caller.now,
    )
    .await;
    match page {
        Ok(page) => (
            StatusCode::OK,
            Json(SnapshotPage {
                snapshot_id: page.snapshot.id.to_hex(),
                high_water_seq: page.snapshot.high_water_seq.to_decimal(),
                entries: page.entries.iter().map(wire_entry).collect(),
                next_cursor: page.next_cursor.map(|cursor| cursor.to_string()),
            }),
        )
            .into_response(),
        Err(error) => snapshot_error_response(&caller.request_id, &error, root_id),
    }
}

/// `GET /v1/roots/{root}/changes?after&limit`：拉取有序事务批次。
pub async fn list_changes(
    State(context): State<Arc<SyncContext>>,
    UrlPath(root): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let parsed = QueryParams::parse(query.as_deref(), &["after", "limit"]).and_then(|params| {
        let after = params
            .decimal("after")?
            .ok_or_else(|| ChangesFailure::InvalidRequest {
                detail: "缺少必填的查询参数 after".to_owned(),
            })?;
        let after = RootSeq::new(after).map_err(|_| ChangesFailure::InvalidRequest {
            detail: "after 超出 RootSeq 取值范围".to_owned(),
        })?;
        let limit = events::changes_limit(params.decimal("limit")?)?;
        Ok((after, limit))
    });
    let (after, limit) = match parsed {
        Ok(parsed) => parsed,
        Err(failure) => return failure_response(&caller.request_id, &failure, root_id),
    };

    let owner = caller.principal.user_id;
    let cutoff = events::retention_cutoff(caller.now, context.config.event_retention_seconds);
    let read = async {
        // 窗口事实与批次在同一个读事务里取：头、下沿与批次互相一致。
        let mut conn = context
            .auth
            .database()
            .pool()
            .acquire()
            .await
            .map_err(|error| ReadFailure::Storage(error.to_string()))?;
        let mut tx = begin_read(&mut conn).await?;
        let window = store_changes::change_window(&mut tx, owner, root_id, cutoff)
            .await
            .map_err(ReadFailure::from_change)?;
        events::judge_cursor(after, window.head, window.expired_through)
            .map_err(ReadFailure::Failure)?;
        let batches =
            store_changes::changes_after_whole_transactions(&mut tx, owner, root_id, after, limit)
                .await
                .map_err(ReadFailure::from_change)?;
        tx.commit()
            .await
            .map_err(|error| ReadFailure::Storage(error.to_string()))?;
        Ok::<_, ReadFailure>((window, batches))
    }
    .await;
    let (window, batches) = match read {
        Ok(read) => read,
        Err(ReadFailure::Failure(failure)) => {
            return failure_response(&caller.request_id, &failure, root_id);
        }
        Err(ReadFailure::OutOfScope) => {
            return denied(&caller.request_id, "Root 不在授权范围");
        }
        Err(ReadFailure::Storage(detail)) => {
            return storage_failure(&caller.request_id, &detail, root_id);
        }
    };
    let progress = events::page_progress(after, batches.last().map(|batch| batch.seq), window.head);
    (
        StatusCode::OK,
        Json(ChangeList {
            root_id: root_id.to_hex(),
            server_epoch: window.epoch.to_decimal(),
            head_seq: window.head.to_decimal(),
            next_after: progress.next_after.to_decimal(),
            has_more: progress.has_more,
            batches: batches
                .iter()
                .map(|batch| ChangeBatch {
                    seq: batch.seq.to_decimal(),
                    tx_id: batch.tx.to_hex(),
                    events_json: batch.events_json.clone(),
                    created_at: batch.created_at.as_unix_seconds().to_string(),
                })
                .collect(),
        }),
    )
        .into_response()
}

/// 读事务的失败：核心判定（410/400）、越权与存储故障分开，调用方各自映射。
enum ReadFailure {
    Failure(ChangesFailure),
    OutOfScope,
    Storage(String),
}

impl ReadFailure {
    fn from_change(error: ChangeError) -> Self {
        match error {
            ChangeError::OutOfScope => Self::OutOfScope,
            other => Self::Storage(other.to_string()),
        }
    }
}

/// 在连接上开读事务（延迟开始，首次读取时固定快照）。
async fn begin_read(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>, ReadFailure> {
    (**conn)
        .begin()
        .await
        .map_err(|error| ReadFailure::Storage(error.to_string()))
}

/// `GET /v1/roots/{root}/events`：SSE 唤醒提示。
pub async fn subscribe(
    State(context): State<Arc<SyncContext>>,
    UrlPath(root): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let (caller, root_id) = match Caller::resolve(&context.auth, &headers, &root).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let state = HintStream {
        context: Arc::clone(&context),
        owner: caller.principal.user_id,
        root: root_id,
        last_sent: None,
    };
    let events = stream::unfold(state, |mut state| async move {
        loop {
            // draining：结束流，让优雅停止不被长连接拖住；客户端会重连或转为轮询 `changes`。
            if state.context.lifecycle.phase() == Phase::Draining {
                return None;
            }
            match state.head().await {
                Ok(head) => {
                    if let Some(seq) = events::next_hint(state.last_sent, head) {
                        state.last_sent = Some(seq);
                        let hint = SeqHint {
                            root_id: state.root.to_hex(),
                            seq: seq.to_decimal(),
                        };
                        // 提示只带 RootSeq，没有批次内容。序列化固定形状的 DTO 不会失败。
                        let event = Event::default()
                            .event("seq")
                            .id(seq.to_decimal())
                            .json_data(&hint)
                            .ok()?;
                        return Some((Ok::<Event, Infallible>(event), state));
                    }
                }
                Err(detail) => {
                    // 读不到头（Root 消失、存储故障）：结束流，不编造提示。
                    warn!(root = %state.root.to_hex(), error = %detail, "SSE 读取 RootSeq 失败，结束事件流");
                    return None;
                }
            }
            tokio::time::sleep(state.context.config.sse_poll_interval).await;
        }
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(context.config.sse_keepalive))
        .into_response()
}

/// SSE 流的内部状态：上一次提示过的头。
struct HintStream {
    context: Arc<SyncContext>,
    owner: UserId,
    root: RootId,
    last_sent: Option<RootSeq>,
}

impl HintStream {
    async fn head(&self) -> Result<RootSeq, String> {
        let mut conn = self
            .context
            .auth
            .database()
            .pool()
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        store_roots::root_in_scope(&mut conn, self.owner, self.root)
            .await
            .map_err(|error| error.to_string())?
            .map(|record| record.seq)
            .ok_or_else(|| "Root 不在授权范围".to_owned())
    }
}

/// 查询串：只接受声明过的参数名，重复/未知/非规范十进制都是 400。
struct QueryParams<'a>(BTreeMap<&'a str, &'a str>);

impl<'a> QueryParams<'a> {
    fn parse(raw: Option<&'a str>, allowed: &[&str]) -> Result<Self, ChangesFailure> {
        let mut values = BTreeMap::new();
        for pair in raw.unwrap_or("").split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if !allowed.contains(&name) {
                return Err(ChangesFailure::InvalidRequest {
                    detail: format!("不支持的查询参数 `{name}`"),
                });
            }
            if values.insert(name, value).is_some() {
                return Err(ChangesFailure::InvalidRequest {
                    detail: format!("查询参数 `{name}` 重复"),
                });
            }
        }
        Ok(Self(values))
    }

    /// 取规范十进制参数；缺省为 `None`，写法不规范是 400。
    fn decimal(&self, name: &str) -> Result<Option<u64>, ChangesFailure> {
        match self.0.get(name) {
            None => Ok(None),
            Some(text) => {
                parse_decimal(text)
                    .map(Some)
                    .ok_or_else(|| ChangesFailure::InvalidRequest {
                        detail: format!("查询参数 `{name}` 不是规范十进制文本"),
                    })
            }
        }
    }
}

fn wire_entry(entry: &store_snapshots::SnapshotEntry) -> WireEntry {
    WireEntry {
        entry_id: entry.entry.to_hex(),
        parent_id: entry.parent.map(|parent| parent.to_hex()),
        name: entry.name.clone(),
        kind: match entry.kind {
            EntryKind::File => WireKind::File,
            EntryKind::Directory => WireKind::Directory,
        },
        revision: entry.revision.to_decimal(),
        version_id: entry.version.map(|version| version.to_hex()),
        size: entry.size.map(|size| size.to_string()),
        hash: entry.hash.map(hex_lower),
    }
}

fn hex_lower(bytes: [u8; 32]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn snapshot_error_response(request_id: &str, error: &SnapshotError, root: RootId) -> Response {
    match error {
        SnapshotError::Expired => failure_response(
            request_id,
            &ChangesFailure::CursorExpired(CursorProblem::SnapshotExpired),
            root,
        ),
        SnapshotError::OutOfScope => denied(request_id, "Root 不在授权范围"),
        SnapshotError::InvalidLimit | SnapshotError::InvalidCursor => failure_response(
            request_id,
            &ChangesFailure::InvalidRequest {
                detail: error.to_string(),
            },
            root,
        ),
        other => storage_failure(request_id, &other.to_string(), root),
    }
}

/// 核心失败 → 错误信封（状态码、错误码与可重试性都来自核心）；410 带结构化原因。
fn failure_response(request_id: &str, failure: &ChangesFailure, root: RootId) -> Response {
    let status =
        StatusCode::from_u16(failure.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let code = match failure.design_error_code() {
        "CURSOR_EXPIRED" => ErrorCode::CursorExpired,
        "INVALID_REQUEST" => ErrorCode::InvalidRequest,
        _ => ErrorCode::NeedsRecovery,
    };
    let details = match failure {
        ChangesFailure::CursorExpired(problem) => {
            let mut details = BTreeMap::new();
            details.insert("reason".to_owned(), problem.reason().to_owned());
            match problem {
                CursorProblem::BeforeRetention { expired_through } => {
                    details.insert("expired_through".to_owned(), expired_through.to_decimal());
                }
                CursorProblem::AheadOfHead { head } => {
                    details.insert("head_seq".to_owned(), head.to_decimal());
                }
                CursorProblem::SnapshotExpired => {}
            }
            Some(details)
        }
        _ => None,
    };
    let envelope = ErrorEnvelope {
        error: ErrorBody {
            code,
            details,
            message: failure.to_string(),
            request_id: request_id.to_owned(),
            retryable: failure.is_retryable(),
            root_id: Some(root.to_hex()),
        },
    };
    (status, Json(envelope)).into_response()
}

fn storage_failure(request_id: &str, detail: &str, root: RootId) -> Response {
    warn!(request_id = %request_id, error = %detail, "快照/changes 存储读写失败");
    internal_error(request_id, "服务端内部错误", Some(root.to_hex()))
}
