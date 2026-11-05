//! S047：操作端点的 HTTP 级验收（design §15.2—15.4、I01、I06）。
//!
//! 用例走真实 router、真实 SQLite：操作记录用 `server-store::operations` 的真实 API 种子（不是假端口），
//! 断言的是端点实际返回的状态码与响应体，以及端点有没有偷偷写状态（`operations` 行数、`roots.seq`）。
//!
//! 覆盖本卡验收点：
//!
//! - 同一请求重试两次逐字段相同，且不新增操作行、不推进 RootSeq；
//! - 响应丢失（先真实提交为 committed，再用同 OpId 同摘要请求）拿到可辨识的重放结果；
//! - 旧 epoch → 410 `EPOCH_CHANGED`；Root 不可写 → 503 `ROOT_UNAVAILABLE`；
//! - 同 OpId 不同摘要 → 409 `OP_ID_REUSED`；
//! - pending → 202 且不含 `revision`/`root_seq`/`version_id`/`entry_id`（不判已同步）；
//! - 未装配执行器的新命令 → 503 `SERVICE_NOT_READY` 且 `operations` 行数不变。

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vaultsyncd_domain::ids::{
    DeviceId, EntryId, ID_LEN, OpId, RootId, TxId, UploadId, UserId, VersionId,
};
use vaultsyncd_domain::version::{Generation, Revision, ServerEpoch, UtcTimestamp};
use vaultsyncd_protocol::generated::{
    ErrorCode, ErrorEnvelope, LoginRequest, OperationKind, OperationRequest, OperationResult,
    OperationState, SessionIssued,
};
use vaultsyncd_server::auth::AuthContext;
use vaultsyncd_server::content::ContentContext;
use vaultsyncd_server::http::router;
use vaultsyncd_server::lifecycle::Lifecycle;
use vaultsyncd_server::operations::OperationsContext;
use vaultsyncd_server::roots::RootContext;
use vaultsyncd_server::uploads::UploadsContext;
use vaultsyncd_server_core::api::operations::{
    ConflictPolicy, OperationKind as CoreKind, OperationRequest as CoreRequest,
};
use vaultsyncd_server_core::auth::password::PasswordHasher;
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::operations::{self as store_operations, NewOperation};
use vaultsyncd_server_store::roots::{self as store_roots, NewRoot, RootHealth};
use vaultsyncd_server_store::users;

