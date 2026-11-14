//! S046-b：上传 finalize 与固定版本内容下载的 HTTP 级验收（design §13.2、§15.2、I01、I05、I12）。
//!
//! 用例走真实 router、真实 SQLite 与真实状态目录（对象区、上传暂存都在临时目录里），用
//! `tower::oneshot` 调用处理器：断言的是端点实际返回的状态码、响应头与响应体，不用核心层假端口
//! 代替端点证据。

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use tower::ServiceExt;
use vaultsyncd_domain::ids::{EntryId, ID_LEN, TxId, UserId, VaultId, VersionId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp};
use vaultsyncd_protocol::generated::{
    ErrorCode, ErrorEnvelope, FinalizedContent, ObjectReuse, SessionIssued, UploadRequest,
    UploadSession, UploadState, UploadStatus,
};
use vaultsyncd_server::auth::AuthContext;
use vaultsyncd_server::content::ContentContext;
use vaultsyncd_server::http::router;
use vaultsyncd_server::lifecycle::Lifecycle;
use vaultsyncd_server::uploads::{PART_HASH_HEADER, UploadsContext};
use vaultsyncd_server::vaults::VaultContext;
use vaultsyncd_server_core::auth::password::PasswordHasher;
use vaultsyncd_server_core::objects::{ContentDigest, ObjectLayout};
use vaultsyncd_server_store::changes;
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::entries::{self, EntryKind, NewEntry};
use vaultsyncd_server_store::users;
use vaultsyncd_server_store::vaults::{self, NewVault};
use vaultsyncd_server_store::versions::{self, NewObject, NewVersion, VersionOrigin};
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
            std::env::temp_dir().join(format!("vaultsyncd-s046b-{}-{label}", std::process::id()));
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

    /// 某个 Vault 的对象区布局：与 `ContentContext` 用的是同一个根（`state_dir/objects`）。
    fn objects(&self, vault: VaultId) -> ObjectLayout {
        ObjectLayout::for_vault(self.state_dir().join("objects"), vault)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 种子身份：owner 有 Vault，intruder 有会话但没有 Vault（用来验证 403）。
struct Seeds {
    owner: UserId,
    vault: VaultId,
    vault_entry: EntryId,
}

async fn seed_accounts(fixture: &Fixture) -> Seeds {
    let owner = UserId::from_bytes([0x11; ID_LEN]);
    let intruder = UserId::from_bytes([0x33; ID_LEN]);
    let vault = VaultId::from_bytes([0x22; ID_LEN]);
    let vault_entry = EntryId::from_bytes([0x55; ID_LEN]);
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
    vaults::insert_vault(
        &mut tx,
        &NewVault {
            id: vault,
            entry: vault_entry,
            owner,
            absolute_path: "/srv/vaultsyncd-s046b-owner".to_owned(),
            volume_identity: vec![0x77; 16],
            vault_file_identity: vec![0x88; 16],
            epoch: ServerEpoch::new(1).expect("epoch"),
        },
    )
    .await
    .expect("写种子 Vault");
    tx.commit().await.expect("提交种子事务");
    database.close().await;
    Seeds {
        owner,
        vault,
        vault_entry,
    }
}

/// 第二位用户拥有独立 Vault，用相同摘要验证对象隔离而不扩大授权范围。
async fn seed_other_vault(fixture: &Fixture) -> Seeds {
    let seeds = Seeds {
        owner: UserId::from_bytes([0x33; ID_LEN]),
        vault: VaultId::from_bytes([0x23; ID_LEN]),
        vault_entry: EntryId::from_bytes([0x56; ID_LEN]),
    };
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let mut tx = database.begin_write().await.expect("写事务");
    vaults::insert_vault(
        &mut tx,
        &NewVault {
            id: seeds.vault,
            entry: seeds.vault_entry,
            owner: seeds.owner,
            absolute_path: "/srv/vaultsyncd-s136c-other".to_owned(),
            volume_identity: vec![0x79; 16],
            vault_file_identity: vec![0x89; 16],
            epoch: ServerEpoch::new(1).expect("epoch"),
        },
    )
    .await
    .expect("写第二个 Vault");
    tx.commit().await.expect("提交第二个 Vault");
    database.close().await;
    seeds
}

/// 在规范路径写入对象字节并登记，返回该 Vault 的规范相对路径。
async fn seed_object(fixture: &Fixture, seeds: &Seeds, bytes: &[u8]) -> String {
    let digest = ContentDigest::from_bytes(hash::digest_of(bytes));
    let storage_path = ObjectLayout::relative_object_path(seeds.vault, &digest)
        .display()
        .to_string();
    let path = fixture.objects(seeds.vault).root().join(&storage_path);
    std::fs::create_dir_all(path.parent().expect("对象父目录")).expect("建对象目录");
    std::fs::write(path, bytes).expect("写对象字节");
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let mut tx = database.begin_write().await.expect("写事务");
    versions::record_object(
        &mut tx,
        seeds.owner,
        &NewObject {
            vault: seeds.vault,
            hash: *digest.as_bytes(),
            size: bytes.len() as u64,
            storage_path: storage_path.clone(),
            verified_at: UtcTimestamp::from_unix_seconds(1_700_000_000).expect("时间戳"),
        },
    )
    .await
    .expect("登记对象");
    tx.commit().await.expect("提交对象");
    database.close().await;
    storage_path
}

/// 进程内装配：与 `bootstrap` 一样把同一份数据库与状态目录交给各上下文，再组装真实 router。
struct App {
    router: Router,
}

async fn open_app(fixture: &Fixture) -> App {
    open_app_office(fixture, None).await
}

async fn open_app_office(
    fixture: &Fixture,
    office: Option<Arc<vaultsyncd_server::office::OfficeRuntime>>,
) -> App {
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let auth = Arc::new(AuthContext::new(database));
    let uploads = Arc::new(UploadsContext::new(
        Arc::clone(&auth),
        fixture.state_dir().as_path(),
    ));
    let vaults = Arc::new(VaultContext::new(
        Arc::clone(&auth),
        None,
        fixture.state_dir(),
        Default::default(),
    ));
    let content = Arc::new(
        ContentContext::new(Arc::clone(&auth), fixture.state_dir().as_path()).with_office(office),
    );
    let lifecycle = Arc::new(Lifecycle::new());
    lifecycle.mark_ready();
    let executor = Arc::new(vaultsyncd_server::executor::ExecutorContext::new(
        auth.database().clone(),
        fixture.state_dir().as_path(),
        None,
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
        router: router(lifecycle, auth, uploads, vaults, content, operations, sync),
    }
}

/// 一次响应的全部可断言事实（状态码、响应头、字节体）。
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("响应体是 UTF-8")
    }

    fn envelope(&self) -> ErrorEnvelope {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!("解析错误信封失败：{error}：{}", self.text());
        })
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }
}

