//! S048：一致性快照、`changes` 与 SSE 的 HTTP 级验收（design §9.3—9.4、I09）。
//!
//! 用例走真实 router、真实 SQLite：条目与批次都用 `server-store` 的真实 API 写入（不是假端口），
//! 断言的是端点实际返回的状态码与响应体：
//!
//! 1. 边写边翻页：翻页期间新增、删除、改名都不进入已固定的快照清单，也不让已出现的条目消失；
//! 2. 同一事务的多个批次/多条事件不会被 `limit` 切到两页之间；
//! 3. SSE 只是提示：丢失、重复、乱序之后，客户端凭 `changes` 的游标仍修复到一致状态；
//! 4. 过期游标（早于保留窗口、晚于当前头）与过期快照是 410，不是 200 空集；
//! 5. 401/403 回归：四个端点都不放过无凭据与越权。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt as _;
use tower::ServiceExt;
use vaultsyncd_domain::ids::{EntryId, ID_LEN, RootId, TxId, UserId};
use vaultsyncd_domain::version::{Revision, ServerEpoch, UtcTimestamp};
use vaultsyncd_protocol::generated::{
    ChangeList, ErrorCode, ErrorEnvelope, LoginRequest, SeqHint, SessionIssued, Snapshot,
    SnapshotPage,
};
use vaultsyncd_server::auth::AuthContext;
use vaultsyncd_server::content::ContentContext;
use vaultsyncd_server::http::router;
use vaultsyncd_server::lifecycle::Lifecycle;
use vaultsyncd_server::operations::OperationsContext;
use vaultsyncd_server::roots::RootContext;
use vaultsyncd_server::sync::{SyncConfig, SyncContext};
use vaultsyncd_server::uploads::UploadsContext;
use vaultsyncd_server_core::api::{events, snapshots};
use vaultsyncd_server_core::auth::password::PasswordHasher;
use vaultsyncd_server_store::changes::{self, ChangeBatch};
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::entries::{self, EntryKind, NewEntry};
use vaultsyncd_server_store::roots::{self as store_roots, NewRoot};
use vaultsyncd_server_store::users;

const PASSWORD: &str = "correct horse battery staple";
const OWNER: UserId = UserId::from_bytes([0x11; ID_LEN]);
const INTRUDER: UserId = UserId::from_bytes([0x33; ID_LEN]);
const ROOT: RootId = RootId::from_bytes([0x22; ID_LEN]);
const FOREIGN_ROOT: RootId = RootId::from_bytes([0x66; ID_LEN]);
const ROOT_ENTRY: EntryId = EntryId::from_bytes([0x55; ID_LEN]);
/// 事件内容里的标记：SSE 提示的任何一帧都不能出现它（提示不携带事件内容）。
const SECRET_MARKER: &str = "secret-event-payload-marker";

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("vaultsyncd-s048-{}-{label}", std::process::id()));
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

fn now() -> UtcTimestamp {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时间晚于 1970")
        .as_secs();
    UtcTimestamp::from_unix_seconds(i64::try_from(seconds).expect("秒数放得进 i64"))
        .expect("时间合法")
}

fn at(seconds: i64) -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(seconds).expect("时间合法")
}

fn tx_id(marker: u8) -> TxId {
    TxId::from_bytes([marker; ID_LEN])
}

fn file_id(marker: u8) -> EntryId {
    EntryId::from_bytes([marker; ID_LEN])
}

struct App {
    router: Router,
    database: Database,
    lifecycle: Arc<Lifecycle>,
}