const PASSWORD: &str = "correct horse battery staple";
const EPOCH: u64 = 1;

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("vaultsyncd-s047-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建测试状态目录");
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("server.sqlite")
    }

    fn state_dir(&self) -> PathBuf {
        self.path.join("state")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Seeds {
    owner: UserId,
    root: RootId,
    blocked_root: RootId,
    foreign_root: RootId,
}

/// 种子：owner 有一个可写 Root 与一个不可写 Root（验证 503 `ROOT_UNAVAILABLE`），
/// intruder 有会话但没有 Root（越权 403）。
async fn seed_accounts(fixture: &Fixture) -> Seeds {
    let owner = UserId::from_bytes([0x11; ID_LEN]);
    let intruder = UserId::from_bytes([0x33; ID_LEN]);
    let root = RootId::from_bytes([0x22; ID_LEN]);
    let blocked_root = RootId::from_bytes([0x44; ID_LEN]);
    let foreign_root = RootId::from_bytes([0x66; ID_LEN]);
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let hasher = PasswordHasher::with_params(8, 1, 1).expect("测试 Argon2id 参数");
    let password_hash = hasher.hash(PASSWORD).expect("散列种子密码");
    let mut tx = database.begin_write().await.expect("写事务");
    for (id, login) in [(owner, "owner"), (intruder, "intruder")] {
        users::insert_user(
            &mut tx,
            &users::NewUser {
                id,
                login: login.to_owned(),
                password_hash: password_hash.clone(),
            },
        )
        .await
        .expect("写种子账号");
    }
    for (id, entry, path, account) in [
        (
            root,
            EntryId::from_bytes([0x55; ID_LEN]),
            "/srv/s047",
            owner,
        ),
        (
            blocked_root,
            EntryId::from_bytes([0x77; ID_LEN]),
            "/srv/s047-blocked",
            owner,
        ),
        (
            foreign_root,
            EntryId::from_bytes([0x99; ID_LEN]),
            "/srv/s047-intruder",
            intruder,
        ),
    ] {
        store_roots::insert_root(
            &mut tx,
            &NewRoot {
                id,
                entry,
                owner: account,
                absolute_path: path.to_owned(),
                volume_identity: vec![0x77; 16],
                root_file_identity: vec![0x88; 16],
                epoch: ServerEpoch::new(EPOCH).expect("epoch"),
            },
        )
        .await
        .expect("写种子 Root");
    }
    store_roots::set_health(&mut tx, owner, blocked_root, RootHealth::Unavailable)
        .await
        .expect("标记 Root 不可写");
    tx.commit().await.expect("提交种子事务");
    database.close().await;
    Seeds {
        owner,
        root,
        blocked_root,
        foreign_root,
    }
}

struct App {
    router: Router,
    database: Database,
}

async fn open_app(fixture: &Fixture) -> App {
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let auth = Arc::new(AuthContext::new(database.clone()));
    let uploads = Arc::new(UploadsContext::new(
        Arc::clone(&auth),
        fixture.state_dir().as_path(),
    ));
    let roots = Arc::new(RootContext::new(
        Arc::clone(&auth),
        None,
        fixture.state_dir(),
        Default::default(),
    ));
    let content = Arc::new(ContentContext::new(
        Arc::clone(&auth),
        fixture.state_dir().as_path(),
    ));
    let operations = Arc::new(OperationsContext::new(Arc::clone(&auth)));
    let lifecycle = Arc::new(Lifecycle::new());
    lifecycle.mark_ready();
    let sync = Arc::new(vaultsyncd_server::sync::SyncContext::new(
        Arc::clone(&auth),
        Arc::clone(&lifecycle),
    ));
    App {
        router: router(lifecycle, auth, uploads, roots, content, operations, sync),
        database,
    }
}

impl App {
    async fn call(&self, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("调用处理器");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("读响应体")
            .to_vec();
        (status, body)
    }

    /// `operations` 表的行数（证明端点有没有偷偷登记操作）。
    async fn operation_rows(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM operations")
            .fetch_one(self.database.pool())
            .await
            .expect("统计 operations 行")
    }

    /// Root 当前的 RootSeq（证明端点有没有推进事件批次）。
    async fn root_seq(&self, root: RootId) -> i64 {
        sqlx::query_scalar("SELECT seq FROM roots WHERE id = ?1")
            .bind(root.as_bytes().as_slice())
            .fetch_one(self.database.pool())
            .await
            .expect("读取 roots.seq")
    }
}

fn text(body: &[u8]) -> String {
    String::from_utf8(body.to_vec()).expect("响应体是 UTF-8")
}

fn envelope(body: &[u8]) -> ErrorEnvelope {
    serde_json::from_slice(body)
        .unwrap_or_else(|error| panic!("解析错误信封失败：{error}：{}", text(body)))
}

fn result(body: &[u8]) -> OperationResult {
    serde_json::from_slice(body)
        .unwrap_or_else(|error| panic!("解析操作结果失败：{error}：{}", text(body)))
}

fn send(method: &str, uri: &str, token: Option<&str>, body: Option<String>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("构造请求"),
        None => builder.body(Body::empty()).expect("构造请求"),
    }
}

async fn login(app: &App, account: &str) -> String {
    let request = LoginRequest {
        device_label: "S047 验收".to_owned(),
        login: account.to_owned(),
        password: PASSWORD.to_owned(),
    };
    let body = serde_json::to_string(&request).expect("序列化登录请求");
    let (status, bytes) = app
        .call(send("POST", "/v1/auth/login", None, Some(body)))
        .await;
    assert_eq!(status, StatusCode::OK, "登录应成功：{}", text(&bytes));
    let issued: SessionIssued = serde_json::from_slice(&bytes).expect("解析会话");
    issued.access_token
}

fn device() -> DeviceId {
    DeviceId::from_bytes([0xa1; ID_LEN])
}

fn op() -> OpId {
    OpId::from_bytes([0xb2; ID_LEN])
}

fn entry() -> EntryId {
    EntryId::from_bytes([0xc3; ID_LEN])
}

