//! S052 集成装置的共用部分：隔离目录、owner 初始化、真实监听的服务端实例与 HTTP 客户端。
//!
//! 隔离约定（design §2.4、根 AGENTS.md）：
//!
//! - 每个用例独占一个 `<target>/s052-lab/<label>-<pid>-<seq>/` 工作根，其下 `state/`（服务端内部状态
//!   目录：SQLite、对象区、上传暂存）与 `work/`（Root 工作目录）互不重叠，都在 btrfs 上（`cargo
//!   target` 与仓库同一个 btrfs 卷；满盘用例改用 `VAULTSYNCD_S052_*_VOLUME` 指定的小卷）。
//! - 端口由内核分配（`listen = 127.0.0.1:0`），不同用例不会互相冲突；没有固定账号、没有真实数据。
//! - 停止是真实的优雅停止（`Lifecycle::begin_shutdown` → HTTP 收尾 → 观察循环停下 → 数据库关闭），
//!   因此“重启”读到的是真正的磁盘事实，而不是内存残留。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::Value;
use tokio::task::JoinHandle;

use vaultsyncd_domain::ids::{IdSource, UserId};
use vaultsyncd_server::bootstrap;
use vaultsyncd_server::config::Config;
use vaultsyncd_server_core::auth::password::PasswordHasher;
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::users as store_users;

/// 测试账号。
pub const LOGIN: &str = "s052-owner";
/// 测试口令（只是测试文档里的常量，不是任何真实部署的凭据）。
pub const PASSWORD: &str = "S052-Correct-Horse-Battery-9!";

/// OS 随机源（账号身份生成用；与生产 `RandomIds` 同一来源）。
struct OsIds;

impl IdSource for OsIds {
    fn next_id_bytes(&mut self) -> [u8; 16] {
        use rand_core::{OsRng, RngCore};
        let mut bytes = [0_u8; 16];
        OsRng.fill_bytes(&mut bytes);
        bytes
    }
}

/// 一个隔离的实验目录。
pub struct Lab {
    root: PathBuf,
    state: PathBuf,
}