/// 种子：owner 有一个 Root（带合成根条目），intruder 有会话与自己的 Root（越权 403）。
async fn open_app(fixture: &Fixture) -> App {
    let database = Database::open(fixture.database())
        .await
        .expect("打开测试库");
    let hasher = PasswordHasher::with_params(8, 1, 1).expect("测试 Argon2id 参数");
    let password_hash = hasher.hash(PASSWORD).expect("散列种子密码");
    let mut tx = database.begin_write().await.expect("写事务");
    for (id, login) in [(OWNER, "owner"), (INTRUDER, "intruder")] {
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
    for (id, entry, path, owner) in [
        (ROOT, ROOT_ENTRY, "/srv/s048", OWNER),
        (
            FOREIGN_ROOT,
            EntryId::from_bytes([0x99; ID_LEN]),
            "/srv/s048-intruder",
            INTRUDER,
        ),
    ] {
        store_roots::insert_root(
            &mut tx,
            &NewRoot {
                id,
                entry,
                owner,
                absolute_path: path.to_owned(),
                volume_identity: vec![0x77; 16],
                root_file_identity: vec![0x88; 16],
                epoch: ServerEpoch::new(1).expect("epoch"),
            },
        )
        .await
        .expect("写种子 Root");
    }
    tx.commit().await.expect("提交种子事务");

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
    let executor = Arc::new(vaultsyncd_server::executor::ExecutorContext::new(
        auth.database().clone(),
        fixture.state_dir().as_path(),
    ));
    let operations = Arc::new(OperationsContext::new(Arc::clone(&auth), executor));
    let lifecycle = Arc::new(Lifecycle::new());
    lifecycle.mark_ready();
    let sync = Arc::new(SyncContext::with_config(
        Arc::clone(&auth),
        Arc::clone(&lifecycle),
        SyncConfig {
            sse_poll_interval: Duration::from_millis(15),
            ..SyncConfig::default()
        },
    ));
    App {
        router: router(
            Arc::clone(&lifecycle),
            auth,
            uploads,
            roots,
            content,
            operations,
            sync,
        ),
        database,
        lifecycle,
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
        let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("读响应体")
            .to_vec();
        (status, body)
    }

    async fn get(&self, uri: &str, token: &str) -> (StatusCode, Vec<u8>) {
        self.call(send("GET", uri, Some(token))).await
    }

    /// 向 owner 的 Root 追加一批事务批次，每个 `(tx, 事件数)` 一行，返回分配到的 seq。
    async fn append(&self, batches: &[(u8, usize)], created_at: UtcTimestamp) -> Vec<u64> {
        let mut tx = self.database.begin_write().await.expect("写事务");
        let mut seqs = Vec::new();
        for (marker, events) in batches {
            let batch: ChangeBatch = changes::append_change(
                &mut tx,
                OWNER,
                ROOT,
                tx_id(*marker),
                &events_json(*marker, *events),
                created_at,
            )
            .await
            .expect("追加批次");
            seqs.push(batch.seq.get());
        }
        tx.commit().await.expect("提交批次");
        seqs
    }

    /// 新增一个文件条目（名称同时是比较键）。
    async fn add_file(&self, marker: u8, name: &str) {
        let mut tx = self.database.begin_write().await.expect("写事务");
        entries::insert_entry(
            &mut tx,
            OWNER,
            &NewEntry {
                id: file_id(marker),
                root: ROOT,
                parent: ROOT_ENTRY,
                name: name.to_owned(),
                name_key: name.to_owned(),
                kind: EntryKind::File,
                revision: Revision::new(1).expect("revision"),
                content_version: None,
            },
        )
        .await
        .expect("写条目");
        tx.commit().await.expect("提交条目");
    }

    async fn rename_file(&self, marker: u8, name: &str) {
        let mut tx = self.database.begin_write().await.expect("写事务");
        entries::move_entry(
            &mut tx,
            OWNER,
            file_id(marker),
            ROOT_ENTRY,
            name,
            name,
            Revision::new(2).expect("revision"),
        )
        .await
        .expect("改名");
        tx.commit().await.expect("提交改名");
    }

    async fn delete_file(&self, marker: u8) {
        let mut tx = self.database.begin_write().await.expect("写事务");
        entries::soft_delete_entry(&mut tx, OWNER, file_id(marker), at(10_000))
            .await
            .expect("软删除");
        tx.commit().await.expect("提交删除");
    }

    async fn login(&self, account: &str) -> String {
        let request = LoginRequest {
            device_label: "S048 验收".to_owned(),
            login: account.to_owned(),
            password: PASSWORD.to_owned(),
        };
        let body = serde_json::to_string(&request).expect("序列化登录请求");
        let (status, bytes) = self
            .call(
                Request::builder()
                    .method("POST")
                    .uri("/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .expect("构造请求"),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "登录应成功：{}", text(&bytes));
        serde_json::from_slice::<SessionIssued>(&bytes)
            .expect("解析会话")
            .access_token
    }
}

/// 一批事件的 JSON 数组文本：`count` 条事件，都带批次标记与内容标记。
fn events_json(marker: u8, count: usize) -> String {
    let events: Vec<String> = (0..count)
        .map(|index| {
            format!(
                r#"{{"kind":"entry_changed","tx":{marker},"n":{index},"note":"{SECRET_MARKER}"}}"#
            )
        })
        .collect();
    format!("[{}]", events.join(","))
}

fn text(body: &[u8]) -> String {
    String::from_utf8(body.to_vec()).expect("响应体是 UTF-8")
}

fn envelope(body: &[u8]) -> ErrorEnvelope {
    serde_json::from_slice(body)
        .unwrap_or_else(|error| panic!("解析错误信封失败：{error}：{}", text(body)))
}

fn send(method: &str, uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("构造请求")
}

fn snapshots_uri(root: RootId) -> String {
    format!("/v1/roots/{}/snapshots", root.to_hex())
}

fn snapshot_entries_uri(root: RootId, snapshot: &str, query: &str) -> String {
    format!("{}/{snapshot}/entries{query}", snapshots_uri(root))
}

fn changes_uri(root: RootId, query: &str) -> String {
    format!("/v1/roots/{}/changes{query}", root.to_hex())
}

fn events_uri(root: RootId) -> String {
    format!("/v1/roots/{}/events", root.to_hex())
}

async fn create_snapshot(app: &App, token: &str) -> Snapshot {
    let (status, body) = app
        .call(send("POST", &snapshots_uri(ROOT), Some(token)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "创建快照：{}", text(&body));
    serde_json::from_slice(&body).expect("解析快照")
}

async fn snapshot_page(app: &App, token: &str, id: &str, query: &str) -> SnapshotPage {
    let (status, body) = app.get(&snapshot_entries_uri(ROOT, id, query), token).await;
    assert_eq!(status, StatusCode::OK, "快照翻页：{}", text(&body));
    serde_json::from_slice(&body).expect("解析快照页")
}

async fn changes_page(app: &App, token: &str, query: &str) -> ChangeList {
    let (status, body) = app.get(&changes_uri(ROOT, query), token).await;
    assert_eq!(status, StatusCode::OK, "拉取 changes：{}", text(&body));
    serde_json::from_slice(&body).expect("解析 ChangeList")
}

// ───────────────────────── 1. 边写边翻页的快照一致性 ─────────────────────────

#[tokio::test]
async fn paging_a_snapshot_while_the_namespace_changes_stays_consistent() {
    let fixture = Fixture::new("snapshot-consistent");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    for marker in 0x30..0x37 {
        app.add_file(marker, &format!("file-{marker:02x}.txt"))
            .await;
    }
    app.append(&[(0x01, 1)], now()).await;

    // 创建快照：7 个文件 + 合成根条目。
    let snapshot = create_snapshot(&app, &token).await;
    assert_eq!(snapshot.entry_count, "8");
    assert_eq!(
        snapshot.high_water_seq, "1",
        "固定的 high_water_seq 就是创建时的 RootSeq"
    );
    assert_eq!(snapshot.server_epoch, "1");
    assert_eq!(snapshot.root_id, ROOT.to_hex());
    let expected: BTreeSet<String> = std::iter::once(ROOT_ENTRY)
        .chain((0x30..0x37).map(file_id))
        .map(|id| id.to_hex())
        .collect();

    let first = snapshot_page(&app, &token, &snapshot.snapshot_id, "?limit=3").await;
    assert_eq!(first.entries.len(), 3);
    let cursor = first.next_cursor.clone().expect("还有后续页");
    assert_eq!(cursor, "3");

    // 翻页期间的写入：新增、删除（一个已翻过、一个还没翻到）、改名（还没翻到的）、再追加批次。
    app.add_file(0x40, "added-during-paging.txt").await;
    let seen_first: Vec<String> = first.entries.iter().map(|e| e.entry_id.clone()).collect();
    let seen_marker = (0x30..0x37)
        .find(|marker| seen_first.contains(&file_id(*marker).to_hex()))
        .expect("首页至少有一个文件");
    let unseen_marker = (0x30..0x37)
        .rev()
        .find(|marker| !seen_first.contains(&file_id(*marker).to_hex()))
        .expect("还有没翻到的文件");
    app.delete_file(seen_marker).await;
    app.rename_file(unseen_marker, "renamed-during-paging.txt")
        .await;
    app.append(&[(0x02, 2)], now()).await;

    // 继续翻完：用同一个 snapshot_id 与上一页给出的游标。
    let mut pages = vec![first];
    let mut next = Some(cursor);
    while let Some(cursor) = next {
        let page = snapshot_page(
            &app,
            &token,
            &snapshot.snapshot_id,
            &format!("?cursor={cursor}&limit=3"),
        )
        .await;
        assert_eq!(
            page.snapshot_id, snapshot.snapshot_id,
            "所有页引用同一个 snapshot_id"
        );
        assert_eq!(
            page.high_water_seq, "1",
            "所有页的 high_water_seq 不随写入变化"
        );
        next = page.next_cursor.clone();
        pages.push(page);
    }
    assert_eq!(pages.last().expect("末页").next_cursor, None);

    let all: Vec<&vaultsyncd_protocol::generated::SnapshotEntry> =
        pages.iter().flat_map(|page| page.entries.iter()).collect();
    let ids: Vec<String> = all.iter().map(|entry| entry.entry_id.clone()).collect();
    assert_eq!(ids.len(), 8, "没有漏页、没有重复");
    assert_eq!(
        ids.iter().cloned().collect::<BTreeSet<_>>(),
        expected,
        "恰为创建时刻的 8 条：新增不出现、被删除的不消失"
    );
    assert!(
        !ids.contains(&file_id(0x40).to_hex()),
        "翻页期间新增的条目不得出现在快照里"
    );
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "按 EntryId 升序，页间无回退");
    let renamed = all
        .iter()
        .find(|entry| entry.entry_id == file_id(unseen_marker).to_hex())
        .expect("被改名的条目仍在");
    assert_eq!(
        renamed.name,
        format!("file-{unseen_marker:02x}.txt"),
        "快照里是创建时刻的名字，不是改名后的"
    );
    let root_row = all
        .iter()
        .find(|entry| entry.entry_id == ROOT_ENTRY.to_hex())
        .expect("含合成根条目");
    assert_eq!(
        (root_row.parent_id.as_deref(), root_row.name.as_str()),
        (None, "")
    );

    // 不是空转：此刻新建的快照确实反映了这些写入（新增在、删除不在、改名生效）。
    let later = create_snapshot(&app, &token).await;
    assert_eq!(
        later.entry_count, "8",
        "8 + 1 新增 - 1 删除 = 8，但内容不同"
    );
    assert_eq!(later.high_water_seq, "2");
    let later_page = snapshot_page(&app, &token, &later.snapshot_id, "").await;
    let later_ids: BTreeSet<String> = later_page
        .entries
        .iter()
        .map(|entry| entry.entry_id.clone())
        .collect();
    assert!(later_ids.contains(&file_id(0x40).to_hex()));
    assert!(!later_ids.contains(&file_id(seen_marker).to_hex()));
    assert!(
        later_page
            .entries
            .iter()
            .any(|entry| entry.name == "renamed-during-paging.txt")
    );
    assert_ne!(later_ids, expected);
}

#[tokio::test]
async fn snapshot_paging_has_a_hard_upper_bound_and_rejects_bad_queries() {
    let fixture = Fixture::new("snapshot-limits");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    let snapshot = create_snapshot(&app, &token).await;
    let id = &snapshot.snapshot_id;

    let over = snapshots::MAX_SNAPSHOT_PAGE + 1;
    for query in [
        format!("?limit={over}"),
        "?limit=0".to_owned(),
        "?limit=abc".to_owned(),
        "?limit=007".to_owned(),
        "?cursor=-1".to_owned(),
        "?cursor=999".to_owned(),
        "?unknown=1".to_owned(),
        "?limit=1&limit=2".to_owned(),
    ] {
        let (status, body) = app
            .get(&snapshot_entries_uri(ROOT, id, &query), &token)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}：{}", text(&body));
        assert_eq!(
            envelope(&body).error.code,
            ErrorCode::InvalidRequest,
            "{query}"
        );
    }
    let (status, _) = app
        .get(
            &snapshot_entries_uri(
                ROOT,
                id,
                &format!("?limit={}", snapshots::MAX_SNAPSHOT_PAGE),
            ),
            &token,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "上限本身合法");
    assert_eq!(
        snapshots::MAX_SNAPSHOT_PAGE,
        vaultsyncd_server_store::snapshots::MAX_SNAPSHOT_PAGE,
        "核心上限与存储层上限同值"
    );
}

#[tokio::test]
async fn an_expired_or_unknown_snapshot_is_410_not_an_empty_page() {
    let fixture = Fixture::new("snapshot-expired");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    app.add_file(0x30, "a.txt").await;
    let snapshot = create_snapshot(&app, &token).await;
    snapshot_page(&app, &token, &snapshot.snapshot_id, "").await;

    // 把清单推到过期（创建时间与过期时间都在遥远的过去）。
    sqlx::query("UPDATE snapshots SET created_at = 1, expires_at = 2")
        .execute(app.database.pool())
        .await
        .expect("使快照过期");
    for id in [snapshot.snapshot_id.clone(), "ab".repeat(16)] {
        let (status, body) = app.get(&snapshot_entries_uri(ROOT, &id, ""), &token).await;
        assert_eq!(
            status,
            StatusCode::GONE,
            "过期/未知快照必须是 410，实际 {status}：{}",
            text(&body)
        );
        let error = envelope(&body).error;
        assert_eq!(error.code, ErrorCode::CursorExpired);
        assert!(!error.retryable, "失效的快照原样重试不会复活");
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(|d| d.get("reason"))
                .map(String::as_str),
            Some("snapshot_expired")
        );
    }

    // 别人的账号拿到快照身份也读不到：它自己的 Root 不含该快照 → 410；对方 Root → 403。
    let intruder = app.login("intruder").await;
    let (status, _) = app
        .get(
            &snapshot_entries_uri(FOREIGN_ROOT, &snapshot.snapshot_id, ""),
            &intruder,
        )
        .await;
    assert_eq!(status, StatusCode::GONE);
    let (status, _) = app
        .get(
            &snapshot_entries_uri(ROOT, &snapshot.snapshot_id, ""),
            &intruder,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // 重新创建后恢复可用。
    let fresh = create_snapshot(&app, &token).await;
    assert_ne!(fresh.snapshot_id, snapshot.snapshot_id);
    assert_eq!(
        snapshot_page(&app, &token, &fresh.snapshot_id, "")
            .await
            .entries
            .len(),
        2
    );
}

// ───────────────────────── 2. changes：有序、事务批次不被拆页 ─────────────────────────

#[tokio::test]
async fn changes_are_ordered_and_a_transaction_is_never_split_across_pages() {
    let fixture = Fixture::new("changes-whole-tx");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;

    // seq 1：独立事务（3 条事件）；seq 2、3、4：同一事务 0x02 的三个连续批次；seq 5：事务 0x03。
    let seqs = app
        .append(
            &[(0x01, 3), (0x02, 4), (0x02, 5), (0x02, 6), (0x03, 1)],
            now(),
        )
        .await;
    assert_eq!(seqs, vec![1, 2, 3, 4, 5]);

    // limit=2：朴素切法会停在 seq 2，把事务 0x02 切成两半；整事务切法延伸到 seq 4。
    let first = changes_page(&app, &token, "?after=0&limit=2").await;
    let first_seqs: Vec<&str> = first.batches.iter().map(|b| b.seq.as_str()).collect();
    assert_eq!(
        first_seqs,
        vec!["1", "2", "3", "4"],
        "同事务的三个批次在同一页"
    );
    assert!(first.has_more);
    assert_eq!(first.next_after, "4");
    assert_eq!(first.head_seq, "5");
    assert_eq!(first.root_id, ROOT.to_hex());
    assert_eq!(first.server_epoch, "1");
    // 批次内容完整：事件条数与写入时一致，TxId 对应事务。
    let counts: Vec<usize> = first
        .batches
        .iter()
        .map(|batch| {
            serde_json::from_str::<Vec<serde_json::Value>>(&batch.events_json)
                .expect("events_json 是数组")
                .len()
        })
        .collect();
    assert_eq!(counts, vec![3, 4, 5, 6]);
    assert_eq!(first.batches[1].tx_id, first.batches[3].tx_id);
    assert_ne!(first.batches[0].tx_id, first.batches[1].tx_id);

    let second = changes_page(&app, &token, "?after=4&limit=2").await;
    assert_eq!(second.batches.len(), 1);
    assert_eq!(second.batches[0].seq, "5");
    assert!(!second.has_more);
    assert_eq!(second.next_after, "5");

    // limit=1 恰落在事务边界：不多取也不少取；逐页拼起来恰好是全部批次，不丢不重。
    let mut cursor = "0".to_owned();
    let mut collected = Vec::new();
    loop {
        let page = changes_page(&app, &token, &format!("?after={cursor}&limit=1")).await;
        collected.extend(page.batches.iter().map(|b| b.seq.clone()));
        assert!(
            collected
                .windows(2)
                .all(|w| w[0].parse::<u64>().unwrap() < w[1].parse::<u64>().unwrap()),
            "严格按 seq 升序：{collected:?}"
        );
        cursor = page.next_after.clone();
        if !page.has_more {
            break;
        }
    }
    assert_eq!(collected, vec!["1", "2", "3", "4", "5"]);

    // 一个批次里的多条事件整批返回，即使 limit=1：不会只给一部分事件。
    app.append(&[(0x04, 40)], now()).await;
    let big = changes_page(&app, &token, "?after=5&limit=1").await;
    assert_eq!(big.batches.len(), 1);
    let events: Vec<serde_json::Value> =
        serde_json::from_str(&big.batches[0].events_json).expect("数组");
    assert_eq!(events.len(), 40, "同一事务的多条变更不被拆页");

    // 已追平：after = 头是合法的空页（has_more=false），不是错误。
    let caught_up = changes_page(&app, &token, "?after=6").await;
    assert!(caught_up.batches.is_empty());
    assert!(!caught_up.has_more);
    assert_eq!(caught_up.next_after, "6");
}

#[tokio::test]
async fn changes_rejects_bad_queries_and_the_limit_has_a_hard_bound() {
    let fixture = Fixture::new("changes-limits");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    app.append(&[(0x01, 1)], now()).await;

    let over = events::MAX_CHANGES_LIMIT + 1;
    for query in [
        String::new(),
        "?limit=10".to_owned(),
        "?after=abc".to_owned(),
        "?after=01".to_owned(),
        "?after=-1".to_owned(),
        "?after=0&limit=0".to_owned(),
        format!("?after=0&limit={over}"),
        "?after=0&extra=1".to_owned(),
        "?after=0&after=1".to_owned(),
    ] {
        let (status, body) = app.get(&changes_uri(ROOT, &query), &token).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{query:?}：{}",
            text(&body)
        );
        assert_eq!(
            envelope(&body).error.code,
            ErrorCode::InvalidRequest,
            "{query:?}"
        );
    }
    let (status, _) = app
        .get(
            &changes_uri(
                ROOT,
                &format!("?after=0&limit={}", events::MAX_CHANGES_LIMIT),
            ),
            &token,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "上限本身合法");
    assert_eq!(
        events::MAX_CHANGES_LIMIT,
        vaultsyncd_server_store::changes::MAX_CHANGE_PAGE,
        "核心上限与存储层上限同值"
    );
}

// ───────────────────────── 3. 游标过期是明确的 410，不是假空集 ─────────────────────────

#[tokio::test]
async fn a_cursor_older_than_the_retention_window_is_410_never_an_empty_page() {
    let fixture = Fixture::new("cursor-expired");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    // seq 1—3 创建于 1970 年（远超 90 天保留期），seq 4—5 是刚创建的。
    app.append(&[(0x01, 1), (0x02, 1), (0x03, 1)], at(1_000))
        .await;
    app.append(&[(0x04, 1), (0x05, 1)], now()).await;

    for after in ["0", "1", "2"] {
        let (status, body) = app
            .get(&changes_uri(ROOT, &format!("?after={after}")), &token)
            .await;
        assert_eq!(
            status,
            StatusCode::GONE,
            "after={after} 早于保留窗口必须 410，不是 200 空集：{}",
            text(&body)
        );
        assert!(
            serde_json::from_slice::<ChangeList>(&body).is_err(),
            "410 的响应体不是 ChangeList（没有假空集）：{}",
            text(&body)
        );
        let error = envelope(&body).error;
        assert_eq!(error.code, ErrorCode::CursorExpired);
        assert!(!error.retryable);
        let details = error.details.expect("带结构化原因");
        assert_eq!(
            details.get("reason").map(String::as_str),
            Some("before_retention_window")
        );
        assert_eq!(
            details.get("expired_through").map(String::as_str),
            Some("3")
        );
        assert_eq!(error.root_id.as_deref(), Some(ROOT.to_hex().as_str()));
    }

    // 下沿本身合法：要的是 seq 3 之后的批次，仍然全部保留。
    let page = changes_page(&app, &token, "?after=3").await;
    let seqs: Vec<&str> = page.batches.iter().map(|b| b.seq.as_str()).collect();
    assert_eq!(seqs, vec!["4", "5"]);
}

#[tokio::test]
async fn a_cursor_ahead_of_the_head_is_410_not_a_caught_up_empty_page() {
    let fixture = Fixture::new("cursor-ahead");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    app.append(&[(0x01, 1), (0x02, 1)], now()).await;

    let (status, body) = app.get(&changes_uri(ROOT, "?after=99"), &token).await;
    assert_eq!(status, StatusCode::GONE, "{}", text(&body));
    let error = envelope(&body).error;
    assert_eq!(error.code, ErrorCode::CursorExpired);
    let details = error.details.expect("带结构化原因");
    assert_eq!(
        details.get("reason").map(String::as_str),
        Some("ahead_of_head")
    );
    assert_eq!(details.get("head_seq").map(String::as_str), Some("2"));
}

// ───────────────────────── 4. SSE 只是提示：丢/重/乱序后 changes 修复 ─────────────────────────

/// 极简 SSE 读取器：按空行切帧，保活注释帧单独计数，不当作提示。
struct SseReader {
    stream: axum::body::BodyDataStream,
    buffer: String,
}

impl SseReader {
    async fn open(app: &App, token: &str) -> (StatusCode, Option<Self>, Option<String>) {
        let response = app
            .router
            .clone()
            .oneshot(send("GET", &events_uri(ROOT), Some(token)))
            .await
            .expect("调用处理器");
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|value| value.to_str().expect("头是 ASCII").to_owned());
        let reader = status.is_success().then(|| Self {
            stream: response.into_body().into_data_stream(),
            buffer: String::new(),
        });
        (status, reader, content_type)
    }

    /// 读下一帧原文（不含结尾空行）；流结束返回 `None`，超时 panic（提示必须及时到达）。
    async fn next_frame(&mut self, wait: Duration) -> Option<String> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let frame = self.buffer[..end].to_owned();
                self.buffer.drain(..end + 2);
                return Some(frame);
            }
            let chunk = tokio::time::timeout(wait, self.stream.next())
                .await
                .expect("SSE 在预期时间内没有新数据");
            match chunk {
                Some(bytes) => {
                    let bytes = bytes.expect("读 SSE 数据");
                    self.buffer
                        .push_str(std::str::from_utf8(&bytes).expect("SSE 是 UTF-8"));
                }
                None => return None,
            }
        }
    }

    /// 读到下一条 `seq` 提示；返回 (帧原文, 解析后的提示)。
    async fn next_hint(&mut self, wait: Duration) -> Option<(String, SeqHint)> {
        loop {
            let frame = self.next_frame(wait).await?;
            if frame.starts_with(':') {
                continue;
            }
            let mut event = None;
            let mut data = None;
            let mut id = None;
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("id:") {
                    id = Some(value.trim().to_owned());
                }
            }
            assert_eq!(event.as_deref(), Some("seq"), "只有 seq 提示：{frame}");
            let hint: SeqHint =
                serde_json::from_str(&data.expect("提示带 data")).expect("data 是 SeqHint");
            assert_eq!(
                id.as_deref(),
                Some(hint.seq.as_str()),
                "id 与 data.seq 一致"
            );
            return Some((frame, hint));
        }
    }
}