fn upload() -> UploadId {
    UploadId::from_bytes([0xd4; ID_LEN])
}

fn version() -> VersionId {
    VersionId::from_bytes([0xe5; ID_LEN])
}

fn operations_uri(root: RootId) -> String {
    format!("/v1/roots/{}/operations", root.to_hex())
}

fn operation_uri(root: RootId, op: OpId) -> String {
    format!("/v1/roots/{}/operations/{}", root.to_hex(), op.to_hex())
}

/// 一份合法的 put 命令（wire 形状）。
fn wire_put(epoch: u64, revision: u64) -> OperationRequest {
    OperationRequest {
        op_id: op().to_hex(),
        device_id: device().to_hex(),
        server_epoch: epoch.to_string(),
        kind: OperationKind::PutContent,
        entry_id: Some(entry().to_hex()),
        base_revision: Some(revision.to_string()),
        base_version_id: None,
        upload_id: Some(upload().to_hex()),
        local_generation: Some("8".to_owned()),
        conflict_policy: None,
        parent_id: None,
        name: None,
        expected_absent: None,
        subtree_token: None,
        version_id: None,
    }
}

/// 与 [`wire_put`] 语义相同的核心请求：种子记录必须与端点算出同一个摘要。
fn core_put(revision: u64) -> CoreRequest {
    CoreRequest {
        op: op(),
        device: device(),
        kind: CoreKind::PutContent,
        expected_epoch: ServerEpoch::new(EPOCH).expect("epoch"),
        entry: Some(entry()),
        base_revision: Some(Revision::new(revision).expect("修订")),
        base_version: None,
        upload: Some(upload()),
        local_generation: Some(Generation::new(8).expect("代际")),
        conflict_policy: ConflictPolicy::PreserveBoth,
        parent: None,
        name: None,
        expected_absent: None,
        subtree_token: None,
        version: None,
    }
}

/// 走与端点相同的摘要通路（核心给出语义字段，存储层给出编码）。
fn digest_of(request: &CoreRequest) -> store_operations::RequestDigest {
    let fields = request.digest_fields();
    let pairs: Vec<(&str, &[u8])> = fields
        .iter()
        .map(|(name, value)| (*name, value.as_bytes()))
        .collect();
    store_operations::request_digest(&pairs).expect("计算摘要")
}

fn operation_key(root: RootId) -> store_operations::OperationKey {
    store_operations::OperationKey {
        root,
        device: device(),
        op: op(),
    }
}

/// 真实登记一条 `pending` 操作（等价于“已受理、还没有业务结果”）。
async fn seed_pending(app: &App, seeds: &Seeds) {
    let mut tx = app.database.begin_write().await.expect("写事务");
    let started = store_operations::begin_operation(
        &mut tx,
        seeds.owner,
        &NewOperation {
            key: operation_key(seeds.root),
            digest: digest_of(&core_put(17)),
        },
    )
    .await
    .expect("登记 pending 操作");
    assert_eq!(started, store_operations::BeginOperation::Started);
    tx.commit().await.expect("提交 pending");
}

/// 真实登记一条 `committed` 操作（同一事务里追加事件批次并写入结果，design §11.2 步骤 7）。
async fn seed_committed(app: &App, seeds: &Seeds, result: &OperationResult) {
    let mut tx = app.database.begin_write().await.expect("写事务");
    let started = store_operations::begin_operation(
        &mut tx,
        seeds.owner,
        &NewOperation {
            key: operation_key(seeds.root),
            digest: digest_of(&core_put(17)),
        },
    )
    .await
    .expect("登记 pending 操作");
    assert_eq!(started, store_operations::BeginOperation::Started);
    let result_json = serde_json::to_string(result).expect("序列化结果");
    store_operations::commit_operation(
        &mut tx,
        seeds.owner,
        &operation_key(seeds.root),
        TxId::from_bytes([0xf6; ID_LEN]),
        r#"[{"kind":"test"}]"#,
        &result_json,
        UtcTimestamp::from_unix_seconds(1_760_000_000).expect("时间"),
    )
    .await
    .expect("提交操作结果");
    tx.commit().await.expect("提交 committed");
}