impl App {
    async fn call(&self, request: Request<Body>) -> Reply {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("调用处理器");
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("读响应体")
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(reply: &Reply, what: &str) -> T {
    serde_json::from_slice(&reply.body)
        .unwrap_or_else(|error| panic!("解析 {what} 失败：{error}：{}", reply.text()))
}

fn post(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("构造请求")
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

fn get_range(uri: &str, token: Option<&str>, range: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header(
            "authorization",
            format!("Bearer {}", token.unwrap_or_default()),
        )
        .header(header::RANGE, range)
        .body(Body::empty())
        .expect("构造请求")
}

fn put_part(
    vault: VaultId,
    upload: &str,
    index: u64,
    token: &str,
    declared: Option<&str>,
    bytes: &[u8],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(format!(
            "/v1/vaults/{}/uploads/{upload}/parts/{index}",
            vault.to_hex()
        ))
        .header("authorization", format!("Bearer {token}"));
    if let Some(declared) = declared {
        builder = builder.header(PART_HASH_HEADER, declared);
    }
    builder.body(Body::from(bytes.to_vec())).expect("构造请求")
}

fn sha256(bytes: &[u8]) -> String {
    ContentDigest::from_bytes(hash::digest_of(bytes)).to_hex()
}

async fn login(app: &App, account: &str) -> String {
    let request = vaultsyncd_protocol::generated::LoginRequest {
        hostname: None,
        operating_system: None,
        device_label: "S046-b 验收".to_owned(),
        login: account.to_owned(),
        password: PASSWORD.to_owned(),
    };
    let body = serde_json::to_string(&request).expect("序列化登录请求");
    let reply = app.call(post_json("/v1/auth/login", None, &body)).await;
    assert_eq!(reply.status, StatusCode::OK, "登录应成功：{}", reply.text());
    parse::<SessionIssued>(&reply, "SessionIssued").access_token
}

async fn begin(
    app: &App,
    vault: VaultId,
    token: &str,
    total_size: u64,
    chunk_size: u64,
    content_hash: &str,
) -> UploadSession {
    let request = UploadRequest {
        chunk_size: chunk_size.to_string(),
        hash: content_hash.to_owned(),
        total_size: total_size.to_string(),
    };
    let body = serde_json::to_string(&request).expect("序列化创建请求");
    let reply = app
        .call(post_json(
            &format!("/v1/vaults/{}/uploads", vault.to_hex()),
            Some(token),
            &body,
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::CREATED,
        "创建上传会话应返回 201：{}",
        reply.text()
    );
    parse(&reply, "UploadSession")
}

async fn status(app: &App, vault: VaultId, upload: &str, token: &str) -> UploadStatus {
    let reply = app
        .call(get(
            &format!("/v1/vaults/{}/uploads/{upload}", vault.to_hex()),
            Some(token),
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "查询上传状态应成功：{}",
        reply.text()
    );
    parse(&reply, "UploadStatus")
}

async fn finalize(app: &App, vault: VaultId, upload: &str, token: &str) -> Reply {
    app.call(post(
        &format!("/v1/vaults/{}/uploads/{upload}/finalize", vault.to_hex()),
        Some(token),
    ))
    .await
}

fn download_uri(vault: VaultId, entry: EntryId, version: VersionId) -> String {
    format!(
        "/v1/vaults/{}/entries/{}/versions/{}/content",
        vault.to_hex(),
        entry.to_hex(),
        version.to_hex()
    )
}

/// 种子：一个文件条目、两个版本（v1 旧内容、v2 当前内容）与两条对象行，字节写进对象区。
struct SeededVersions {
    entry: EntryId,
    old: (VersionId, Vec<u8>),
    current: (VersionId, Vec<u8>),
}

async fn seed_versions(fixture: &Fixture, seeds: &Seeds, at: i64) -> SeededVersions {
    let entry = EntryId::from_bytes([0x66; ID_LEN]);
    let old_bytes = b"old-content".to_vec();
    let current_bytes = b"new-content-here".to_vec();
    let old_version = VersionId::from_bytes([0x71; ID_LEN]);
    let current_version = VersionId::from_bytes([0x72; ID_LEN]);
    let old_digest = ContentDigest::from_bytes(hash::digest_of(&old_bytes));
    let current_digest = ContentDigest::from_bytes(hash::digest_of(&current_bytes));
    let moment = UtcTimestamp::from_unix_seconds(at).expect("时间戳");

    // 对象区：按内容地址写两份对象，路径来自 `ObjectLayout` 的规范相对路径。
    let layout = fixture.objects(seeds.vault);
    for (digest, bytes) in [(&old_digest, &old_bytes), (&current_digest, &current_bytes)] {
        let path = layout.object_path(digest);
        std::fs::create_dir_all(path.parent().expect("对象父目录")).expect("建对象目录");
        std::fs::write(&path, bytes).expect("写对象字节");
    }

    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let mut tx = database.begin_write().await.expect("写事务");
    entries::insert_entry(
        &mut tx,
        seeds.owner,
        &NewEntry {
            id: entry,
            vault: seeds.vault,
            parent: seeds.vault_entry,
            name: "notes.txt".to_owned(),
            name_key: "notes.txt".to_owned(),
            kind: EntryKind::File,
            revision: Revision::new(1).expect("修订号"),
            content_version: None,
        },
        moment,
    )
    .await
    .expect("写种子条目");
    tx.commit().await.expect("提交种子条目");

    for (digest, bytes, version, marker) in [
        (&old_digest, &old_bytes, old_version, 0x31_u8),
        (&current_digest, &current_bytes, current_version, 0x32_u8),
    ] {
        let mut tx = database.begin_write().await.expect("写事务");
        versions::record_object(
            &mut tx,
            seeds.owner,
            &NewObject {
                vault: seeds.vault,
                hash: *digest.as_bytes(),
                size: bytes.len() as u64,
                storage_path: ObjectLayout::relative_object_path(seeds.vault, digest)
                    .display()
                    .to_string(),
                verified_at: moment,
            },
        )
        .await
        .expect("写种子对象行");
        let batch = changes::append_change(
            &mut tx,
            seeds.owner,
            seeds.vault,
            TxId::from_bytes([marker; ID_LEN]),
            r#"[{"kind":"content_put","entry":"66"}]"#,
            moment,
        )
        .await
        .expect("追加批次");
        versions::record_version(
            &mut tx,
            seeds.owner,
            &NewVersion {
                id: version,
                vault: seeds.vault,
                entry,
                object_hash: *digest.as_bytes(),
                size: bytes.len() as u64,
                created_seq: batch.seq,
                origin: VersionOrigin::Client,
                at: moment,
            },
        )
        .await
        .expect("写种子版本行");
        tx.commit().await.expect("提交种子版本");
    }
    database.close().await;
    SeededVersions {
        entry,
        old: (old_version, old_bytes),
        current: (current_version, current_bytes),
    }
}

/// 一条上传：按块上传 `bytes`（每块长度由几何决定），返回会话身份。
async fn upload_content(
    app: &App,
    vault: VaultId,
    token: &str,
    bytes: &[u8],
    chunk_size: u64,
    total_hash: &str,
) -> UploadSession {
    let session = begin(
        app,
        vault,
        token,
        bytes.len() as u64,
        chunk_size,
        total_hash,
    )
    .await;
    let chunk = usize::try_from(chunk_size).expect("块大小");
    for (index, part) in bytes.chunks(chunk).enumerate() {
        let reply = app
            .call(put_part(
                vault,
                &session.upload_id,
                index as u64,
                token,
                Some(&sha256(part)),
                part,
            ))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "上传块 {index} 应成功：{}",
            reply.text()
        );
    }
    session
}

#[tokio::test]
async fn finalize_publishes_content_and_only_returns_a_reference() {
    let fixture = Fixture::new("finalize-ok");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let content = b"six-byte-content";
    let session = upload_content(&app, seeds.vault, &token, content, 4, &sha256(content)).await;

    let reply = finalize(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "finalize 应成功：{}",
        reply.text()
    );
    let finalized: FinalizedContent = parse(&reply, "FinalizedContent");
    assert_eq!(finalized.upload_id, session.upload_id);
    assert_eq!(finalized.vault_id, seeds.vault.to_hex());
    assert_eq!(finalized.hash, sha256(content));
    assert_eq!(finalized.size, content.len().to_string());
    assert_eq!(finalized.part_count, "4");
    assert_eq!(finalized.reuse, ObjectReuse::FreshlyPublished);
    assert!(!finalized.replayed);
    assert_eq!(
        finalized.storage_path,
        ObjectLayout::relative_object_path(
            seeds.vault,
            &ContentDigest::from_hex(&sha256(content)).expect("内容地址")
        )
        .display()
        .to_string()
    );

    // 对象字节真的落到了对象区，且内容与会话声明一致。
    let object = fixture
        .objects(seeds.vault)
        .root()
        .join(&finalized.storage_path);
    assert_eq!(
        std::fs::read(&object).expect("读已发布对象"),
        content.to_vec(),
        "对象字节应等于上传内容"
    );
    // 会话变成 finalized；finalize 不改条目，因此这里只断言上传状态。
    let after = status(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(after.upload.state, UploadState::Finalized);
}

#[tokio::test]
async fn same_digest_in_two_vaults_has_independent_publication_reuse_and_replay() {
    let fixture = Fixture::new("vault-scoped-finalize");
    let first = seed_accounts(&fixture).await;
    let second = seed_other_vault(&fixture).await;
    let app = open_app(&fixture).await;
    let owner = login(&app, "owner").await;
    let other = login(&app, "intruder").await;
    let bytes = b"same digest, independent vault objects";
    let digest = ContentDigest::from_bytes(hash::digest_of(bytes));
    let first_session = upload_content(&app, first.vault, &owner, bytes, 8, &sha256(bytes)).await;
    let second_session = upload_content(&app, second.vault, &other, bytes, 8, &sha256(bytes)).await;

    // 持有另一 Vault 会话也不扩大 Vault 授权；拒绝后原会话仍可正常 finalize。
    for (vault, session, token) in [
        (first.vault, &first_session, &other),
        (second.vault, &second_session, &owner),
    ] {
        let denied = finalize(&app, vault, &session.upload_id, token).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text());
        assert_eq!(denied.envelope().error.code, ErrorCode::AccessDenied);
    }

    let mut paths = Vec::new();
    for (vault, session, token) in [
        (first.vault, first_session, &owner),
        (second.vault, second_session, &other),
    ] {
        let expected = ObjectLayout::relative_object_path(vault, &digest)
            .display()
            .to_string();
        let reply = finalize(&app, vault, &session.upload_id, token).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        let published: FinalizedContent = parse(&reply, "FinalizedContent");
        assert_eq!(published.storage_path, expected);
        assert_eq!(published.vault_id, vault.to_hex());
        assert_eq!(published.reuse, ObjectReuse::FreshlyPublished);
        assert!(!published.replayed);
        assert_eq!(
            std::fs::read(fixture.objects(vault).root().join(&expected)).unwrap(),
            bytes
        );

        let replay = finalize(&app, vault, &session.upload_id, token).await;
        assert_eq!(replay.status, StatusCode::OK, "{}", replay.text());
        let replayed: FinalizedContent = parse(&replay, "FinalizedContent");
        assert!(replayed.replayed);
        assert_eq!(replayed.storage_path, expected);

        let again = upload_content(&app, vault, token, bytes, 8, &sha256(bytes)).await;
        let reply = finalize(&app, vault, &again.upload_id, token).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        let reused: FinalizedContent = parse(&reply, "FinalizedContent");
        assert_eq!(reused.reuse, ObjectReuse::ExistingVerified);
        assert!(!reused.replayed);
        assert_eq!(reused.storage_path, expected);
        paths.push(expected);
    }
    assert_ne!(paths[0], paths[1]);
    for (vault, path) in [(first.vault, &paths[0]), (second.vault, &paths[1])] {
        assert!(
            fixture.objects(vault).root().join(path).is_file(),
            "每个 Vault 的同一摘要各有一条自己的规范路径"
        );
    }
    let database = Database::open(fixture.database()).await.unwrap();
    let rows: Vec<(Vec<u8>, String)> = sqlx::query_as(
        "SELECT vault_id, storage_path FROM objects WHERE hash = ? ORDER BY vault_id",
    )
    .bind(digest.as_bytes().as_slice())
    .fetch_all(database.pool())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (first.vault.as_bytes().to_vec(), paths[0].clone()),
            (second.vault.as_bytes().to_vec(), paths[1].clone()),
        ]
    );
    database.close().await;
}

#[tokio::test]
async fn registered_object_is_reused_at_its_canonical_path() {
    let fixture = Fixture::new("finalize-reuse");
    let seeds = seed_accounts(&fixture).await;
    let bytes = b"registered object";
    let registered = seed_object(&fixture, &seeds, bytes).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let session = upload_content(&app, seeds.vault, &token, bytes, 8, &sha256(bytes)).await;
    for replayed in [false, true] {
        let reply = finalize(&app, seeds.vault, &session.upload_id, &token).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        let finalized: FinalizedContent = parse(&reply, "FinalizedContent");
        assert_eq!(finalized.storage_path, registered);
        assert_eq!(finalized.reuse, ObjectReuse::ExistingVerified);
        assert_eq!(finalized.replayed, replayed);
    }
    assert_eq!(
        std::fs::read(fixture.objects(seeds.vault).root().join(&registered)).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn another_vaults_object_is_not_reused_for_new_publication() {
    let fixture = Fixture::new("cross-vault-reuse");
    let first = seed_accounts(&fixture).await;
    let second = seed_other_vault(&fixture).await;
    let bytes = b"object belonging to a different vault";
    let first_path = seed_object(&fixture, &first, bytes).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "intruder").await;
    let session = upload_content(&app, second.vault, &token, bytes, 8, &sha256(bytes)).await;
    let expected = ObjectLayout::relative_object_path(
        second.vault,
        &ContentDigest::from_bytes(hash::digest_of(bytes)),
    )
    .display()
    .to_string();
    assert_ne!(expected, first_path);
    for replayed in [false, true] {
        let reply = finalize(&app, second.vault, &session.upload_id, &token).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        let finalized: FinalizedContent = parse(&reply, "FinalizedContent");
        assert_eq!(finalized.storage_path, expected);
        assert_eq!(finalized.replayed, replayed);
        if !replayed {
            assert_eq!(finalized.reuse, ObjectReuse::FreshlyPublished);
        }
    }
    for (vault, path) in [(first.vault, &first_path), (second.vault, &expected)] {
        assert_eq!(
            std::fs::read(fixture.objects(vault).root().join(path)).unwrap(),
            bytes
        );
    }
    let database = Database::open(fixture.database()).await.unwrap();
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT storage_path FROM objects ORDER BY vault_id")
            .fetch_all(database.pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&first_path));
    assert!(rows.contains(&expected));
    database.close().await;
}

#[tokio::test]
async fn finalize_with_missing_parts_is_400_and_keeps_the_session_open() {
    let fixture = Fixture::new("finalize-incomplete");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let content = b"abcdefgh";
    let session = begin(
        &app,
        seeds.vault,
        &token,
        content.len() as u64,
        4,
        &sha256(content),
    )
    .await;
    // 只传第 0 块：块数够但缺一块，finalize 的前提不成立。
    let reply = app
        .call(put_part(
            seeds.vault,
            &session.upload_id,
            0,
            &token,
            Some(&sha256(&content[..4])),
            &content[..4],
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "块应被接收：{}", reply.text());

    let reply = finalize(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "缺块时 finalize 必须拒绝：{}",
        reply.text()
    );
    let envelope = reply.envelope();
    assert_eq!(envelope.error.code, ErrorCode::InvalidRequest);
    assert!(!envelope.error.retryable);

    // 会话没有被推进，也没有对象被发布。
    let after = status(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(after.upload.state, UploadState::Open);
    assert!(
        !fixture
            .objects(seeds.vault)
            .root()
            .join(seeds.vault.to_hex())
            .exists(),
        "缺块时不得发布任何对象"
    );
}

#[tokio::test]
async fn finalize_with_mismatched_content_is_422_and_aborts_the_session() {
    let fixture = Fixture::new("finalize-mismatch");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let uploaded = b"actual-bytes";
    // 会话声明的是另一份内容的哈希：块本身合法，拼装后总哈希不符。
    let session = upload_content(
        &app,
        seeds.vault,
        &token,
        uploaded,
        4,
        &sha256(b"declared-bytes"),
    )
    .await;

    let reply = finalize(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "内容与声明不符时必须是 422：{}",
        reply.text()
    );
    assert_eq!(reply.envelope().error.code, ErrorCode::HashMismatch);

    // 校验失败不提交：会话被放弃，且没有任何对象行/对象字节残留。
    let after = status(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(after.upload.state, UploadState::Aborted);
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let mut conn = database.pool().acquire().await.expect("取连接");
    let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT hash FROM objects")
        .fetch_all(&mut *conn)
        .await
        .expect("查对象行");
    assert!(rows.is_empty(), "不符的内容不得登记对象：{rows:?}");
    drop(conn);
    database.close().await;
}

#[tokio::test]
async fn finalize_replay_returns_the_same_reference() {
    let fixture = Fixture::new("finalize-replay");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let content = b"replay-me";
    let session = upload_content(&app, seeds.vault, &token, content, 4, &sha256(content)).await;

    let first: FinalizedContent = parse(
        &finalize(&app, seeds.vault, &session.upload_id, &token).await,
        "FinalizedContent",
    );
    let reply = finalize(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(reply.status, StatusCode::OK, "重放应成功：{}", reply.text());
    let second: FinalizedContent = parse(&reply, "FinalizedContent");
    assert!(second.replayed, "第二次 finalize 必须标记为重放");
    assert_eq!(second.hash, first.hash);
    assert_eq!(second.size, first.size);
    assert_eq!(second.storage_path, first.storage_path);
    assert_eq!(second.part_count, first.part_count);
}

#[tokio::test]
async fn part_digest_header_is_required_and_enforced() {
    let fixture = Fixture::new("part-header");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let content = b"abcdefgh";
    let session = begin(
        &app,
        seeds.vault,
        &token,
        content.len() as u64,
        4,
        &sha256(content),
    )
    .await;

    // 缺少 `x-part-sha256`：形状不成立，不登记块。
    let reply = app
        .call(put_part(
            seeds.vault,
            &session.upload_id,
            0,
            &token,
            None,
            &content[..4],
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::BAD_REQUEST,
        "缺块摘要头必须 400：{}",
        reply.text()
    );
    assert_eq!(reply.envelope().error.code, ErrorCode::InvalidRequest);

    // 声明了错误摘要：422 `HASH_MISMATCH`，块不登记。
    let reply = app
        .call(put_part(
            seeds.vault,
            &session.upload_id,
            0,
            &token,
            Some(&sha256(b"nope")),
            &content[..4],
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "摘要不符必须 422：{}",
        reply.text()
    );
    assert_eq!(reply.envelope().error.code, ErrorCode::HashMismatch);

    // 正确摘要才登记，并且进度里出现这一块。
    let reply = app
        .call(put_part(
            seeds.vault,
            &session.upload_id,
            0,
            &token,
            Some(&sha256(&content[..4])),
            &content[..4],
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "正确摘要应被接收：{}",
        reply.text()
    );
    let after = status(&app, seeds.vault, &session.upload_id, &token).await;
    assert_eq!(after.received.len(), 1);
    assert_eq!(after.received[0].index, "0");
}

#[tokio::test]
async fn download_serves_the_requested_version_and_range_slices() {
    let fixture = Fixture::new("download-slices");
    let seeds = seed_accounts(&fixture).await;
    let seeded = seed_versions(&fixture, &seeds, 1_700_000_000).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let (current_version, current_bytes) = seeded.current.clone();
    let uri = download_uri(seeds.vault, seeded.entry, current_version);

    // 整份：200 + 长度 + 内容地址头。
    let reply = app.call(get(&uri, Some(&token))).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "整份下载应成功：{}",
        reply.text()
    );
    assert_eq!(reply.body, current_bytes);
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(
        reply.header("content-length").as_deref(),
        Some(current_bytes.len().to_string().as_str())
    );
    assert_eq!(
        reply.header("x-content-sha256").as_deref(),
        Some(sha256(&current_bytes).as_str())
    );
    assert_eq!(reply.header("accept-ranges").as_deref(), Some("bytes"));
    assert!(
        reply.header("content-range").is_none(),
        "整份响应不带 Content-Range"
    );

    // 区间：206 + `Content-Range` + 精确切片。
    let reply = app.call(get_range(&uri, Some(&token), "bytes=2-5")).await;
    assert_eq!(
        reply.status,
        StatusCode::PARTIAL_CONTENT,
        "区间应 206：{}",
        reply.text()
    );
    assert_eq!(reply.body, current_bytes[2..=5].to_vec());
    assert_eq!(
        reply.header("content-range").as_deref(),
        Some(format!("bytes 2-5/{}", current_bytes.len()).as_str())
    );

    // 后缀区间：最后 3 个字节。
    let reply = app.call(get_range(&uri, Some(&token), "bytes=-3")).await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        reply.body,
        current_bytes[current_bytes.len() - 3..].to_vec()
    );

    // 越界与非法区间：400，不用空内容或整份冒充。
    for range in ["bytes=100-200", "bytes=5-2", "bytes=abc", "chunks=0-1"] {
        let reply = app.call(get_range(&uri, Some(&token), range)).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "区间 `{range}` 必须拒绝：{}",
            reply.text()
        );
        assert_eq!(reply.envelope().error.code, ErrorCode::InvalidRequest);
    }
    let activity_uri = format!("/v1/vaults/{}/activity?direction=download", seeds.vault);
    let reply = app.call(get(&activity_uri, Some(&token))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    let page: vaultsyncd_protocol::generated::ActivityPage = parse(&reply, "ActivityPage");
    assert_eq!(page.items.len(), 7);
    assert_eq!(
        page.items
            .iter()
            .filter(|item| item.status == "served")
            .count(),
        3
    );
    assert_eq!(
        page.items
            .iter()
            .filter(|item| item.status == "failed"
                && item.error_code.as_deref() == Some("INVALID_REQUEST"))
            .count(),
        4
    );
    let range = page
        .items
        .iter()
        .find(|item| item.offset.as_deref() == Some("2"))
        .unwrap();
    assert_eq!(range.bytes.as_deref(), Some("4"));
    assert_eq!(
        range.version_id.as_deref(),
        Some(current_version.to_hex().as_str())
    );
    assert_eq!(
        range.entry_id.as_deref(),
        Some(seeded.entry.to_hex().as_str())
    );
    assert!(range.path.is_some());
    assert!(range.client_id.is_some());
}

#[tokio::test]
async fn activity_requires_scope_validates_query_and_keeps_finalize_replay_unique() {
    let fixture = Fixture::new("activity-scope");
    let seeds = seed_accounts(&fixture).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;
    let intruder = login(&app, "intruder").await;
    let uri = format!("/v1/vaults/{}/activity", seeds.vault);
    assert_eq!(
        app.call(get(&uri, None)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(get(&uri, Some(&intruder))).await.status,
        StatusCode::FORBIDDEN
    );
    for query in [
        "before=0",
        "before=01",
        "before=-1",
        "before=9223372036854775808",
        "before=1&before=2",
        "direction=none",
        "direction=upload&direction=download",
        "unknown=1",
    ] {
        let reply = app.call(get(&format!("{uri}?{query}"), Some(&token))).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    }
    let session = upload_content(&app, seeds.vault, &token, b"abc", 4, &sha256(b"abc")).await;
    for _ in 0..2 {
        assert_eq!(
            finalize(&app, seeds.vault, &session.upload_id, &token)
                .await
                .status,
            StatusCode::OK
        );
    }
    let reply = app.call(get(&uri, Some(&token))).await;
    let page: vaultsyncd_protocol::generated::ActivityPage = parse(&reply, "ActivityPage");
    assert_eq!(page.items.len(), 1);
    let item = &page.items[0];
    assert_eq!(item.status, "ready");
    assert_eq!(item.action, "upload_finalize");
    assert_eq!(item.size.as_deref(), Some("3"));
    assert_eq!(item.upload_id.as_deref(), Some(session.upload_id.as_str()));
    assert!(item.bytes.is_none());
    assert!(item.path.is_none());
    assert!(item.client_id.is_some());
    assert!(item.device_id.is_none());
    let text = reply.text();
    assert!(!text.contains(&token));
    assert!(!text.contains("storage_path"));

    // 认证归属已落盘而executor尚未创建operations行时，也必须拒绝不同语义复用。
    let operation_uri = format!("/v1/vaults/{}/operations", seeds.vault);
    let mut operation = serde_json::json!({"kind":"put_content","op_id":"d1".repeat(16),"device_id":"d2".repeat(16),"server_epoch":"1","entry_id":seeds.vault_entry.to_hex(),"base_revision":"0","upload_id":"d3".repeat(16)});
    let first = app
        .call(post_json(
            &operation_uri,
            Some(&token),
            &operation.to_string(),
        ))
        .await;
    assert_eq!(first.status, StatusCode::BAD_REQUEST, "{}", first.text());
    operation["upload_id"] = serde_json::Value::String("d4".repeat(16));
    let reused = app
        .call(post_json(
            &operation_uri,
            Some(&token),
            &operation.to_string(),
        ))
        .await;
    assert_eq!(reused.status, StatusCode::CONFLICT, "{}", reused.text());
    assert_eq!(reused.envelope().error.code, ErrorCode::OpIdReused);
}

#[tokio::test]
async fn download_of_a_superseded_version_serves_the_old_bytes() {
    let fixture = Fixture::new("download-old-version");
    let seeds = seed_accounts(&fixture).await;
    let seeded = seed_versions(&fixture, &seeds, 1_700_000_100).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;

    // head 已经指向新内容，但按旧版本身份下载必须拿到旧字节（I12：不按路径取当前内容）。
    let (old_version, old_bytes) = seeded.old.clone();
    let reply = app
        .call(get(
            &download_uri(seeds.vault, seeded.entry, old_version),
            Some(&token),
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "旧版本应可下载：{}",
        reply.text()
    );
    assert_eq!(reply.body, old_bytes);
    assert_eq!(
        reply.header("x-content-sha256").as_deref(),
        Some(sha256(&old_bytes).as_str())
    );
}

#[tokio::test]
async fn download_of_a_gone_version_is_410() {
    let fixture = Fixture::new("download-gone");
    let seeds = seed_accounts(&fixture).await;
    let seeded = seed_versions(&fixture, &seeds, 1_700_000_200).await;
    let app = open_app(&fixture).await;
    let token = login(&app, "owner").await;

    let missing = VersionId::from_bytes([0x7f; ID_LEN]);
    let reply = app
        .call(get(
            &download_uri(seeds.vault, seeded.entry, missing),
            Some(&token),
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::GONE,
        "过期/不存在的版本必须 410：{}",
        reply.text()
    );
    assert_eq!(reply.envelope().error.code, ErrorCode::VersionGone);
}

#[tokio::test]
async fn content_endpoints_require_credentials_and_vault_scope() {
    let fixture = Fixture::new("content-auth");
    let seeds = seed_accounts(&fixture).await;
    let seeded = seed_versions(&fixture, &seeds, 1_700_000_300).await;
    let app = open_app(&fixture).await;
    let (current_version, _) = seeded.current.clone();
    let uri = download_uri(seeds.vault, seeded.entry, current_version);

    // 无凭据：401 `AUTH_EXPIRED`。
    let reply = app.call(get(&uri, None)).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());
    assert_eq!(reply.envelope().error.code, ErrorCode::AuthExpired);
    let reply = app
        .call(post(
            &format!(
                "/v1/vaults/{}/uploads/{}0000000000000000000000000000000/finalize",
                seeds.vault.to_hex(),
                ""
            ),
            None,
        ))
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.text());

    // 别人的 Vault：403 `ACCESS_DENIED`，下载与 finalize 都拒。
    let intruder = login(&app, "intruder").await;
    let reply = app.call(get(&uri, Some(&intruder))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    assert_eq!(reply.envelope().error.code, ErrorCode::AccessDenied);

    let owner = login(&app, "owner").await;
    let session = upload_content(&app, seeds.vault, &owner, b"abc", 4, &sha256(b"abc")).await;
    let reply = finalize(&app, seeds.vault, &session.upload_id, &intruder).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text());
    assert_eq!(reply.envelope().error.code, ErrorCode::AccessDenied);
}

/// The fake service speaks OnlyOffice's real JWT/URL protocol; authorization and disk reads stay real.
struct OfficeFixture {
    runtime: Arc<vaultsyncd_server::office::OfficeRuntime>,
    task: tokio::task::JoinHandle<()>,
    mode: Arc<std::sync::atomic::AtomicU8>,
    sources: Arc<std::sync::Mutex<Vec<String>>>,
    inputs: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    arrived: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Semaphore>,
}
impl Drop for OfficeFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl OfficeFixture {
    async fn new(fixture: &Fixture, initial_mode: u8) -> Self {
        use axum::response::IntoResponse;
        use base64::Engine;
        use std::sync::atomic::{AtomicU8, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let secret = fixture.path.join("office-jwt");
        std::fs::write(&secret, "x".repeat(64)).unwrap();
        let runtime = Arc::new(
            vaultsyncd_server::office::OfficeRuntime::load(
                &vaultsyncd_server::config::OfficeConfig {
                    document_server_url: base.clone(),
                    jwt_secret_file: secret,
                },
            )
            .unwrap(),
        );
        let mode = Arc::new(AtomicU8::new(initial_mode));
        let sources = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inputs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let arrived = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (server_mode, server_sources, server_inputs, server_arrived, server_gate) = (
            Arc::clone(&mode),
            Arc::clone(&sources),
            Arc::clone(&inputs),
            Arc::clone(&arrived),
            Arc::clone(&gate),
        );
        let file_mode = Arc::clone(&mode);
        let router = Router::new()
            .route(
                "/converter",
                axum::routing::post(move |axum::Json(request): axum::Json<serde_json::Value>| {
                    let (mode, sources, inputs, arrived, gate) = (
                        Arc::clone(&server_mode),
                        Arc::clone(&server_sources),
                        Arc::clone(&server_inputs),
                        Arc::clone(&server_arrived),
                        Arc::clone(&server_gate),
                    );
                    let base = base.clone();
                    async move {
                        let token = request["token"].as_str().unwrap();
                        let pieces: Vec<_> = token.split('.').collect();
                        let payload: serde_json::Value = serde_json::from_slice(
                            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                                .decode(pieces[1])
                                .unwrap(),
                        )
                        .unwrap();
                        let source = payload["url"].as_str().unwrap().to_owned();
                        sources.lock().unwrap().push(source.clone());
                        let current = mode.load(Ordering::SeqCst);
                        if current == 3 {
                            arrived.notify_one();
                            let permit = gate.acquire().await.unwrap();
                            permit.forget();
                        }
                        let response = match reqwest::Client::builder()
                            .no_proxy()
                            .build()
                            .unwrap()
                            .get(&source)
                            .send()
                            .await
                        {
                            Ok(response) => response,
                            Err(_) => {
                                return axum::Json(serde_json::json!({"error":-4})).into_response();
                            }
                        };
                        if !response.status().is_success() {
                            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
                            return axum::Json(serde_json::json!({"error":-4})).into_response();
                        }
                        let input = response.bytes().await.unwrap().to_vec();
                        inputs.lock().unwrap().push(input);
                        if current == 4 {
                            arrived.notify_one();
                            let permit = gate.acquire().await.unwrap();
                            permit.forget();
                        }
                        let file = if current == 1 {
                            "https://foreign.invalid/cache/files/result.pdf".to_owned()
                        } else {
                            format!("{base}cache/files/result.pdf")
                        };
                        axum::Json(serde_json::json!({"endConvert":true,"fileUrl":file}))
                            .into_response()
                    }
                }),
            )
            .route(
                "/cache/files/result.pdf",
                axum::routing::get(move || {
                    let mode = Arc::clone(&file_mode);
                    async move {
                        if mode.load(Ordering::SeqCst) == 2 {
                            "<html>invalid output</html>"
                        } else {
                            "%PDF-1.4\nS213 converted fixture"
                        }
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            runtime,
            task,
            mode,
            sources,
            inputs,
            arrived,
            gate,
        }
    }
}
fn office_request(
    vault: VaultId,
    entry: EntryId,
    version: VersionId,
    bytes: &[u8],
    token: Option<&str>,
) -> Request<Body> {
    post_json(&download_uri(vault,entry,version).replace("/content","/office-preview"),token,
        &serde_json::json!({"file_type":"docx","hash":sha256(bytes),"size":bytes.len().to_string()}).to_string())
}

fn viewer_request(
    vault: VaultId,
    entry: EntryId,
    version: VersionId,
    bytes: &[u8],
    token: Option<&str>,
) -> Request<Body> {
    post_json(
        &download_uri(vault, entry, version).replace("/content", "/office-viewer"),
        token,
        &serde_json::json!({"file_type":"xlsx","hash":sha256(bytes),"size":bytes.len().to_string()}).to_string(),
    )
}

fn close_viewer_request(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn office_viewer_signs_only_fixed_readonly_configuration_and_revokes_its_source() {
    use base64::Engine;
    use hmac::Mac;
    use vaultsyncd_protocol::generated::OfficeViewerSession;

    let fixture = Fixture::new("viewer-fixed-scope");
    let seeds = seed_accounts(&fixture).await;
    let versions = seed_versions(&fixture, &seeds, 1_700_000_000).await;
    let office = OfficeFixture::new(&fixture, 0).await;
    let app = open_app_office(&fixture, Some(Arc::clone(&office.runtime))).await;
    let owner = login(&app, "owner").await;
    let other_session = login(&app, "owner").await;
    let intruder = login(&app, "intruder").await;
    for (token, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some(intruder.as_str()), StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            app.call(viewer_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                token
            ))
            .await
            .status,
            expected
        );
    }
    let wrong_hash = app
        .call(viewer_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.current.1,
            Some(&owner),
        ))
        .await;
    assert_eq!(wrong_hash.status, StatusCode::UNPROCESSABLE_ENTITY);
    let reply = app
        .call(viewer_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let session: OfficeViewerSession = parse(&reply, "viewer");
    assert_eq!(session.source_version, versions.old.0.to_hex());
    assert_eq!(session.source_hash, sha256(&versions.old.1));
    assert_eq!(session.session_id.len(), 64);
    let mut config: serde_json::Value = serde_json::from_str(&session.config_json).unwrap();
    assert_eq!(config["documentType"], "cell");
    assert_eq!(config["document"]["key"], session.session_id);
    assert_eq!(config["editorConfig"]["mode"], "view");
    assert_eq!(config["editorConfig"]["user"]["id"], session.session_id);
    assert_eq!(config["editorConfig"]["user"]["name"], "查看者");
    assert_eq!(
        config["editorConfig"]["coEditing"],
        serde_json::json!({"mode":"strict", "change":false})
    );
    assert_eq!(
        config["editorConfig"]["customization"]["macrosMode"],
        "disable"
    );
    assert_eq!(config["editorConfig"]["customization"]["plugins"], false);
    for permission in [
        "edit",
        "review",
        "comment",
        "fillForms",
        "download",
        "print",
        "chat",
    ] {
        assert_eq!(config["document"]["permissions"][permission], false);
    }
    assert!(config["editorConfig"].get("callbackUrl").is_none());
    let source = config["document"]["url"].as_str().unwrap().to_owned();
    let token = config.as_object_mut().unwrap().remove("token").unwrap();
    let pieces: Vec<_> = token.as_str().unwrap().split('.').collect();
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(pieces[2])
        .unwrap();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&[b'x'; 64]).unwrap();
    mac.update(format!("{}.{}", pieces[0], pieces[1]).as_bytes());
    mac.verify_slice(&signature).unwrap();
    let mut changed = config.clone();
    changed["document"]["permissions"]["edit"] = serde_json::json!(true);
    let modified = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&changed).unwrap());
    let mut changed_mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&[b'x'; 64]).unwrap();
    changed_mac.update(format!("{}.{}", pieces[0], modified).as_bytes());
    assert!(
        changed_mac.verify_slice(&signature).is_err(),
        "changing viewer permissions invalidates the signed configuration"
    );
    let signed: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(pieces[1])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        signed, config,
        "JWT binds all read-only fields and the source, not a subset"
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    assert_eq!(
        client
            .get(&source)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        &versions.old.1
    );
    assert!(
        office.inputs.lock().unwrap().is_empty(),
        "viewer creation must not invoke PDF conversion"
    );
    let close_uri = format!(
        "{}/{}",
        download_uri(seeds.vault, versions.entry, versions.old.0)
            .replace("/content", "/office-viewer"),
        session.session_id
    );
    assert_eq!(
        app.call(close_viewer_request(&close_uri, &other_session))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.call(close_viewer_request(
            &close_uri.replace(&versions.old.0.to_hex(), &versions.current.0.to_hex()),
            &owner
        ))
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client.get(&source).send().await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    for _ in 0..2 {
        assert_eq!(
            app.call(close_viewer_request(&close_uri, &owner))
                .await
                .status,
            StatusCode::OK
        );
    }
    assert!(
        client.get(&source).send().await.is_err(),
        "DELETE immediately closes listener and its accepted connections"
    );
}

#[tokio::test]
async fn office_viewer_limits_sources_and_rechecks_login_revocation() {
    use vaultsyncd_protocol::generated::OfficeViewerSession;
    let fixture = Fixture::new("viewer-slots-revocation");
    let seeds = seed_accounts(&fixture).await;
    let versions = seed_versions(&fixture, &seeds, 1_700_000_000).await;
    let office = OfficeFixture::new(&fixture, 0).await;
    let app = open_app_office(&fixture, Some(Arc::clone(&office.runtime))).await;
    let owner = login(&app, "owner").await;
    let disabled = open_app(&fixture).await;
    assert_eq!(
        disabled
            .call(viewer_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                Some(&owner)
            ))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let mut sessions = Vec::new();
    for _ in 0..2 {
        let reply = app
            .call(viewer_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                Some(&owner),
            ))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
        sessions.push(parse::<OfficeViewerSession>(&reply, "viewer"));
    }
    assert_ne!(sessions[0].session_id, sessions[1].session_id);
    assert_eq!(
        app.call(viewer_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner)
        ))
        .await
        .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let close_uri = |session: &str| {
        format!(
            "{}/{}",
            download_uri(seeds.vault, versions.entry, versions.old.0)
                .replace("/content", "/office-viewer"),
            session
        )
    };
    assert_eq!(
        app.call(close_viewer_request(
            &close_uri(&sessions[0].session_id),
            &owner
        ))
        .await
        .status,
        StatusCode::OK
    );
    let reply = app
        .call(viewer_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    sessions.push(parse::<OfficeViewerSession>(&reply, "viewer"));
    let config: serde_json::Value = serde_json::from_str(&sessions[1].config_json).unwrap();
    let source = config["document"]["url"].as_str().unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client.get(source).send().await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        app.call(post("/v1/auth/logout", Some(&owner))).await.status,
        StatusCode::OK
    );
    assert_eq!(
        client.get(source).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(close_viewer_request(
            &close_uri(&sessions[1].session_id),
            &owner
        ))
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn office_viewer_expiry_closes_source_and_returns_bounded_capacity_without_browser_cleanup() {
    use vaultsyncd_protocol::generated::OfficeViewerSession;
    let fixture = Fixture::new("viewer-expiry");
    let seeds = seed_accounts(&fixture).await;
    let versions = seed_versions(&fixture, &seeds, 1_700_000_000).await;
    let office = OfficeFixture::new(&fixture, 0).await;
    let app = open_app_office(&fixture, Some(Arc::clone(&office.runtime))).await;
    let owner = login(&app, "owner").await;
    let mut sources = Vec::new();
    for _ in 0..2 {
        let reply = app
            .call(viewer_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                Some(&owner),
            ))
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        let session: OfficeViewerSession = parse(&reply, "viewer");
        let config: serde_json::Value = serde_json::from_str(&session.config_json).unwrap();
        sources.push(config["document"]["url"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        app.call(viewer_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner)
        ))
        .await
        .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    // SQLx setup and hashing run with real time; pause only the lease timeout interval.
    tokio::time::pause();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(121)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    for source in &sources {
        assert!(
            client.get(source).send().await.is_err(),
            "expired sources must stop listening without DELETE"
        );
    }
    for _ in 0..2 {
        let reply = app
            .call(viewer_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                Some(&owner),
            ))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "expiry must release both source memory slots: {}",
            reply.text()
        );
        let session: OfficeViewerSession = parse(&reply, "viewer");
        let uri = format!(
            "{}/{}",
            download_uri(seeds.vault, versions.entry, versions.old.0)
                .replace("/content", "/office-viewer"),
            session.session_id
        );
        assert_eq!(
            app.call(close_viewer_request(&uri, &owner)).await.status,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn office_preview_preserves_fixed_version_scope_integrity_and_closes_source_channel() {
    let fixture = Fixture::new("office-fixed-scope");
    let seeds = seed_accounts(&fixture).await;
    let versions = seed_versions(&fixture, &seeds, 1_700_000_000).await;
    let office = OfficeFixture::new(&fixture, 0).await;
    let app = open_app_office(&fixture, Some(Arc::clone(&office.runtime))).await;
    let owner = login(&app, "owner").await;
    let intruder = login(&app, "intruder").await;
    assert_eq!(
        app.call(office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            None
        ))
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&intruder)
        ))
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.call(office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.current.1,
            Some(&owner)
        ))
        .await
        .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        app.call(office_request(
            seeds.vault,
            versions.entry,
            VersionId::from_bytes([0x73; ID_LEN]),
            &versions.old.1,
            Some(&owner)
        ))
        .await
        .status,
        StatusCode::GONE
    );
    let uri = download_uri(seeds.vault, versions.entry, versions.old.0)
        .replace("/content", "/office-preview");
    let anonymous = app.call(post_json(&uri, None, "{")).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let malformed = app.call(post_json(&uri, Some(&owner), "{")).await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        parse::<ErrorEnvelope>(&malformed, "Office malformed envelope")
            .error
            .code,
        ErrorCode::InvalidRequest
    );
    let oversized=app.call(post_json(&uri,Some(&owner),&serde_json::json!({"file_type":"docx","hash":sha256(&versions.old.1),"size":(vaultsyncd_server::office::MAX_OFFICE_BYTES+1).to_string()}).to_string())).await;
    assert_eq!(oversized.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(office.inputs.lock().unwrap().is_empty());
    let reply = app
        .call(office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(
        reply.header("content-type").as_deref(),
        Some("application/pdf")
    );
    assert_eq!(reply.header("cache-control").as_deref(), Some("no-store"));
    assert_eq!(
        reply.header("x-preview-source-sha256").as_deref(),
        Some(sha256(&versions.old.1).as_str())
    );
    assert_eq!(
        reply.header("x-preview-source-version").as_deref(),
        Some(versions.old.0.to_hex().as_str())
    );
    assert_eq!(
        reply.header("x-content-sha256").as_deref(),
        Some(sha256(&reply.body).as_str())
    );
    assert_eq!(*office.inputs.lock().unwrap(), vec![versions.old.1.clone()]);
    let source = office.sources.lock().unwrap()[0].clone();
    assert!(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(source)
            .send()
            .await
            .is_err(),
        "source listener must close after conversion"
    );
    let path = fixture
        .objects(seeds.vault)
        .object_path(&ContentDigest::from_bytes(hash::digest_of(&versions.old.1)));
    std::fs::write(path, vec![b'x'; versions.old.1.len()]).unwrap();
    assert_eq!(
        app.call(office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner)
        ))
        .await
        .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        office.inputs.lock().unwrap().len(),
        1,
        "tampered content must not reach OnlyOffice"
    );
}

#[tokio::test]
async fn office_preview_does_not_return_foreign_urls_nonpdf_or_unconfigured_results() {
    use std::sync::atomic::Ordering;
    let fixture = Fixture::new("office-output-rejections");
    let seeds = seed_accounts(&fixture).await;
    let versions = seed_versions(&fixture, &seeds, 1_700_000_000).await;
    let office = OfficeFixture::new(&fixture, 1).await;
    let app = open_app_office(&fixture, Some(Arc::clone(&office.runtime))).await;
    let owner = login(&app, "owner").await;
    for mode in [1, 2] {
        office.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            app.call(office_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                Some(&owner)
            ))
            .await
            .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let disabled = open_app(&fixture).await;
    let token = login(&disabled, "owner").await;
    assert_eq!(
        disabled
            .call(office_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                Some(&token)
            ))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn office_preview_rechecks_revocation_before_source_and_before_pdf_response() {
    for mode in [3, 4] {
        let fixture = Fixture::new(&format!("office-revoke-{mode}"));
        let seeds = seed_accounts(&fixture).await;
        let versions = seed_versions(&fixture, &seeds, 1_700_000_000).await;
        let office = OfficeFixture::new(&fixture, mode).await;
        let app = open_app_office(&fixture, Some(Arc::clone(&office.runtime))).await;
        let owner = login(&app, "owner").await;
        let request = office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner),
        );
        let router = app.router.clone();
        let pending = tokio::spawn(async move { router.oneshot(request).await.unwrap() });
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            office.arrived.notified(),
        )
        .await
        .unwrap();
        assert_eq!(
            app.call(post("/v1/auth/logout", Some(&owner))).await.status,
            StatusCode::OK
        );
        office.gate.add_permits(1);
        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap();
        if mode == 3 {
            assert_eq!(reply.status(), StatusCode::UNAUTHORIZED);
            assert!(
                office.inputs.lock().unwrap().is_empty(),
                "revoked source request must not expose bytes"
            );
        } else {
            assert_eq!(
                reply.status(),
                StatusCode::UNAUTHORIZED,
                "revocation after source read must deny final PDF"
            );
        }
    }
}

#[tokio::test]
async fn office_cancelling_http_does_not_free_slots_before_upstream_workers_finish() {
    use std::sync::atomic::Ordering;
    let fixture = Fixture::new("office-cancel-slots");
    let seeds = seed_accounts(&fixture).await;
    let versions = seed_versions(&fixture, &seeds, 1_700_000_000).await;
    let office = OfficeFixture::new(&fixture, 3).await;
    let app = open_app_office(&fixture, Some(Arc::clone(&office.runtime))).await;
    let owner = login(&app, "owner").await;
    let mut pending = Vec::new();
    for _ in 0..2 {
        let router = app.router.clone();
        let request = office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner),
        );
        pending.push(tokio::spawn(async move {
            router.oneshot(request).await.unwrap()
        }));
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            office.arrived.notified(),
        )
        .await
        .unwrap();
    }
    for task in pending {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    let reply = app
        .call(office_request(
            seeds.vault,
            versions.entry,
            versions.old.0,
            &versions.old.1,
            Some(&owner),
        ))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "cancelled callers cannot bypass in-flight worker capacity"
    );
    assert_eq!(office.sources.lock().unwrap().len(), 2);
    office.mode.store(0, Ordering::SeqCst);
    office.gate.add_permits(2);
    let mut restored = false;
    for _ in 0..30 {
        let reply = app
            .call(office_request(
                seeds.vault,
                versions.entry,
                versions.old.0,
                &versions.old.1,
                Some(&owner),
            ))
            .await;
        if reply.status == StatusCode::OK {
            restored = true;
            break;
        }
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(restored, "completed worker capacity must be released");
}
