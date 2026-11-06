//! 上传三端点的 HTTP 级验收（S045-b）：真实 router、真实 SQLite、真实暂存目录，用
//! `tower::oneshot` 直接调用路由器——不替换处理器、不伪造端口与连接。
//!
//! 补齐 `agent/tasks/S045.md` §4 第 1 条要求的 HTTP 证据（核心层用假端口覆盖过同一批规则，
//! 但那只证明用例规则，不证明 HTTP 层的状态码、错误码与“拒绝后没有残留”）：
//!
//! 1. `begin → put_part → status` 正常链路：status 报服务端真实已收块与缺块区间，且**重连**
//!    （丢弃 app、数据库句柄与连接池，从同一批文件重新装配）后拿到完全一致的已收块；
//! 2. 越界块索引 → 400 `INVALID_REQUEST`，不产生块行、不写暂存文件；
//! 3. 超过上限的载荷 → 实测两种真实返回码：超出该块应有长度 400 `INVALID_REQUEST`、
//!    超过 Axum 默认请求体上限 413，两者都不落盘；
//! 4. 块字节与声明摘要不符 → 422 `HASH_MISMATCH`，不登记块；
//! 5. 跨会话（A 会话创建的上传用 B 的登录会话写块）→ 404，B 的进度不变；
//! 6. 回归：无凭据 401 `AUTH_EXPIRED`、别人的 Root 403 `ACCESS_DENIED`。
//!
//! 种子沿用 `tests/process.rs` 的做法：直接开真库，用 `server-store` 的公开接口写 user 与 root；
//! 访问凭据走真实 `POST /v1/auth/login`（不手写会话行，否则测不到认证链）。种子密码散列用
//! 小参数 Argon2id 只影响写入耗时——校验读的是 PHC 文本里的参数，登录路径仍是生产校验器。

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use vaultsyncd_domain::ids::{EntryId, ID_LEN, RootId, UploadId, UserId};
use vaultsyncd_domain::version::ServerEpoch;
use vaultsyncd_protocol::generated::{
    ErrorCode, ErrorEnvelope, LoginRequest, SessionIssued, UploadPartAck, UploadRequest,
    UploadState, UploadStatus,
};
use vaultsyncd_server::auth::AuthContext;
use vaultsyncd_server::content::ContentContext;
use vaultsyncd_server::http::router;
use vaultsyncd_server::lifecycle::Lifecycle;
use vaultsyncd_server::roots::RootContext;
use vaultsyncd_server::uploads::{PART_HASH_HEADER, UploadsContext};
use vaultsyncd_server_core::auth::password::PasswordHasher;
use vaultsyncd_server_core::objects::ContentDigest;
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::roots::{self, NewRoot};
use vaultsyncd_server_store::uploads as store_uploads;
use vaultsyncd_server_store::users;
use vaultsyncd_transfer::hash;

/// 种子密码：只在本文件的登录请求里出现。
const PASSWORD: &str = "correct horse battery staple";