impl Lab {
    /// 在 btrfs 上建独立目录：`state/` 与 `work/` 分开。
    ///
    /// `work_base`/`state_base` 允许满盘用例把其中一侧放到小型测试卷上，另一侧留在普通 btrfs；
    /// 两侧永远不在同一个目录里（内部状态与工作目录不重叠）。
    pub fn new(label: &str, work_base: Option<&Path>, state_base: Option<&Path>) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let work_root = work_base
            .map_or_else(default_base, Path::to_path_buf)
            .join(format!("{label}-{}-{sequence}-work", std::process::id()));
        let state_root = state_base
            .map_or_else(default_base, Path::to_path_buf)
            .join(format!("{label}-{}-{sequence}-state", std::process::id()));
        let _ = std::fs::remove_dir_all(&work_root);
        let _ = std::fs::remove_dir_all(&state_root);
        std::fs::create_dir_all(state_root.join("state")).expect("建状态目录");
        std::fs::create_dir_all(work_root.join("work")).expect("建工作目录");
        Lab {
            root: work_root,
            state: state_root,
        }
    }

    /// 实验根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 服务端内部状态目录。
    pub fn state(&self) -> PathBuf {
        self.state.join("state")
    }

    /// 实验根目录（清理用；状态目录可能在别处）。
    pub fn roots(&self) -> [&Path; 2] {
        [self.root.as_path(), self.state.as_path()]
    }

    /// Root 工作目录（测试直接往里写外部文件）。
    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// 允许注册的父目录：工作目录本身。
    pub fn allowed_base(&self) -> PathBuf {
        self.root.clone()
    }

    fn config_path(&self) -> PathBuf {
        self.state.join("server.toml")
    }

    fn config_text(&self) -> String {
        format!(
            "[server]\nlisten = \"127.0.0.1:0\"\n\n[state]\ndir = \"{}\"\n\n[roots]\nallowed_bases = [\"{}\"]\n\n[log]\nlevel = \"warn\"\nformat = \"text\"\n",
            self.state().display(),
            self.allowed_base().display()
        )
    }

    /// 写配置文件并解析成进程配置。
    pub fn config(&self) -> Config {
        let text = self.config_text();
        std::fs::write(self.config_path(), &text).expect("写配置");
        Config::parse(&text, self.config_path()).expect("配置合法")
    }

    /// 以**离线**方式初始化首位 owner：与 `vaultsyncd-admin init` 同一套散列与约束。
    pub async fn init_owner(&self) {
        let database = Database::open(self.state().join("server.sqlite"))
            .await
            .expect("打开测试数据库");
        let hasher = PasswordHasher::production();
        let hash = hasher.hash(PASSWORD).expect("计算密码散列");
        let mut tx = database.begin_write().await.expect("开始写事务");
        store_users::insert_user(
            &mut tx,
            &store_users::NewUser {
                id: UserId::generate(&mut OsIds),
                login: LOGIN.to_owned(),
                password_hash: hash,
            },
        )
        .await
        .expect("写入 owner");
        tx.commit().await.expect("提交 owner");
        database.close().await;
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

fn default_base() -> PathBuf {
    // 仓库 `target/` 与源码同一个 btrfs 卷（满盘用例用环境变量换成小卷）。
    // 用 `canonicalize` 去掉 `..` 分量：允许基目录的规则拒绝含 `.`/`..` 的路径。
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/s052-lab");
    std::fs::create_dir_all(&base).expect("建实验根目录");
    base.canonicalize().expect("规范化实验根目录")
}

/// 一个正在运行的服务端实例。
pub struct Running {
    base: String,
    client: reqwest::Client,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Running {
    /// 启动实例并等到健康探测通过。
    ///
    /// # Errors
    ///
    /// 启动失败或健康探测超时。
    pub async fn start(lab: &Lab) -> Result<Self, String> {
        let service = bootstrap::prepare(lab.config())
            .await
            .map_err(|error| format!("启动失败：{error}"))?;
        let addr = service.local_addr().map_err(|error| error.to_string())?;
        let (stop, wait) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let shutdown = async move {
                let _ = wait.await;
            };
            if let Err(error) = service.serve(shutdown).await {
                eprintln!("服务端运行期错误：{error}");
            }
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| error.to_string())?;
        let running = Running {
            base: format!("http://{addr}"),
            client,
            stop: Some(stop),
            task: Some(task),
        };
        running.wait_healthy().await?;
        Ok(running)
    }

    async fn wait_healthy(&self) -> Result<(), String> {
        for _ in 0..200 {
            match self.client.get(self.uri("/v1/health")).send().await {
                Ok(response) if response.status() == StatusCode::OK => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        Err("健康探测超时".to_owned())
    }

    /// 停止实例（真实的优雅停止路径，包含观察循环与数据库关闭）。
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(30), task).await;
        }
    }

    /// 完整 URI。
    pub fn uri(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// 发一个 JSON 请求并返回状态码与解析后的响应体（空体为 `Value::Null`）。
    pub async fn json(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value), String> {
        let mut request = self.client.request(
            method.parse().map_err(|_| "方法".to_owned())?,
            self.uri(path),
        );
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|error| error.to_string())?;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.clone()))
        };
        Ok((status, value))
    }

    /// 发一个二进制块请求（`PUT .../parts/{index}`）。
    pub async fn put_part(
        &self,
        path: &str,
        token: &str,
        bytes: Vec<u8>,
    ) -> Result<(StatusCode, Value), String> {
        let hash = hex(&vaultsyncd_platform_linux_server::files::hash::digest(
            &bytes,
        ));
        let response = self
            .client
            .put(self.uri(path))
            .bearer_auth(token)
            .header("x-part-sha256", hash)
            .body(bytes)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|error| error.to_string())?;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Ok((status, value))
    }

    /// 下载固定版本内容（返回状态码、字节与响应头里的哈希）。
    pub async fn download(
        &self,
        path: &str,
        token: &str,
    ) -> Result<(StatusCode, Vec<u8>, Option<String>), String> {
        let response = self
            .client
            .get(self.uri(path))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let hash = response
            .headers()
            .get("x-content-hash")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = response
            .bytes()
            .await
            .map_err(|error| error.to_string())?
            .to_vec();
        Ok((status, bytes, hash))
    }
}

/// 登录并返回访问凭据。
pub async fn login(running: &Running) -> Result<String, String> {
    let body = serde_json::json!({
        "device_label": "S052 集成验收",
        "login": LOGIN,
        "password": PASSWORD,
    });
    let (status, value) = running
        .json("POST", "/v1/auth/login", None, Some(&body))
        .await?;
    if status != StatusCode::OK {
        return Err(format!("登录失败（{status}）：{value}"));
    }
    value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "登录响应没有 access_token".to_owned())
}

