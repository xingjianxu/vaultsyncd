//! Root 注册与清单端点：`POST /v1/admin/roots`、`GET /v1/roots`（design §11.4、§15.2）。
//!
//! 本模块只做“HTTP → 探测 → 用例判定 → 存储”的装配，判定在
//! `vaultsyncd_server_core::api::roots`，挂载探测在
//! `vaultsyncd_platform_linux_server`，写库在 `vaultsyncd_server_store::roots`：
//!
//! - **两段允许基目录检查**：先按客户端给的文本判（不在允许基目录内立刻 403，连探测都不做，
//!   否则这个接口就成了任意路径的存在性探针），探测之后再按规范路径判一次，符号链接不能把
//!   Root 带出允许范围。
//! - **配置阶段拒绝用 422 `UNSUPPORTED_STORAGE_BACKEND`**（design §11.4）：FUSE/占位视图、
//!   非 btrfs、NOCOW、嵌套子卷或子挂载、内部状态目录重叠、与已注册 Root 的父子/别名重叠都在
//!   这里拒绝；扫描不完整不是通过。
//! - **注册不改写目录内容**：只读探测（`RootHandle` 的句柄打开与目录遍历）、只写数据库。既有
//!   文件不被导入、不被清空、不被改名；初始采集属后续步骤，响应里报 `pending_import=true`。
//! - **普通客户端看不到绝对路径**：`GET /v1/roots` 只有身份与健康状态；注册响应同样不回显
//!   路径（它已经在请求里，回显只会把服务器布局写进客户端日志）。
//! - **管理权限**：首发只有一位 owner（design §7.2、§17.1，`vaultsyncd-admin init` 只建一个
//!   账号且没有账号注册接口），因此“仅 owner 管理”的落地是“必须有有效会话”。多账号角色
//!   区分需要账号卡先加角色列，本卡不虚构。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::Mutex;
use tracing::{info, warn};
use vaultsyncd_domain::ids::{EntryId, IdSource, RootId, UserId};
use vaultsyncd_domain::version::ServerEpoch;
use vaultsyncd_platform_linux_server::files::restricted::ResolveMode;
use vaultsyncd_platform_linux_server::files::root::{RootHandle, RootSnapshot};
use vaultsyncd_platform_linux_server::mounts::admission::{
    self, AdmissionDecision, AdmissionInput, AdmissionOutcome, fsid_text,
};
use vaultsyncd_platform_linux_server::mounts::mountinfo;
use vaultsyncd_platform_linux_server::mounts::probe::{self, ScanPolicy};
use vaultsyncd_protocol::generated::{
    ErrorBody, ErrorCode, ErrorEnvelope, ManagedRoot, ManagedRootList, RootCollection,
    RootCollectionState, RootHealth, RootList, RootRegisterRequest, RootRegistered, RootSummary,
};
use vaultsyncd_server_core::api::roots::{
    AllowedBases, CandidateFacts, RegistrationRejection, RegistrationVerdict, evaluate_registration,
};
use vaultsyncd_server_store::roots::{self as store_roots, NewRoot, RootHealth as StoreRootHealth};

use crate::auth::{self, AuthContext, RandomIds};

/// Root 端点共享的状态：认证上下文、允许基目录、内部状态目录与扫描上限。
pub struct RootContext {
    auth: Arc<AuthContext>,
    bases: Option<AllowedBases>,
    state_dir: PathBuf,
    scan: ScanPolicy,
    ids: Mutex<RandomIds>,
    observer: Option<Arc<crate::external::ObserverRuntime>>,
}

impl RootContext {
    /// 绑定同一生产观察器；测试或未装配时诚实报告尚未扫描。
    pub fn with_observer(mut self, observer: Arc<crate::external::ObserverRuntime>) -> Self {
        self.observer = Some(observer);
        self
    }