/// 一个用例独占的临时状态目录；`Drop` 时删除（只写临时目录，不碰真实实例）。
struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("vaultsyncd-s045b-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建测试状态目录");
        Self { path }
    }

    fn database(&self) -> PathBuf {
        self.path.join("server.sqlite")
    }

    /// 上传暂存目录的父目录（`UploadsContext` 在其下建 `uploads/`）。
    fn state_dir(&self) -> PathBuf {
        self.path.join("state")
    }

    fn uploads_dir(&self) -> PathBuf {
        self.state_dir().join("uploads")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 种子身份：owner 与一个无关账号（用来验证跨 Root 授权）。
struct Seeds {
    owner: UserId,
    root: RootId,
}

async fn seed_accounts(fixture: &Fixture) -> Seeds {
    let owner = UserId::from_bytes([0x11; ID_LEN]);
    let intruder = UserId::from_bytes([0x33; ID_LEN]);
    let root = RootId::from_bytes([0x22; ID_LEN]);
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
    // 只给 owner 一个 Root：intruder 账号有会话但没有 Root，用来看跨账号访问的 403。
    roots::insert_root(
        &mut tx,
        &NewRoot {
            id: root,
            entry: EntryId::from_bytes([0x55; ID_LEN]),
            owner,
            absolute_path: "/srv/vaultsyncd-s045b-owner".to_owned(),
            volume_identity: vec![0x77; 16],
            root_file_identity: vec![0x88; 16],
            epoch: ServerEpoch::new(1).expect("epoch"),
        },
    )
    .await
    .expect("写种子 Root");
    tx.commit().await.expect("提交种子事务");
    database.close().await;

    Seeds { owner, root }
}

/// 进程内装配：与 `bootstrap` 一样把同一份数据库与状态目录交给三个上下文，然后组装真实 router。
struct App {
    router: Router,
}

async fn open_app(fixture: &Fixture) -> App {
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let auth = Arc::new(AuthContext::new(database));
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
    let lifecycle = Arc::new(Lifecycle::new());
    lifecycle.mark_ready();
    let executor = Arc::new(vaultsyncd_server::executor::ExecutorContext::new(
        auth.database().clone(),
        fixture.state_dir().as_path(),
    ));
    let operations = Arc::new(vaultsyncd_server::operations::OperationsContext::new(
        Arc::clone(&auth),
        executor,
    ));
    let sync = Arc::new(vaultsyncd_server::sync::SyncContext::new(
        Arc::clone(&auth),
        Arc::clone(&lifecycle),
    ));
    App {
        router: router(
            lifecycle,
            Arc::clone(&auth),
            uploads,
            roots,
            content,
            operations,
            sync,
        ),
    }
}

impl App {
    async fn call(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("调用处理器");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("读响应体");
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("响应体是 UTF-8"),
        )
    }
}

fn parse<T: serde::de::DeserializeOwned>(body: &str, what: &str) -> T {
    serde_json::from_str(body).unwrap_or_else(|error| panic!("解析 {what} 失败：{error}：{body}"))
}

fn error_envelope(body: &str) -> ErrorEnvelope {
    parse(body, "错误信封")
}

fn post_json(uri: &str, token: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body.to_owned())).expect("构造请求")
}

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("构造请求")
}

/// 一次块上传：`declared` 是请求头里声明的块摘要（故意可错，用来验证 `HASH_MISMATCH`）。
fn put_part_request(
    root: RootId,
    upload: &str,
    index: u64,
    token: &str,
    declared: &str,
    bytes: &[u8],
) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!(
            "/v1/roots/{}/uploads/{upload}/parts/{index}",
            root.to_hex()
        ))
        .header("authorization", format!("Bearer {token}"))
        .header(PART_HASH_HEADER, declared)
        .body(Body::from(bytes.to_vec()))
        .expect("构造请求")
}

fn sha256(bytes: &[u8]) -> String {
    ContentDigest::from_bytes(hash::digest_of(bytes)).to_hex()
}

async fn login(app: &App, account: &str) -> String {
    let request = LoginRequest {
        device_label: "S045-b 验收".to_owned(),
        login: account.to_owned(),
        password: PASSWORD.to_owned(),
    };
    let body = serde_json::to_string(&request).expect("序列化登录请求");
    let (status, body) = app.call(post_json("/v1/auth/login", None, &body)).await;
    assert_eq!(status, StatusCode::OK, "登录应成功：{body}");
    parse::<SessionIssued>(&body, "SessionIssued").access_token
}