/// 注册一个新的 Root 并返回 Root 身份。
pub async fn register_root(running: &Running, token: &str, path: &Path) -> Result<String, String> {
    let body = serde_json::json!({ "path": path.display().to_string() });
    let (status, value) = running
        .json("POST", "/v1/admin/roots", Some(token), Some(&body))
        .await?;
    if status != StatusCode::CREATED && status != StatusCode::OK {
        return Err(format!("注册 Root 失败（{status}）：{value}"));
    }
    value
        .get("root_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "注册响应没有 root_id".to_owned())
}

/// 上传一份内容并 finalize；返回（upload_id、hash、size）。
pub async fn upload(
    running: &Running,
    token: &str,
    root: &str,
    content: &[u8],
    chunk_size: usize,
) -> Result<(String, String, u64), String> {
    let hash = hex(&vaultsyncd_platform_linux_server::files::hash::digest(
        content,
    ));
    let body = serde_json::json!({
        "chunk_size": chunk_size.to_string(),
        "hash": hash,
        "total_size": content.len().to_string(),
    });
    let (status, value) = running
        .json(
            "POST",
            &format!("/v1/roots/{root}/uploads"),
            Some(token),
            Some(&body),
        )
        .await?;
    if status != StatusCode::CREATED && status != StatusCode::OK {
        return Err(format!("创建上传会话失败（{status}）：{value}"));
    }
    let upload_id = value
        .get("upload_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "上传响应没有 upload_id".to_owned())?
        .to_owned();
    let chunk_size: usize = value
        .get("chunk_size")
        .and_then(Value::as_str)
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| "上传响应没有 chunk_size".to_owned())?;
    for (index, chunk) in content.chunks(chunk_size.max(1)).enumerate() {
        let path = format!("/v1/roots/{root}/uploads/{upload_id}/parts/{index}");
        let (status, value) = running.put_part(&path, token, chunk.to_vec()).await?;
        if status != StatusCode::OK {
            return Err(format!("上传块 {index} 失败（{status}）：{value}"));
        }
    }
    let (status, value) = running
        .json(
            "POST",
            &format!("/v1/roots/{root}/uploads/{upload_id}/finalize"),
            Some(token),
            Some(&serde_json::json!({})),
        )
        .await?;
    if status != StatusCode::OK {
        return Err(format!("finalize 失败（{status}）：{value}"));
    }
    Ok((upload_id, hash, content.len() as u64))
}

/// 提交内容 put 操作（返回状态码与响应体）。
pub async fn put_operation(
    running: &Running,
    token: &str,
    root: &str,
    op_id: &str,
    device_id: &str,
    epoch: &str,
    entry: &str,
    revision: &str,
    upload: &str,
) -> Result<(StatusCode, Value), String> {
    let body = serde_json::json!({
        "op_id": op_id,
        "device_id": device_id,
        "server_epoch": epoch,
        "kind": "put_content",
        "entry_id": entry,
        "base_revision": revision,
        "upload_id": upload,
        "local_generation": "7",
        "conflict_policy": "preserve_both",
    });
    running
        .json(
            "POST",
            &format!("/v1/roots/{root}/operations"),
            Some(token),
            Some(&body),
        )
        .await
}

/// 新快照的身份。
pub async fn create_snapshot(running: &Running, token: &str, root: &str) -> Result<Value, String> {
    let (status, value) = running
        .json(
            "POST",
            &format!("/v1/roots/{root}/snapshots"),
            Some(token),
            Some(&serde_json::json!({})),
        )
        .await?;
    if status != StatusCode::CREATED && status != StatusCode::OK {
        return Err(format!("创建快照失败（{status}）：{value}"));
    }
    Ok(value)
}

/// 快照清单（一次取完；测试 Root 很小）。
pub async fn snapshot_entries(
    running: &Running,
    token: &str,
    root: &str,
    snapshot: &str,
) -> Result<Vec<Value>, String> {
    let mut entries = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut path = format!("/v1/roots/{root}/snapshots/{snapshot}/entries?limit=100");
        if let Some(cursor) = &cursor {
            path.push_str(&format!("&cursor={cursor}"));
        }
        let (status, value) = running.json("GET", &path, Some(token), None).await?;
        if status != StatusCode::OK {
            return Err(format!("读取快照失败（{status}）：{value}"));
        }
        if let Some(page) = value.get("entries").and_then(Value::as_array) {
            entries.extend(page.iter().cloned());
        }
        cursor = value
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Ok(entries);
        }
    }
}

/// 拉取全部 changes 批次（从 0 开始翻页）。
pub async fn all_changes(running: &Running, token: &str, root: &str) -> Result<Vec<Value>, String> {
    let mut batches = Vec::new();
    let mut after = "0".to_owned();
    loop {
        let path = format!("/v1/roots/{root}/changes?after={after}&limit=100");
        let (status, value) = running.json("GET", &path, Some(token), None).await?;
        if status != StatusCode::OK {
            return Err(format!("读取 changes 失败（{status}）：{value}"));
        }
        if let Some(page) = value.get("batches").and_then(Value::as_array) {
            batches.extend(page.iter().cloned());
        }
        let has_more = value
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_more {
            return Ok(batches);
        }
        after = value
            .get("next_after")
            .and_then(Value::as_str)
            .ok_or_else(|| "changes 响应没有 next_after".to_owned())?
            .to_owned();
    }
}

/// 十六进制摘要文本。
pub fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 生成一个规范 16 字节身份的十六进制文本（测试里的 OpId/DeviceId）。
pub fn identity(seed: u8) -> String {
    [seed; 16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 等待某个条件成立（观察循环是周期任务，需要轮询）。
pub async fn wait_for<F, Fut>(label: &str, mut probe: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    for _ in 0..120 {
        if probe().await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!("等待 {label} 超时"))
}