    /// 生产装配。
    #[must_use]
    pub fn new(
        auth: Arc<AuthContext>,
        bases: Option<AllowedBases>,
        state_dir: PathBuf,
        scan: ScanPolicy,
    ) -> Self {
        Self {
            auth,
            bases,
            state_dir,
            scan,
            ids: Mutex::new(RandomIds),
            observer: None,
        }
    }
}

/// 管理通道需唯一 active owner；有效普通会话不能读取服务器布局。
async fn require_owner(context: &RootContext, owner: UserId) -> Result<(), Box<Response>> {
    let owners: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT id FROM users WHERE status='active' LIMIT 2")
            .fetch_all(context.auth.database().pool())
            .await
            .map_err(|error| Box::new(storage_failure(&next_request_id(), &error.to_string())))?;
    if owners.len() != 1 || owners[0].as_slice() != owner.as_bytes() {
        return Err(Box::new(rejected(
            &next_request_id(),
            &RegistrationRejection::OutsideAllowedBases,
            "路径管理仅允许唯一 active owner；请由管理员核对账号配置。",
        )));
    }
    Ok(())
}

/// 未取得本进程完整扫描证据时禁止显示采集成功。
pub(crate) fn not_scanned() -> RootCollection {
    RootCollection {
        state: RootCollectionState::NotScanned,
        examined: "0".into(),
        pending: "0".into(),
        detail: "本进程尚未扫描；不能据此判断目录为空或已同步。".into(),
    }
}