async fn begin(
    app: &App,
    root: RootId,
    token: &str,
    total_size: u64,
    chunk_size: u64,
    content_hash: &str,
) -> vaultsyncd_protocol::generated::UploadSession {
    let request = UploadRequest {
        chunk_size: chunk_size.to_string(),
        hash: content_hash.to_owned(),
        total_size: total_size.to_string(),
    };
    let body = serde_json::to_string(&request).expect("序列化创建请求");
    let (status, body) = app
        .call(post_json(
            &format!("/v1/roots/{}/uploads", root.to_hex()),
            Some(token),
            &body,
        ))
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "创建上传会话应返回 201：{body}"
    );
    parse(&body, "UploadSession")
}

async fn status(app: &App, root: RootId, upload: &str, token: &str) -> UploadStatus {
    let (status, body) = app
        .call(get(
            &format!("/v1/roots/{}/uploads/{upload}", root.to_hex()),
            Some(token),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "查询上传状态应成功：{body}");
    parse(&body, "UploadStatus")
}

/// 块行与暂存文件：拒绝路径必须两边都没有残留。
async fn stored_part_indices(fixture: &Fixture, owner: UserId, upload: &str) -> Vec<u64> {
    let upload_id = UploadId::from_hex(upload).expect("上传会话身份");
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库核对块行");
    let mut conn = database.pool().acquire().await.expect("取连接");
    let parts = store_uploads::parts_of(&mut conn, owner, upload_id)
        .await
        .expect("读块行");
    drop(conn);
    database.close().await;
    parts.into_iter().map(|part| part.index).collect()
}

fn staging_files(fixture: &Fixture, upload: &str) -> Vec<String> {
    let directory = fixture.uploads_dir().join(upload);
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn begin_put_part_and_status_report_real_parts_and_survive_reconnect() {
    let fixture = Fixture::new("happy-path");
    let seeds = seed_accounts(&fixture).await;
    let content = b"0123456789";
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;

    let session = begin(&app, seeds.root, &token, 10, 4, &sha256(content)).await;
    assert_eq!(session.state, UploadState::Open);
    assert_eq!(session.chunk_size, "4");
    assert_eq!(session.part_count, "3");
    assert_eq!(session.total_size, "10");
    assert_eq!(session.hash, sha256(content));
    assert_eq!(session.root_id, seeds.root.to_hex());

    // 块 0：4 字节；块 2：2 字节（末块短，几何按 ceil 给出）。
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            0,
            &token,
            &sha256(&content[0..4]),
            &content[0..4],
        ))
        .await;
    assert_eq!(status_code, StatusCode::OK, "首个块应被接收：{body}");
    let ack: UploadPartAck = parse(&body, "UploadPartAck");
    assert_eq!(ack.index, "0");
    assert_eq!(ack.length, "4");
    assert_eq!(ack.hash, sha256(&content[0..4]));
    assert_eq!(ack.state, UploadState::Open);
    assert!(!ack.already_present);

    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            2,
            &token,
            &sha256(&content[8..10]),
            &content[8..10],
        ))
        .await;
    assert_eq!(status_code, StatusCode::OK, "末块应被接收：{body}");
    let ack: UploadPartAck = parse(&body, "UploadPartAck");
    assert_eq!(ack.length, "2");

    // 服务端真实已收块：索引 0 与 2，缺块区间恰好是 [1, 2)。
    let reported = status(&app, seeds.root, &session.upload_id, &token).await;
    let received: Vec<(String, String)> = reported
        .received
        .iter()
        .map(|part| (part.index.clone(), part.length.clone()))
        .collect();
    assert_eq!(
        received,
        vec![
            ("0".to_owned(), "4".to_owned()),
            ("2".to_owned(), "2".to_owned())
        ],
        "status 必须报真实已收块"
    );
    assert_eq!(reported.received[0].hash, sha256(&content[0..4]));
    assert_eq!(reported.received[1].hash, sha256(&content[8..10]));
    let missing: Vec<(String, String)> = reported
        .missing
        .iter()
        .map(|range| (range.first_index.clone(), range.count.clone()))
        .collect();
    assert_eq!(
        missing,
        vec![("1".to_owned(), "1".to_owned())],
        "缺块区间必须由服务端计算"
    );
    assert_eq!(reported.upload.state, UploadState::Open);
    assert_eq!(
        stored_part_indices(&fixture, seeds.owner, &session.upload_id).await,
        vec![0, 2],
        "收到的块必须真的登记成块行"
    );

    // 重连：丢掉 app（以及它持有的数据库句柄与连接池），从同一批文件重新装配后复查。
    drop(app);
    let app = open_app(&fixture).await;
    let reconnected = status(&app, seeds.root, &session.upload_id, &token).await;
    assert_eq!(
        reconnected, reported,
        "重连后服务端给出的已收块、缺块与会话信息必须完全一致"
    );
}