fn recorded_result() -> OperationResult {
    OperationResult {
        op_id: op().to_hex(),
        state: OperationState::Committed,
        root_seq: Some("42".to_owned()),
        entry_id: Some(entry().to_hex()),
        revision: Some("18".to_owned()),
        version_id: Some(version().to_hex()),
        echo_local_generation: Some("8".to_owned()),
        conflict: None,
        replayed: None,
        error_code: None,
        error_message: None,
    }
}

#[tokio::test]
async fn new_command_is_rejected_without_writing_state() {
    let fixture = Fixture::new("executor-missing");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;

    let body = serde_json::to_string(&wire_put(EPOCH, 17)).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{}", text(&bytes));
    let rejected = envelope(&bytes);
    assert_eq!(rejected.error.code, ErrorCode::ServiceNotReady);
    assert!(rejected.error.retryable, "服务端恢复后重试可能成功");
    assert!(
        !rejected.error.request_id.is_empty(),
        "错误必须保留 request_id"
    );
    assert_eq!(
        rejected.error.root_id.as_deref(),
        Some(seeds.root.to_hex().as_str())
    );
    assert!(
        !text(&bytes).contains("\"committed\""),
        "不得伪造已提交：{}",
        text(&bytes)
    );
    assert_eq!(
        app.operation_rows().await,
        0,
        "未被受理的命令不得登记操作行"
    );
    assert_eq!(app.root_seq(seeds.root).await, 0, "不得推进 RootSeq");
}