/// 把 `changes` 当作唯一数据来源的客户端：按 seq 去重，只接受“恰为下一个”的批次。
#[derive(Default)]
struct Replica {
    applied: Vec<(u64, String)>,
}

impl Replica {
    fn cursor(&self) -> u64 {
        self.applied.last().map_or(0, |(seq, _)| *seq)
    }

    /// 拉取直到追平；返回本次新应用的批次数。重复/过时批次被丢弃，出现缺口立即报错。
    async fn pull(&mut self, app: &App, token: &str) -> usize {
        let mut applied_now = 0;
        loop {
            let page = changes_page(app, token, &format!("?after={}&limit=2", self.cursor())).await;
            for batch in &page.batches {
                let seq: u64 = batch.seq.parse().expect("seq 是十进制");
                if seq <= self.cursor() {
                    continue;
                }
                assert_eq!(
                    seq,
                    self.cursor() + 1,
                    "批次必须连续，出现缺口说明 changes 漏了数据"
                );
                self.applied.push((seq, batch.events_json.clone()));
                applied_now += 1;
            }
            if !page.has_more {
                return applied_now;
            }
        }
    }

    /// 收到一条提示：只有提示的 seq 比本地游标新才值得拉取（旧提示、重复提示都无害）。
    async fn on_hint(&mut self, app: &App, token: &str, hint: &SeqHint) -> usize {
        let seq: u64 = hint.seq.parse().expect("seq 是十进制");
        if seq <= self.cursor() {
            return 0;
        }
        self.pull(app, token).await
    }
}