#[tokio::test]
async fn out_of_range_part_index_is_rejected_without_creating_a_part_row() {
    let fixture = Fixture::new("index-out-of-range");
    let seeds = seed_accounts(&fixture).await;
    let content = b"0123456789";
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let session = begin(&app, seeds.root, &token, 10, 4, &sha256(content)).await;

    // 块数是 3，索引 3 越界：载荷本身合法（长度与摘要都对），只有索引越界。
    let payload = b"xxxx";
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            3,
            &token,
            &sha256(payload),
            payload,
        ))
        .await;
    assert_eq!(status_code, StatusCode::BAD_REQUEST, "{body}");
    let envelope = error_envelope(&body);
    assert_eq!(envelope.error.code, ErrorCode::InvalidRequest);
    assert!(
        envelope.error.message.contains("超出块数 3"),
        "错误信息应指出越界索引与块数：{body}"
    );
    assert!(!envelope.error.retryable, "越界索引重试同一请求没有意义");

    assert!(
        stored_part_indices(&fixture, seeds.owner, &session.upload_id)
            .await
            .is_empty(),
        "越界索引不得产生块行"
    );
    assert!(
        staging_files(&fixture, &session.upload_id).is_empty(),
        "越界索引必须在写盘前被拒绝"
    );
    let reported = status(&app, seeds.root, &session.upload_id, &token).await;
    assert!(reported.received.is_empty(), "进度不得变化：{reported:?}");
    assert_eq!(reported.missing.len(), 1);
}