#[tokio::test]
async fn pending_operation_answers_202_without_sync_fields() {
    let fixture = Fixture::new("pending");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    seed_pending(&app, &seeds).await;
    assert_eq!(app.operation_rows().await, 1);

    let body = serde_json::to_string(&wire_put(EPOCH, 17)).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{}", text(&bytes));
    let replayed = result(&bytes);
    assert_eq!(replayed.state, OperationState::Pending);
    assert_eq!(replayed.replayed, Some(true), "命中已记录的结果");
    assert_eq!(replayed.revision, None, "pending 不得带 revision");
    assert_eq!(replayed.root_seq, None, "pending 不得带 root_seq");
    assert_eq!(replayed.version_id, None, "pending 不得带 version_id");
    assert_eq!(replayed.entry_id, None, "pending 不得带 entry_id");
    assert_eq!(app.operation_rows().await, 1, "重放不得新增操作行");
    assert_eq!(app.root_seq(seeds.root).await, 0, "pending 不占 RootSeq");

    // 同一请求再重试一次：仍是 202，响应逐字段相同。
    let (status, again) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(result(&again), replayed, "同一请求的重试必须逐字段相同");

    // 结果查询与提交端点给同一个结论。
    let (status, bytes) = app
        .call(send(
            "GET",
            &operation_uri(seeds.root, op()),
            Some(&token),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let queried = result(&bytes);
    assert_eq!(queried.state, OperationState::Pending);
    assert_eq!(queried.replayed, None, "查询不是重放");
    assert_eq!(queried.root_seq, None);
}

#[tokio::test]
async fn committed_operation_replays_the_same_result_after_response_loss() {
    let fixture = Fixture::new("committed");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let recorded = recorded_result();
    seed_committed(&app, &seeds, &recorded).await;
    assert_eq!(app.root_seq(seeds.root).await, 1);

    let body = serde_json::to_string(&wire_put(EPOCH, 17)).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{}", text(&bytes));
    let replayed = result(&bytes);
    assert_eq!(replayed.op_id, op().to_hex());
    assert_eq!(replayed.state, OperationState::Committed);
    assert_eq!(replayed.replayed, Some(true), "响应丢失后的重试必须可辨识");
    assert_eq!(replayed.root_seq.as_deref(), Some("42"));
    assert_eq!(replayed.revision.as_deref(), Some("18"));
    assert_eq!(
        replayed.entry_id.as_deref(),
        Some(entry().to_hex().as_str())
    );
    assert_eq!(
        replayed.version_id.as_deref(),
        Some(version().to_hex().as_str())
    );
    assert_eq!(replayed.echo_local_generation.as_deref(), Some("8"));
    assert_eq!(app.operation_rows().await, 1, "重放不得新增操作行");
    assert_eq!(app.root_seq(seeds.root).await, 1, "重放不得推进 RootSeq");

    let (status, bytes) = app
        .call(send(
            "GET",
            &operation_uri(seeds.root, op()),
            Some(&token),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let queried = result(&bytes);
    assert_eq!(queried.state, OperationState::Committed);
    assert_eq!(queried.replayed, None);
    assert_eq!(queried.root_seq.as_deref(), Some("42"));
    assert_eq!(queried.revision.as_deref(), Some("18"));
}

#[tokio::test]
async fn same_op_id_with_different_semantics_is_rejected_as_reused() {
    let fixture = Fixture::new("op-id-reused");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    seed_pending(&app, &seeds).await;

    // 同 op_id/device，但基线 revision 不同 → 不同摘要 → 协议错误。
    let body = serde_json::to_string(&wire_put(EPOCH, 18)).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", text(&bytes));
    let reused = envelope(&bytes);
    assert_eq!(reused.error.code, ErrorCode::OpIdReused);
    assert!(
        !reused.error.retryable,
        "同 ID 不同请求是协议错误，重试无用"
    );
    assert!(!reused.error.request_id.is_empty());
    assert_eq!(app.operation_rows().await, 1, "不得覆盖已记录的操作");
}

#[tokio::test]
async fn stale_epoch_and_unavailable_root_are_rejected_before_the_operation_log() {
    let fixture = Fixture::new("epoch-health");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;

    let body = serde_json::to_string(&wire_put(EPOCH - 1, 17)).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::GONE, "{}", text(&bytes));
    let stale = envelope(&bytes);
    assert_eq!(stale.error.code, ErrorCode::EpochChanged);
    assert!(!stale.error.retryable, "旧 epoch 要求客户端先重建");

    let body = serde_json::to_string(&wire_put(EPOCH, 17)).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.blocked_root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{}", text(&bytes));
    let blocked = envelope(&bytes);
    assert_eq!(blocked.error.code, ErrorCode::RootUnavailable);
    assert!(blocked.error.retryable);
    assert_eq!(app.operation_rows().await, 0, "被拒的请求不得登记操作行");
}

#[tokio::test]
async fn auth_scope_shape_and_unknown_operation_are_reported_distinctly() {
    let fixture = Fixture::new("errors");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let body = serde_json::to_string(&wire_put(EPOCH, 17)).expect("序列化请求");

    // 没有凭据：401。
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            None,
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(envelope(&bytes).error.code, ErrorCode::AuthExpired);

    // 别人的 Root：403（与不存在同一个回答）。
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.foreign_root),
            Some(&token),
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", text(&bytes));
    assert_eq!(envelope(&bytes).error.code, ErrorCode::AccessDenied);

    // 形状不合法：create 缺少 expected_absent=true。
    let mut create = wire_put(EPOCH, 17);
    create.kind = OperationKind::Create;
    create.entry_id = None;
    create.base_revision = None;
    create.upload_id = None;
    create.parent_id = Some(entry().to_hex());
    create.name = Some("报告.docx".to_owned());
    let body = serde_json::to_string(&create).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", text(&bytes));
    assert_eq!(envelope(&bytes).error.code, ErrorCode::InvalidRequest);

    // 身份文本不是规范十六进制。
    let mut malformed = wire_put(EPOCH, 17);
    malformed.op_id = "not-hex".to_owned();
    let body = serde_json::to_string(&malformed).expect("序列化请求");
    let (status, bytes) = app
        .call(send(
            "POST",
            &operations_uri(seeds.root),
            Some(&token),
            Some(body),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(envelope(&bytes).error.code, ErrorCode::InvalidRequest);

    // 查询一个不存在的操作：404，且与“不属于你”同一个回答。
    let unknown = OpId::from_bytes([0xee; ID_LEN]);
    let (status, bytes) = app
        .call(send(
            "GET",
            &operation_uri(seeds.root, unknown),
            Some(&token),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", text(&bytes));
    assert_eq!(envelope(&bytes).error.code, ErrorCode::UnsupportedEntry);

    // 查询他人的 Root：403。
    let (status, _) = app
        .call(send(
            "GET",
            &operation_uri(seeds.foreign_root, unknown),
            Some(&token),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