/// `GET /v1/admin/roots`：owner 配置、现场挂载核对和真实采集进度。
pub async fn list_managed_roots(
    State(context): State<Arc<RootContext>>,
    headers: HeaderMap,
) -> Response {
    let principal = match auth::principal_from(&context.auth, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if let Err(response) = require_owner(&context, principal.user_id).await {
        return *response;
    }
    let records = match context.auth.database().pool().acquire().await {
        Ok(mut conn) => match store_roots::roots_of_owner(&mut conn, principal.user_id).await {
            Ok(records) => records,
            Err(error) => return storage_failure(&next_request_id(), &error.to_string()),
        },
        Err(error) => return storage_failure(&next_request_id(), &error.to_string()),
    };
    let mut roots = Vec::new();
    for record in records {
        let path = PathBuf::from(&record.absolute_path);
        let identity = record.root_file_identity.clone();
        let reason = match tokio::task::spawn_blocking(move || {
            let expected = std::str::from_utf8(&identity)
                .ok()
                .and_then(|text| RootSnapshot::parse_spec(text).ok());
            match expected {
                Some(expected) => {
                    vaultsyncd_platform_linux_server::mounts::health::verify_root_mount(
                        &path, &expected,
                    )
                    .err()
                    .map(|error| error.detail_code().to_owned())
                }
                None => Some("invalid_root_identity".to_owned()),
            }
        })
        .await
        {
            Ok(reason) => reason,
            Err(error) => return storage_failure(&next_request_id(), &error.to_string()),
        };
        let reason = reason.unwrap_or_else(|| {
            if record.health == StoreRootHealth::Unavailable {
                "stored_unavailable".into()
            } else {
                String::new()
            }
        });
        let mut collection = match &context.observer {
            Some(observer) => observer.collection_status(record.id).await,
            None => not_scanned(),
        };
        if !reason.is_empty() {
            collection.state = RootCollectionState::Blocked;
            collection.detail = "Root 不可用，已暂停采集；不推导删除。".into();
        }
        roots.push(ManagedRoot {
            root_id: record.id.to_hex(),
            path: record.absolute_path,
            health: if reason.is_empty() {
                RootHealth::Healthy
            } else {
                RootHealth::Unavailable
            },
            next_step: if reason.is_empty() {
                "等待稳定采集；可刷新查看本进程进度。".into()
            } else {
                "恢复原挂载、目录身份与服务账号读取权限，再刷新核验；不要删除或重新绑定来掩盖故障。"
                    .into()
            },
            reason,
            root_seq: record.seq.get().to_string(),
            collection,
        });
    }
    let allowed_bases = context
        .bases
        .as_ref()
        .map(|bases| {
            bases
                .paths()
                .map(|path| path.display().to_string())
                .collect()
        })
        .unwrap_or_default();
    let mut response = (
        StatusCode::OK,
        Json(ManagedRootList {
            allowed_bases,
            roots,
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().expect("static header"));
    response
}

/// `GET /v1/roots`：当前调用者可见的 Root 与健康状态，不含绝对路径。
pub async fn list_roots(State(context): State<Arc<RootContext>>, headers: HeaderMap) -> Response {
    let principal = match auth::principal_from(&context.auth, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    list_for_owner(context, principal.user_id, None).await
}

/// HTTP 和已验证 UID 的本机管理共用同一 Root 清单用例。
pub(crate) async fn list_for_owner(
    context: Arc<RootContext>,
    owner: UserId,
    page: Option<u32>,
) -> Response {
    let mut conn = match context.auth.database().pool().acquire().await {
        Ok(conn) => conn,
        Err(error) => return storage_failure(&next_request_id(), &error.to_string()),
    };
    let records = match match page {
        Some(page) => store_roots::roots_of_owner_page(&mut conn, owner, page).await,
        None => store_roots::roots_of_owner(&mut conn, owner).await,
    } {
        Ok(records) => records,
        Err(error) => return storage_failure(&next_request_id(), &error.to_string()),
    };
    let has_more = page.is_some() && records.len() > 100;
    let mut roots: Vec<RootSummary> = records
        .into_iter()
        .map(|record| RootSummary {
            root_id: record.id.to_hex(),
            health: map_health(record.health),
        })
        .collect();
    // 按身份排序：库里的 `ORDER BY absolute_path` 是为运维可读，对外换成身份顺序，避免清单
    // 顺序泄露路径相对位置。
    roots.sort_by(|left, right| left.root_id.cmp(&right.root_id));
    if let Some(page) = page {
        roots.truncate(100);
        return (StatusCode::OK, Json(serde_json::json!({"roots": roots, "next_page": if has_more { Some(page + 1) } else { None }}))).into_response();
    }
    (StatusCode::OK, Json(RootList { roots })).into_response()
}

/// `POST /v1/admin/roots`：校验允许基目录与存储准入后注册。
pub async fn register_root(
    State(context): State<Arc<RootContext>>,
    headers: HeaderMap,
    Json(body): Json<RootRegisterRequest>,
) -> Response {
    let principal = match auth::principal_from(&context.auth, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if let Err(response) = require_owner(&context, principal.user_id).await {
        return *response;
    }
    register_for_owner(context, principal.user_id, body).await
}

/// HTTP 与本机管理共用准入探测和 Root 注册事务，不绕过存储校验。
pub(crate) async fn register_for_owner(
    context: Arc<RootContext>,
    owner: UserId,
    body: RootRegisterRequest,
) -> Response {
    let _objects = context.auth.database().object_activity().await;
    let request_id = next_request_id();
    let Some(bases) = context.bases.as_ref() else {
        return rejected(
            &request_id,
            &RegistrationRejection::OutsideAllowedBases,
            "服务端没有配置允许的 Root 基目录",
        );
    };
    let given = PathBuf::from(&body.path);
    if !bases.contains(&given) {
        // 不探测：越界的路径连是否存在都不该回答（否则这里成了任意路径探针）。
        return rejected(
            &request_id,
            &RegistrationRejection::OutsideAllowedBases,
            "路径不在服务端预配置的允许基目录内",
        );
    }

    // 已注册 Root 的路径：重叠与别名判定要用它们的探测事实，而不是只比路径文本。
    let mut conn = match context.auth.database().pool().acquire().await {
        Ok(conn) => conn,
        Err(error) => return storage_failure(&request_id, &error.to_string()),
    };
    let registered = match store_roots::roots_of_owner(&mut conn, owner).await {
        Ok(records) => records
            .into_iter()
            .map(|record| PathBuf::from(record.absolute_path))
            .collect::<Vec<_>>(),
        Err(error) => return storage_failure(&request_id, &error.to_string()),
    };
    drop(conn);

    let state_dir = context.state_dir.clone();
    let scan = context.scan;
    let probe = tokio::task::spawn_blocking(move || {
        probe_candidate(&given, &state_dir, &registered, &scan)
    })
    .await;
    let probe = match probe {
        Ok(probe) => probe,
        Err(error) => {
            warn!(request_id = %request_id, error = %error, "Root 探测任务失败");
            return storage_failure(&request_id, &error.to_string());
        }
    };

    let (resolved, facts) = probe;
    let verdict = evaluate_registration(bases, &resolved, facts);
    let (volume_identity, root_identity) = match verdict {
        RegistrationVerdict::Accepted {
            volume_identity,
            root_identity,
        } => (volume_identity, root_identity),
        RegistrationVerdict::Rejected(rejection) => {
            info!(
                request_id = %request_id,
                user_id = %owner,
                reason = rejection.reason(),
                "Root 注册被拒绝"
            );
            return rejected(&request_id, &rejection, rejection.detail());
        }
    };

    let absolute_path = resolved.display().to_string();
    let mut ids = context.ids.lock().await;
    let new = NewRoot {
        id: RootId::from_bytes(ids.next_id_bytes()),
        entry: EntryId::from_bytes(ids.next_id_bytes()),
        owner,
        absolute_path: absolute_path.clone(),
        volume_identity,
        root_file_identity: root_identity,
        // 服务端 epoch 由恢复流程推进（design §11.3）；当前进程还没有独立的 epoch 状态，
        // 注册写数据库的零值，避免与未来 capabilities 端点给出两个不同来源的 epoch。
        epoch: ServerEpoch::ZERO,
    };
    drop(ids);

    let mut tx = match context.auth.database().begin_write().await {
        Ok(tx) => tx,
        Err(error) => return storage_failure(&request_id, &error.to_string()),
    };
    let record = match store_roots::insert_root(&mut tx, &new).await {
        Ok(record) => record,
        Err(error) => {
            warn!(request_id = %request_id, error = %error, "写入 Root 失败");
            return storage_failure(&request_id, &error.to_string());
        }
    };
    if let Err(error) = tx.commit().await {
        return storage_failure(&request_id, &error.to_string());
    }
    info!(
        request_id = %request_id,
        user_id = %owner,
        root_id = %record.id,
        path = %absolute_path,
        "Root 已注册（既有文件未改动，待初始采集）"
    );
    let payload = RootRegistered {
        root_id: record.id.to_hex(),
        health: map_health(record.health),
        // 注册只读探测、不导入既有内容：初始采集（后续步骤）完成后才不再是待导入。
        pending_import: true,
    };
    (StatusCode::OK, Json(payload)).into_response()
}

/// 阻塞探测：打开 Root 句柄、扫描目录、把候选与内部目录/已注册 Root 一起做准入判定。
fn probe_candidate(
    given: &Path,
    state_dir: &Path,
    registered: &[PathBuf],
    scan: &ScanPolicy,
) -> (PathBuf, CandidateFacts) {
    let handle = match RootHandle::open(given, ResolveMode::Auto) {
        Ok(handle) => handle,
        Err(error) => {
            return (
                given.to_path_buf(),
                CandidateFacts::NotADirectory {
                    reason: error.code().to_owned(),
                    detail: error.to_string(),
                },
            );
        }
    };
    let facts = handle.facts().clone();
    let resolved = handle.path().to_path_buf();
    let subvolume_id = match admission::storage_identity(&facts) {
        Some(identity) => identity.subvolume_id,
        None => {
            // 准入判定会给出同一个结论；这里先返回拒绝，避免在没有子卷 ID 时扫描。
            return (
                resolved,
                CandidateFacts::UnsupportedStorage {
                    reason: "subvolume_identity_unavailable".to_owned(),
                    detail: "btrfs 上无法读取 FSID 或子卷 ID".to_owned(),
                },
            );
        }
    };
    let report = match probe::scan_root(&resolved, subvolume_id, scan) {
        Ok(report) => report,
        Err(error) => {
            return (
                resolved,
                CandidateFacts::NotADirectory {
                    reason: "scan_failed".to_owned(),
                    detail: error.to_string(),
                },
            );
        }
    };
    let internal: Vec<_> = match probe::probe_path(state_dir) {
        Ok(facts) => vec![facts],
        Err(error) => {
            return (
                resolved,
                CandidateFacts::Inconclusive {
                    reason: "internal_state_unreadable".to_owned(),
                    detail: format!("内部状态目录 {} 不可读：{error}", state_dir.display()),
                },
            );
        }
    };
    let registered: Vec<_> = registered
        .iter()
        .filter_map(|path| probe::probe_path(path).ok())
        .collect();
    let mounts = match mountinfo::read_self() {
        Ok(mounts) => mounts,
        Err(error) => {
            return (
                resolved,
                CandidateFacts::Inconclusive {
                    reason: "mountinfo_unreadable".to_owned(),
                    detail: format!("/proc/self/mountinfo 不可读：{error}"),
                },
            );
        }
    };
    let outcome = admission::evaluate(&AdmissionInput {
        root: &facts,
        internal_state_dirs: &internal,
        registered_roots: &registered,
        mounts: &mounts,
        scan: &report,
    });
    (resolved, candidate_facts(&facts, outcome))
}

/// 把平台准入结论翻译成核心的候选事实（身份字节在这里编码，之后原样入库）。
fn candidate_facts(facts: &probe::StorageFacts, outcome: AdmissionOutcome) -> CandidateFacts {
    match outcome.decision {
        AdmissionDecision::Accepted => {
            let volume_identity = match admission::storage_identity(facts) {
                Some(identity) => fsid_text(&identity.fsid).into_bytes(),
                None => Vec::new(),
            };
            CandidateFacts::Accepted {
                volume_identity,
                root_identity: RootSnapshot::of_facts(facts).to_spec().into_bytes(),
            }
        }
        AdmissionDecision::Rejected(rejection) => CandidateFacts::UnsupportedStorage {
            reason: rejection.code().to_owned(),
            detail: rejection.detail(),
        },
        AdmissionDecision::Inconclusive(reason) => CandidateFacts::Inconclusive {
            reason: reason.code().to_owned(),
            detail: outcome.notes.join("; "),
        },
    }
}

fn map_health(health: StoreRootHealth) -> RootHealth {
    match health {
        StoreRootHealth::Healthy => RootHealth::Healthy,
        StoreRootHealth::Unavailable => RootHealth::Unavailable,
    }
}

fn rejected(request_id: &str, rejection: &RegistrationRejection, message: &str) -> Response {
    let status =
        StatusCode::from_u16(rejection.http_status()).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY);
    let code = match rejection.design_error_code() {
        "ACCESS_DENIED" => ErrorCode::AccessDenied,
        _ => ErrorCode::UnsupportedStorageBackend,
    };
    let mut details = std::collections::BTreeMap::new();
    details.insert("reason".to_owned(), rejection.reason().to_owned());
    let envelope = ErrorEnvelope {
        error: ErrorBody {
            code,
            details: Some(details),
            message: message.to_owned(),
            request_id: request_id.to_owned(),
            retryable: rejection.retryable(),
            root_id: None,
        },
    };
    (status, Json(envelope)).into_response()
}

fn storage_failure(request_id: &str, detail: &str) -> Response {
    warn!(request_id = %request_id, error = %detail, "Root 端点的存储操作失败");
    let envelope = ErrorEnvelope {
        error: ErrorBody {
            code: ErrorCode::NeedsRecovery,
            details: None,
            message: "服务端内部错误".to_owned(),
            request_id: request_id.to_owned(),
            retryable: true,
            root_id: None,
        },
    };
    (StatusCode::SERVICE_UNAVAILABLE, Json(envelope)).into_response()
}

fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("roots-{}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, header};
    use axum::routing::{get, post};
    use tower::ServiceExt;
    use vaultsyncd_domain::ids::UserId;
    use vaultsyncd_server_core::auth::password::PasswordHasher;
    use vaultsyncd_server_store::users;

    const PASSWORD: &str = "correct horse battery staple";

    struct Fixture {
        dir: PathBuf,
        bases: Option<AllowedBases>,
        auth: Arc<AuthContext>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// 建一个含 owner 账号的临时库；允许基目录由调用方随后设置。
    async fn fixture(label: &str) -> Fixture {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "vaultsyncd-s044-roots-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let database = crate::auth::test_database(&dir.join("server.sqlite")).await;
        let mut tx = database.begin_write().await.expect("写事务");
        let hash = PasswordHasher::production().hash(PASSWORD).expect("散列");
        users::insert_user(
            &mut tx,
            &users::NewUser {
                id: UserId::from_bytes([0x22; 16]),
                login: "owner".to_owned(),
                password_hash: hash,
            },
        )
        .await
        .expect("写 owner");
        tx.commit().await.expect("提交");
        Fixture {
            dir,
            bases: None,
            auth: Arc::new(AuthContext::new(database)),
        }
    }

    /// 认证端点的路由（登录只认 `AuthContext` 状态，与 Root 端点分开组装）。
    fn auth_router(fixture: &Fixture) -> axum::Router {
        axum::Router::new()
            .route("/v1/auth/login", post(auth::login))
            .with_state(Arc::clone(&fixture.auth))
    }

    fn roots_router(fixture: &Fixture) -> axum::Router {
        let context = Arc::new(RootContext::new(
            Arc::clone(&fixture.auth),
            fixture.bases.clone(),
            fixture.dir.join("state"),
            ScanPolicy::default(),
        ));
        axum::Router::new()
            .route("/v1/roots", get(list_roots))
            .route(
                "/v1/admin/roots",
                get(list_managed_roots).post(register_root),
            )
            .with_state(context)
    }

    async fn call(router: axum::Router, request: Request<Body>) -> (StatusCode, String) {
        let response = router.oneshot(request).await.expect("调用处理器");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("读响应体");
        (status, String::from_utf8(bytes.to_vec()).expect("UTF-8"))
    }

    async fn login_token(fixture: &Fixture) -> String {
        let (status, body) = call(
            auth_router(fixture),
            Request::builder()
                .method("POST")
                .uri("/v1/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    "{{\"login\":\"owner\",\"password\":\"{PASSWORD}\",\"device_label\":\"tests\"}}"
                )))
                .expect("构造请求"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.split("\"access_token\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("取访问凭据")
            .to_owned()
    }

    fn register_request(token: &str, path: &Path) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/admin/roots")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(format!("{{\"path\":\"{}\"}}", path.display())))
            .expect("构造请求")
    }

    fn list_request(token: &str) -> Request<Body> {
        Request::builder()
            .uri("/v1/roots")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("构造请求")
    }

    async fn root_rows(fixture: &Fixture) -> usize {
        let mut conn = fixture
            .auth
            .database()
            .pool()
            .acquire()
            .await
            .expect("取连接");
        store_roots::roots_of_owner(&mut conn, UserId::from_bytes([0x22; 16]))
            .await
            .expect("读 Root")
            .len()
    }

    #[tokio::test]
    async fn roots_list_requires_a_session_and_never_returns_paths() {
        let fixture = fixture("list").await;
        let (status, body) = call(
            roots_router(&fixture),
            Request::builder()
                .uri("/v1/roots")
                .body(Body::empty())
                .expect("构造请求"),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

        let token = login_token(&fixture).await;
        let (status, body) = call(roots_router(&fixture), list_request(&token)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, "{\"roots\":[]}", "没有 Root 时给出空清单");
        assert!(!body.contains(&fixture.dir.display().to_string()), "{body}");
    }

    #[tokio::test]
    async fn management_paths_require_unique_owner_and_unknown_is_not_complete() {
        let mut fixture = fixture("managed").await;
        fixture.bases = Some(AllowedBases::new([fixture.dir.clone()]).unwrap());
        let request = || {
            Request::builder()
                .uri("/v1/admin/roots")
                .body(Body::empty())
                .unwrap()
        };
        let (status, _) = call(roots_router(&fixture), request()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let token = login_token(&fixture).await;
        let managed = || {
            Request::builder()
                .uri("/v1/admin/roots")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap()
        };
        let (status, body) = call(roots_router(&fixture), managed()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains(&fixture.dir.display().to_string()));
        let (_, body) = call(roots_router(&fixture), list_request(&token)).await;
        assert!(!body.contains(&fixture.dir.display().to_string()));
        let unknown = not_scanned();
        assert_eq!(unknown.state, RootCollectionState::NotScanned);
        assert!(unknown.detail.contains("不能"));
        let mut tx = fixture.auth.database().begin_write().await.unwrap();
        users::insert_user(
            &mut tx,
            &users::NewUser {
                id: UserId::from_bytes([0x33; 16]),
                login: "other".into(),
                password_hash: PasswordHasher::production().hash(PASSWORD).unwrap(),
            },
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let (status, body) = call(roots_router(&fixture), managed()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(!body.contains(&fixture.dir.display().to_string()));
        let (status, _) = call(
            roots_router(&fixture),
            register_request(&token, &fixture.dir),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(root_rows(&fixture).await, 0);
    }

    #[tokio::test]
    async fn path_outside_allowed_bases_is_forbidden_and_writes_nothing() {
        let mut fixture = fixture("outside").await;
        fixture.bases = Some(AllowedBases::new([PathBuf::from("/data/roots")]).expect("基目录"));
        let token = login_token(&fixture).await;
        let (status, body) = call(
            roots_router(&fixture),
            register_request(&token, Path::new("/etc")),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("ACCESS_DENIED"), "{body}");
        assert!(body.contains("outside_allowed_bases"), "{body}");
        assert!(
            !body.contains("/data/roots"),
            "拒绝理由不得回显允许基目录：{body}"
        );
        assert_eq!(root_rows(&fixture).await, 0, "被拒绝的注册不能留下 Root 行");
    }

    #[tokio::test]
    async fn without_configured_bases_every_registration_is_refused() {
        let fixture = fixture("unconfigured").await;
        let token = login_token(&fixture).await;
        let (status, body) = call(
            roots_router(&fixture),
            register_request(&token, &fixture.dir),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("ACCESS_DENIED"), "{body}");
        assert_eq!(root_rows(&fixture).await, 0);
    }

    #[tokio::test]
    async fn a_file_inside_the_base_is_rejected_and_its_bytes_are_untouched() {
        let mut fixture = fixture("file").await;
        fixture.bases = Some(AllowedBases::new([fixture.dir.clone()]).expect("基目录"));
        std::fs::create_dir_all(fixture.dir.join("state")).expect("内部目录");
        let sample = fixture.dir.join("not-a-directory.txt");
        std::fs::write(&sample, b"untouched").expect("写样本");
        let token = login_token(&fixture).await;
        let (status, body) = call(roots_router(&fixture), register_request(&token, &sample)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("UNSUPPORTED_STORAGE_BACKEND"), "{body}");
        assert!(body.contains("probe_failed"), "{body}");
        assert!(body.contains("不是目录"), "{body}");
        assert_eq!(std::fs::read(&sample).expect("读样本"), b"untouched");
        assert_eq!(root_rows(&fixture).await, 0, "探测失败不能留下 Root 行");
    }
}