#[tokio::test]
async fn oversized_payload_is_rejected_by_both_real_limits_and_leaves_nothing_behind() {
    let fixture = Fixture::new("oversize");
    let seeds = seed_accounts(&fixture).await;
    let content = b"0123456789";
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let session = begin(&app, seeds.root, &token, 10, 4, &sha256(content)).await;

    // (a) 超过该块应有长度（4 字节），但仍在 Axum 默认请求体上限内：由用例层拒绝，400。
    let longer = vec![0x41_u8; 64];
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            0,
            &token,
            &sha256(&longer),
            &longer,
        ))
        .await;
    assert_eq!(status_code, StatusCode::BAD_REQUEST, "{body}");
    let envelope = error_envelope(&body);
    assert_eq!(envelope.error.code, ErrorCode::InvalidRequest);
    assert!(
        envelope.error.message.contains("长度应为 4，实际 64"),
        "错误信息应给出期望与实际长度：{body}"
    );

    // (b) 契约允许的最大块（默认块大小 4 MiB）必须真的收得下：Axum 默认请求体上限只有 2 MiB，
    //     路由层已按策略把上传端点的上限抬到“最大块大小 + 1”，否则默认块大小根本无法上传。
    let default_chunk = 4 * 1024 * 1024_usize;
    let big_content: Vec<u8> = (0..2 * default_chunk)
        .map(|offset| (offset % 251) as u8)
        .collect();
    let big = begin(
        &app,
        seeds.root,
        &token,
        (2 * default_chunk) as u64,
        default_chunk as u64,
        &sha256(&big_content),
    )
    .await;
    assert_eq!(big.chunk_size, default_chunk.to_string());
    assert_eq!(big.part_count, "2");
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &big.upload_id,
            0,
            &token,
            &sha256(&big_content[..default_chunk]),
            &big_content[..default_chunk],
        ))
        .await;
    assert_eq!(status_code, StatusCode::OK, "默认块大小必须可用：{body}");
    let ack: UploadPartAck = parse(&body, "UploadPartAck");
    assert_eq!(ack.length, default_chunk.to_string());

    // (c) 超过策略上限（16 MiB + 1）的载荷：路由层按同一策略直接 413，处理器不执行、不落盘。
    let huge = vec![0x42_u8; 16 * 1024 * 1024 + 2];
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &big.upload_id,
            1,
            &token,
            &sha256(&huge),
            &huge,
        ))
        .await;
    assert_eq!(
        status_code,
        StatusCode::PAYLOAD_TOO_LARGE,
        "超策略上限的真实返回码：{body}"
    );

    assert!(
        stored_part_indices(&fixture, seeds.owner, &session.upload_id)
            .await
            .is_empty(),
        "超长载荷不得产生块行"
    );
    assert!(
        staging_files(&fixture, &session.upload_id).is_empty(),
        "超长载荷必须在写盘前被拒绝，不留暂存文件"
    );
    assert_eq!(
        stored_part_indices(&fixture, seeds.owner, &big.upload_id).await,
        vec![0],
        "413 的块不得留下块行，已收的 4 MiB 块必须真的登记"
    );
    assert_eq!(
        staging_files(&fixture, &big.upload_id),
        vec!["00000000000000000000".to_owned()],
        "413 的块不得留下暂存文件"
    );
    let reported_big = status(&app, seeds.root, &big.upload_id, &token).await;
    assert_eq!(
        reported_big
            .received
            .iter()
            .map(|part| part.index.as_str())
            .collect::<Vec<_>>(),
        vec!["0"],
        "进度只应包括真正收下的块：{reported_big:?}"
    );
    let reported = status(&app, seeds.root, &session.upload_id, &token).await;
    assert!(reported.received.is_empty(), "进度不得变化：{reported:?}");
}

#[tokio::test]
async fn mismatched_part_digest_is_rejected_with_422_and_not_recorded() {
    let fixture = Fixture::new("digest-mismatch");
    let seeds = seed_accounts(&fixture).await;
    let content = b"0123456789";
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let session = begin(&app, seeds.root, &token, 10, 4, &sha256(content)).await;

    // 字节本身合法（4 字节），声明的摘要却是另一段内容的。
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            0,
            &token,
            &sha256(b"not-these-bytes"),
            &content[0..4],
        ))
        .await;
    assert_eq!(
        status_code,
        StatusCode::UNPROCESSABLE_ENTITY,
        "摘要不符的真实返回码：{body}"
    );
    let envelope = error_envelope(&body);
    assert_eq!(envelope.error.code, ErrorCode::HashMismatch);
    assert!(
        envelope.error.message.contains(&sha256(&content[0..4])),
        "错误信息应带上实测摘要：{body}"
    );
    assert!(
        stored_part_indices(&fixture, seeds.owner, &session.upload_id)
            .await
            .is_empty(),
        "摘要不符不得登记块"
    );
    assert!(
        staging_files(&fixture, &session.upload_id).is_empty(),
        "摘要不符必须在写盘前被拒绝"
    );
    let reported = status(&app, seeds.root, &session.upload_id, &token).await;
    assert!(reported.received.is_empty(), "进度不得变化：{reported:?}");
}