#[tokio::test]
async fn sse_hints_carry_no_data_and_lost_duplicated_reordered_hints_are_repaired_by_changes() {
    let fixture = Fixture::new("sse-repair");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    let wait = Duration::from_secs(5);

    let (status, reader, content_type) = SseReader::open(&app, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type.as_deref(), Some("text/event-stream"));
    let mut sse = reader.expect("事件流");

    // 新连接立即给出当前头（空 Root 为 0）。
    let (frame, hint) = sse.next_hint(wait).await.expect("连接即提示");
    assert_eq!(hint.seq, "0");
    assert_eq!(hint.root_id, ROOT.to_hex());
    let mut all_frames = vec![frame];

    let mut replica = Replica::default();

    // 第一轮写入：提示正常送达 → 客户端拉取到 seq 3。
    app.append(&[(0x01, 2), (0x02, 1), (0x03, 3)], now()).await;
    let (frame, hint1) = sse.next_hint(wait).await.expect("第一轮提示");
    all_frames.push(frame);
    assert_eq!(hint1.seq, "3");
    assert_eq!(replica.on_hint(&app, &token, &hint1).await, 3);

    // 第二轮写入：提示被丢弃（客户端没有收到）。服务端头已前进，客户端落后——这就是“丢”的后果。
    app.append(&[(0x04, 1), (0x05, 2)], now()).await;
    let (frame, lost) = sse.next_hint(wait).await.expect("第二轮提示（将被丢弃）");
    all_frames.push(frame);
    assert_eq!(lost.seq, "5");
    assert_eq!(replica.cursor(), 3, "提示丢失后客户端落后于服务端头");

    // 第三轮写入：提示重复且乱序到达——先到一条陈旧提示（hint1），再到新提示两次。
    app.append(&[(0x06, 1), (0x07, 4)], now()).await;
    let (frame, hint3) = sse.next_hint(wait).await.expect("第三轮提示");
    all_frames.push(frame);
    assert_eq!(hint3.seq, "7");
    assert_eq!(
        replica.on_hint(&app, &token, &hint1).await,
        0,
        "陈旧提示（乱序）不触发拉取"
    );
    let caught_up = replica.on_hint(&app, &token, &hint3).await;
    assert_eq!(caught_up, 4, "一条新提示就把丢失那一轮的 seq 4、5 一并修复");
    assert_eq!(
        replica.on_hint(&app, &token, &hint3).await,
        0,
        "重复提示无害、不重复应用"
    );
    assert_eq!(replica.cursor(), 7);

    // 第四轮写入：提示再次丢失，没有任何后续提示——靠兜底轮询修复。
    app.append(&[(0x08, 1)], now()).await;
    let (frame, _lost_again) = sse.next_hint(wait).await.expect("第四轮提示（将被丢弃）");
    all_frames.push(frame);
    assert_eq!(replica.cursor(), 7);
    assert_eq!(
        replica.pull(&app, &token).await,
        1,
        "断线/丢提示期间只轮询 changes 即可恢复"
    );

    // 最终一致：本地副本恰为服务端的全部批次，每个 seq 一次、严格连续、内容逐字节相同。
    let server: Vec<(i64, String)> =
        sqlx::query_as("SELECT seq, events_json FROM changes WHERE root_id = ?1 ORDER BY seq")
            .bind(ROOT.as_bytes().as_slice())
            .fetch_all(app.database.pool())
            .await
            .expect("读服务端批次");
    assert_eq!(server.len(), 8);
    let local: Vec<(i64, String)> = replica
        .applied
        .iter()
        .map(|(seq, events)| (i64::try_from(*seq).expect("seq"), events.clone()))
        .collect();
    assert_eq!(
        local, server,
        "丢/重/乱序之后 changes 仍把副本修复到与服务端一致"
    );

    // SSE 只是提示：任何一帧都不含事件内容，也不含批次字段。
    for frame in &all_frames {
        assert!(
            !frame.contains(SECRET_MARKER),
            "提示泄露了事件内容：{frame}"
        );
        assert!(
            !frame.contains("events_json") && !frame.contains("tx_id"),
            "{frame}"
        );
    }
}