#[tokio::test]
async fn upload_from_another_login_session_is_404_and_does_not_touch_progress() {
    let fixture = Fixture::new("cross-session");
    let seeds = seed_accounts(&fixture).await;
    let content = b"0123456789";
    let app = open_app(&fixture).await;
    let token_a = login(&app, "owner").await;
    let token_b = login(&app, "owner").await;
    assert_ne!(token_a, token_b, "两次登录必须是两个不同的登录会话");

    let session = begin(&app, seeds.root, &token_a, 10, 4, &sha256(content)).await;
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            0,
            &token_a,
            &sha256(&content[0..4]),
            &content[0..4],
        ))
        .await;
    assert_eq!(status_code, StatusCode::OK, "{body}");

    // 同一 owner 的另一个登录会话：写块与读进度都得到与“不存在”相同的 404。
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            1,
            &token_b,
            &sha256(&content[4..8]),
            &content[4..8],
        ))
        .await;
    assert_eq!(status_code, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        error_envelope(&body).error.code,
        ErrorCode::UnsupportedEntry
    );

    let (status_code, body) = app
        .call(get(
            &format!(
                "/v1/roots/{}/uploads/{}",
                seeds.root.to_hex(),
                session.upload_id
            ),
            Some(&token_b),
        ))
        .await;
    assert_eq!(status_code, StatusCode::NOT_FOUND, "{body}");

    // A 的进度不变：只有块 0，块 1、2 仍缺；B 的写请求没有留下任何痕迹。
    let reported = status(&app, seeds.root, &session.upload_id, &token_a).await;
    assert_eq!(
        reported
            .received
            .iter()
            .map(|part| part.index.as_str())
            .collect::<Vec<_>>(),
        vec!["0"],
        "跨会话写块不得改变 A 的进度：{reported:?}"
    );
    assert_eq!(reported.missing.len(), 1);
    assert_eq!(reported.missing[0].first_index, "1");
    assert_eq!(reported.missing[0].count, "2");
    assert_eq!(
        stored_part_indices(&fixture, seeds.owner, &session.upload_id).await,
        vec![0],
        "跨会话写块不得产生块行"
    );
    assert_eq!(
        staging_files(&fixture, &session.upload_id),
        vec!["00000000000000000000".to_owned()],
        "暂存目录里只应有 A 自己写入的那一块"
    );
}

#[tokio::test]
async fn missing_credentials_are_401_and_another_accounts_root_is_403() {
    let fixture = Fixture::new("authz");
    let seeds = seed_accounts(&fixture).await;
    let content = b"0123456789";
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let session = begin(&app, seeds.root, &token, 10, 4, &sha256(content)).await;
    let uri = format!(
        "/v1/roots/{}/uploads/{}",
        seeds.root.to_hex(),
        session.upload_id
    );

    // 无凭据：401 AUTH_EXPIRED（不是 403，也不是“会话不存在”）。
    let (status_code, body) = app.call(get(&uri, None)).await;
    assert_eq!(status_code, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(error_envelope(&body).error.code, ErrorCode::AuthExpired);
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            0,
            "",
            &sha256(&content[0..4]),
            &content[0..4],
        ))
        .await;
    assert_eq!(
        status_code,
        StatusCode::UNAUTHORIZED,
        "空凭据也必须 401：{body}"
    );
    assert_eq!(error_envelope(&body).error.code, ErrorCode::AuthExpired);

    // 别人的 Root：403 ACCESS_DENIED（不泄露 Root 或会话是否存在）。
    let intruder_token = login(&app, "intruder").await;
    let (status_code, body) = app.call(get(&uri, Some(&intruder_token))).await;
    assert_eq!(status_code, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(error_envelope(&body).error.code, ErrorCode::AccessDenied);
    let (status_code, body) = app
        .call(put_part_request(
            seeds.root,
            &session.upload_id,
            0,
            &intruder_token,
            &sha256(&content[0..4]),
            &content[0..4],
        ))
        .await;
    assert_eq!(status_code, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(error_envelope(&body).error.code, ErrorCode::AccessDenied);

    // 越权请求没有产生任何块行：owner 的进度仍是全缺。
    assert!(
        stored_part_indices(&fixture, seeds.owner, &session.upload_id)
            .await
            .is_empty(),
        "越权请求不得写入块行"
    );
}