#[tokio::test]
async fn sse_ends_when_the_server_starts_draining() {
    let fixture = Fixture::new("sse-draining");
    let app = open_app(&fixture).await;
    let token = app.login("owner").await;
    let (_, reader, _) = SseReader::open(&app, &token).await;
    let mut sse = reader.expect("事件流");
    sse.next_hint(Duration::from_secs(5))
        .await
        .expect("连接即提示");

    app.lifecycle.begin_shutdown();
    // 流在下一轮轮询时主动结束，不拖住优雅停止。
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while sse.next_frame(Duration::from_secs(5)).await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "draining 之后事件流必须结束");
}

// ───────────────────────── 5. 401 / 403 回归 ─────────────────────────

#[tokio::test]
async fn every_endpoint_requires_a_session_and_stays_inside_the_callers_roots() {
    let fixture = Fixture::new("auth");
    let app = open_app(&fixture).await;
    let owner = app.login("owner").await;
    let intruder = app.login("intruder").await;
    let snapshot = create_snapshot(&app, &owner).await;

    let endpoints: Vec<(&str, String)> = vec![
        ("POST", snapshots_uri(ROOT)),
        ("GET", snapshot_entries_uri(ROOT, &snapshot.snapshot_id, "")),
        ("GET", changes_uri(ROOT, "?after=0")),
        ("GET", events_uri(ROOT)),
    ];
    for (method, uri) in &endpoints {
        // 无凭据与伪造凭据：401。
        let (status, body) = app.call(send(method, uri, None)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} 无凭据：{}",
            text(&body)
        );
        assert_eq!(envelope(&body).error.code, ErrorCode::AuthExpired);
        let (status, _) = app.call(send(method, uri, Some("not-a-real-token"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} 伪造凭据");
        // 别人的账号访问 owner 的 Root：403，且不泄露存在性（与不存在的 Root 同一回答）。
        let (status, body) = app.call(send(method, uri, Some(&intruder))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} {uri} 越权：{}",
            text(&body)
        );
        assert_eq!(envelope(&body).error.code, ErrorCode::AccessDenied);
    }
    // 不存在的 Root 与越权同一个 403；Root 身份写法不对是 400。
    let missing = RootId::from_bytes([0xee; ID_LEN]);
    for uri in [
        snapshots_uri(missing),
        changes_uri(missing, "?after=0"),
        events_uri(missing),
    ] {
        let method = if uri.ends_with("/snapshots") {
            "POST"
        } else {
            "GET"
        };
        let (status, _) = app.call(send(method, &uri, Some(&owner))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    let (status, _) = app
        .call(send(
            "GET",
            "/v1/roots/not-hex/changes?after=0",
            Some(&owner),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = app
        .call(send(
            "GET",
            &snapshot_entries_uri(ROOT, "not-hex", ""),
            Some(&owner),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // 对照：owner 自己都能读通，说明上面的拒绝来自授权而不是端点坏了。
    let (status, _) = app.get(&changes_uri(ROOT, "?after=0"), &owner).await;
    assert_eq!(status, StatusCode::OK);
    let (status, reader, _) = SseReader::open(&app, &owner).await;
    assert_eq!(status, StatusCode::OK);
    drop(reader);
}
