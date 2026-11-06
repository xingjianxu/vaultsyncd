//! Windows 客体上的真实跨进程编排：三个隔离客户端身份 + 真实 Linux 服务端。
//!
//! 拓扑（每次报告都重复声明，避免把多进程误读成多台实机）：
//!
//! - **一台** Windows 11 客体（`docs/operations/windows-lab.md`；第二台按计划属 S107）；
//! - 三个客户端身份/环境：独立 DeviceId、状态目录、`client.sqlite`、同步根、缓存与
//!   `vaultsyncd-agent.exe` 进程；
//! - 产品不变量“每用户会话单实例”使三个 agent **顺序**运行（同一交互会话），因此报告
//!   `concurrent_agent_processes=1`，不宣称并发；这正是本卡验收允许并必须写明的限制。

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::json;
use vaultsyncd_http_client::{
    ApiClient, ApiRequest, AuthMode, ClientConfig, Credentials, ReqwestTransport, TlsTrust, sync,
};
use vaultsyncd_local_ipc::{
    generated::{
        BindingStatus, Command as IpcCommand, CommandName, CommandParameters, GetStatusResult,
        Request, Response, ResponseKind,
    },
    version,
};
use vaultsyncd_platform_windows::{files::RootHandle, ipc::Endpoint};
use vaultsyncd_protocol::generated::{
    EntryKind, OperationState, RecoveryItem, RecoveryPage, SessionIssued, SnapshotEntry,
};
use vaultsyncd_transfer::hash::{digest_of, to_hex};

use crate::{
    Options,
    proxy::{OperationEvent, Proxy},
    scenario::{Block, ClientSpec, Mode, Scenario},
};

/// 客户端配置里稳定的“服务端身份”（同一服务端，三个客户端共享；凭据库因此共享，符合
/// 同一账号多设备的真实形态）。
const SERVER_ID: &str = "5e74000000000000000000000000a001";
/// 单次等待上限（agent 启动/收敛）。
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// 场景动作后的收敛等待上限。
const ACTION_TIMEOUT: Duration = Duration::from_secs(180);

type Api = ApiClient<ReqwestTransport>;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 本机 fixture（宿主 helper 生成，经 scp 传入）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    /// 宿主侧服务端地址（脚本会改写成客体回环隧道地址）。
    url: String,
    /// Root 身份。
    root: String,
    /// owner 会话（编排器自己的 HTTP 客户端用它读取快照/历史/回收站）。
    issued: SessionIssued,
    /// 测试账号与口令（仅用于让真实 agent 经 `--login-stdin` 登录）。
    login: String,
    /// 测试口令（合成测试常量，不是部署凭据）。
    password: String,
    /// 服务端二进制版本（宿主 helper 的 `CARGO_PKG_VERSION`）。
    server_version: String,
    /// 服务端 Root 初始内容（路径、摘要、大小）。
    seeded: Vec<Seeded>,
    /// 服务端工作目录绝对路径（宿主侧收尾核对用）。
    work_dir: String,
    /// 测试 TLS 入口的私有 CA（编排器启动代理后填入）。
    #[serde(default)]
    ca_file: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Seeded {
    path: String,
    hash: String,
    size: String,
}

/// 一个客户端环境。
struct Client {
    spec: ClientSpec,
    device_id: String,
    binding_id: String,
    root: PathBuf,
    cache: PathBuf,
    state: PathBuf,
    config: PathBuf,
    log: PathBuf,
    child: Option<ChildGuard>,
    agent_version: Option<String>,
}

/// 进程句柄守卫：任何提前返回都杀掉本程序启动的 agent，不留孤儿。
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// 编排状态。
struct Run {
    api: Api,
    root: String,
    proxy: Proxy,
    fixture: Fixture,
    agent: PathBuf,
    run_dir: PathBuf,
    clients: Vec<Client>,
}

#[derive(Debug, Serialize)]
struct Report {
    passed: bool,
    scenario: String,
    commit: String,
    dirty: bool,
    guest: String,
    server_version: String,
    agent_version: String,
    topology: Topology,
    clients: Vec<ClientReport>,
    blocks: Vec<BlockReport>,
    coverage: Coverage,
    convergence: Convergence,
    proxy: ProxyReport,
    final_entries: Vec<EntryReport>,
    final_conflict_copies: Vec<String>,
    work_dir: String,
}

#[derive(Debug, Serialize)]
struct Topology {
    physical_windows_guests: u32,
    guest: String,
    client_identities: u32,
    isolation: Vec<String>,
    concurrent_agent_processes: u32,
    sequential_reason: String,
    second_guest_task: String,
}

#[derive(Debug, Serialize)]
struct ClientReport {
    id: String,
    label: String,
    mode: String,
    device_id: String,
    binding_id: String,
    root: String,
    state_dir: String,
    agent_log: String,
    agent_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct BlockReport {
    id: String,
    kind: String,
    requirements: Vec<String>,
    evidence: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Coverage {
    inputs: Vec<Covered>,
    missing: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Covered {
    label: String,
    hash: String,
    found_in: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Convergence {
    passes: u32,
    server_paths: usize,
    client_paths: BTreeMap<String, usize>,
    matched: bool,
    full_materialized_bytes_verified: usize,
    on_demand_placeholders: usize,
}

#[derive(Debug, Serialize)]
struct ProxyReport {
    url: String,
    ca_file: String,
    requests: u64,
    dropped: u64,
    events: Vec<OperationEvent>,
}

#[derive(Debug, Serialize)]
struct EntryReport {
    path: String,
    entry_id: String,
    hash: String,
    version_id: String,
    size: String,
}

/// 执行整轮场景。返回进程退出码（0 成功，1 失败，2 数据/用法错误）。
pub fn execute(options: &Options, scenario: &Scenario) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("check=fail runtime {error}");
            return 1;
        }
    };
    match runtime.block_on(round(options, scenario)) {
        Ok(report) => {
            write_report(options, &report);
            println!(
                "S074_REPORT {}",
                serde_json::to_string(&report).unwrap_or_default()
            );
            println!("check=pass s074_three_identity_run");
            0
        }
        Err(error) => {
            eprintln!("check=fail {}", one_line(&error.to_string()));
            let failure =
                json!({"passed": false, "error": error.to_string(), "topology": topology(0)});
            let _ = fs::create_dir_all(options.report.parent().unwrap_or(Path::new(".")));
            let _ = fs::write(
                &options.report,
                serde_json::to_vec_pretty(&failure).unwrap_or_default(),
            );
            1
        }
    }
}

fn write_report(options: &Options, report: &Report) {
    if let Some(parent) = options.report.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match serde_json::to_vec_pretty(report) {
        Ok(bytes) => {
            if let Err(error) = fs::write(&options.report, bytes) {
                eprintln!("check=warn report_write {error}");
            }
        }
        Err(error) => eprintln!("check=warn report_encode {error}"),
    }
}

async fn round(options: &Options, scenario: &Scenario) -> Result<Report> {
    unsafe {
        AllocConsole();
    }
    println!("check=info guest_os {}", guest_version());
    println!("check=info scenario_file={}", options.scenario.display());
    let fixture_bytes = fs::read(&options.fixture)?;
    let mut fixture: Fixture = serde_json::from_slice(&fixture_bytes)?;
    let run_dir = options
        .driver
        .join(format!("run-e2e-{}", std::process::id()));
    if run_dir.exists() {
        fs::remove_dir_all(&run_dir)?;
    }
    fs::create_dir_all(run_dir.join("clients"))?;
    fs::create_dir_all(run_dir.join("logs"))?;
    let proxy = Proxy::start(&fixture.url, &run_dir.join("tls")).await?;
    println!(
        "check=info tls_proxy {} ca={}",
        proxy.url(),
        proxy.ca_file.display()
    );
    fixture.url = proxy.url();
    fixture.ca_file = Some(proxy.ca_file.clone());
    let trust = TlsTrust::system_roots_with_pem_file(&proxy.ca_file)?;
    let api = ApiClient::new(ClientConfig::new(proxy.url()).with_trust(trust))?;
    api.auth()
        .install(Credentials::from_issued(&fixture.issued)?);
    // 经真实 TLS 入口确认服务端可达，并核对宿主种子内容确实已经是服务端命名空间事实。
    let seeded = entries(&api, &fixture.root).await?;
    let seeded_paths = path_map(&seeded)?;
    for seed in &fixture.seeded {
        let entry = seeded_paths
            .get(&seed.path)
            .ok_or_else(|| format!("服务端缺少种子条目：{}", seed.path))?;
        if entry.hash.as_deref() != Some(seed.hash.as_str())
            || entry.size.as_deref() != Some(seed.size.as_str())
        {
            return Err(format!(
                "服务端种子 {} 摘要/大小不符：{:?}/{:?} != {}/{}",
                seed.path, entry.hash, entry.size, seed.hash, seed.size
            )
            .into());
        }
    }
    println!(
        "check=pass https_reachable_and_non_empty_server_root seeded={}",
        fixture.seeded.len()
    );

    let mut clients = Vec::new();
    for spec in &scenario.clients {
        clients.push(prepare_client(&run_dir, spec)?);
    }
    let mut run = Run {
        api,
        root: fixture.root.clone(),
        proxy,
        fixture: fixture.clone(),
        agent: options.agent.clone(),
        run_dir: run_dir.clone(),
        clients,
    };

    let mut blocks = Vec::new();
    for block in &scenario.blocks {
        if !options.blocks.is_empty() && !options.blocks.iter().any(|id| id == block.id()) {
            println!("check=skip block={} (filtered)", block.id());
            continue;
        }
        println!("check=info block_start id={}", block.id());
        let evidence = match block {
            Block::Import { local, .. } => run.import(local).await?,
            Block::OfflineEdit { path, contents, .. } => run.offline_edit(path, contents).await?,
            Block::Rename {
                renamer,
                placeholder_client,
                from,
                to,
                ..
            } => run.rename(renamer, placeholder_client, from, to).await?,
            Block::DeleteModify {
                path,
                deleter,
                modifier,
                modified_content,
                ..
            } => {
                run.delete_modify(path, deleter, modifier, modified_content)
                    .await?
            }
            Block::LostResponse { put, delete, .. } => run.lost_response(put, delete).await?,
        };
        println!("check=pass block_done id={}", block.id());
        blocks.push(BlockReport {
            id: block.id().to_owned(),
            kind: block_kind(block).to_owned(),
            requirements: block.requirements().to_vec(),
            evidence,
        });
    }

    let passes = run.converge().await?;
    let convergence = run.verify_namespaces(passes).await?;
    let coverage = run.coverage(scenario).await?;
    let final_entries = run.final_entries().await?;
    let proxy_report = ProxyReport {
        url: run.proxy.url(),
        ca_file: run.proxy.ca_file.display().to_string(),
        requests: run.proxy.requests(),
        dropped: run.proxy.dropped(),
        events: run.proxy.events(),
    };
    if coverage.missing.is_empty() {
        println!(
            "check=pass every_input_content_found inputs={}",
            coverage.inputs.len()
        );
    } else {
        return Err(format!("输入内容未找到：{:?}", coverage.missing).into());
    }
    let report = Report {
        passed: true,
        scenario: scenario.id.clone(),
        commit: std::env::var("VAULTSYNCD_E2E_COMMIT").unwrap_or_else(|_| "unknown".into()),
        dirty: std::env::var("VAULTSYNCD_E2E_DIRTY")
            .map(|value| value != "0")
            .unwrap_or(false),
        guest: guest_version(),
        server_version: fixture.server_version.clone(),
        agent_version: run
            .clients
            .iter()
            .filter_map(|client| client.agent_version.clone())
            .next()
            .unwrap_or_default(),
        topology: topology(1),
        clients: run
            .clients
            .iter()
            .map(|client| ClientReport {
                id: client.spec.id.clone(),
                label: client.spec.label.clone(),
                mode: client.spec.mode.as_str().to_owned(),
                device_id: client.device_id.clone(),
                binding_id: client.binding_id.clone(),
                root: client.root.display().to_string(),
                state_dir: client.state.display().to_string(),
                agent_log: client.log.display().to_string(),
                agent_version: client.agent_version.clone(),
            })
            .collect(),
        blocks,
        coverage,
        convergence,
        proxy: proxy_report,
        final_entries,
        final_conflict_copies: final_conflicts(&run.final_entries().await?),
        work_dir: fixture.work_dir.clone(),
    };
    println!(
        "check=pass topology_one_guest_three_isolated_identities sequential_agent_processes=1"
    );
    for warning in run.cleanup(&fixture.root) {
        println!("check=warn cleanup {warning}");
    }
    Ok(report)
}

fn topology(guest_count: u32) -> Topology {
    Topology {
        physical_windows_guests: guest_count,
        guest: guest_version(),
        client_identities: 3,
        isolation: vec![
            "device_id".into(),
            "state_dir/client.sqlite".into(),
            "sync_root".into(),
            "cache".into(),
            "agent_process".into(),
        ],
        concurrent_agent_processes: 1,
        sequential_reason:
            "产品不变量：每个 Windows 用户会话/状态库只允许一个 agent；第二台实机按计划属 S107"
                .into(),
        second_guest_task: "S107".into(),
    }
}

fn prepare_client(run_dir: &Path, spec: &ClientSpec) -> Result<Client> {
    let base = run_dir.join("clients").join(&spec.id);
    let root = base.join("root");
    let cache = base.join("cache");
    let state = base.join("state");
    for path in [&root, &cache, &state] {
        fs::create_dir_all(path)?;
    }
    let config = base.join("agent.json");
    let log = run_dir.join("logs").join(format!("{}-agent.log", spec.id));
    Ok(Client {
        spec: spec.clone(),
        device_id: id32(),
        binding_id: id32(),
        root,
        cache,
        state,
        config,
        log,
        child: None,
        agent_version: None,
    })
}

impl Run {
    async fn import(
        &mut self,
        local: &BTreeMap<String, Vec<crate::scenario::FileSpec>>,
    ) -> Result<Vec<String>> {
        for (client_id, files) in local {
            let index = self.index(client_id)?;
            for file in files {
                write_file(&self.clients[index].root, &file.path, &file.content)?;
            }
        }
        self.converge().await?;
        let rows = self.entries().await?;
        let paths = path_map(&rows)?;
        // T01：服务端原有内容不得因首次合并被推导删除。
        for seed in &self.fixture.seeded {
            if !paths.contains_key(&seed.path) {
                return Err(format!("首次导入推导删除了服务端条目：{}", seed.path).into());
            }
        }
        let mut evidence = Vec::new();
        let hashes = hash_index(&rows);
        let shared = local
            .values()
            .flat_map(|files| files.iter())
            .find(|file| file.path == "shared-import.txt")
            .map(|file| hash_of(file.content.as_bytes()))
            .ok_or("场景缺少 shared-import.txt")?;
        let merged = hashes.get(&shared).map_or(0, Vec::len);
        if merged != 1 {
            return Err(format!("同内容归并失败：shared-import 摘要出现 {merged} 次").into());
        }
        evidence.push(format!("shared_content_hash={shared} entries={merged}"));
        // 每端的独有内容与不同内容都必须存在；不同内容的原路径各保留一份副本。
        for (client_id, files) in local {
            for file in files {
                if file.path == "shared-import.txt" {
                    continue;
                }
                let hash = hash_of(file.content.as_bytes());
                let count = hashes.get(&hash).map_or(0, Vec::len);
                if count == 0 {
                    return Err(format!("{client_id} 导入内容丢失：{} {hash}", file.path).into());
                }
                evidence.push(format!(
                    "{client_id}:{} hash={hash} entries={count}",
                    file.path
                ));
            }
        }
        let server_divergent = self
            .fixture
            .seeded
            .iter()
            .find(|seed| seed.path == "divergent.txt")
            .map(|seed| seed.hash.clone())
            .ok_or("服务端缺少 divergent.txt 种子")?;
        let divergent = hashes.get(&server_divergent).map_or(0, Vec::len);
        if divergent != 1 {
            return Err(format!("服务端 divergent 内容应恰好 1 条，实际 {divergent}").into());
        }
        let distinct_divergent: BTreeSet<String> = local
            .values()
            .flat_map(|files| files.iter())
            .filter(|file| file.path == "divergent.txt")
            .map(|file| hash_of(file.content.as_bytes()))
            .collect();
        if distinct_divergent.len() != local.len() {
            return Err("三端 divergent 内容必须两两不同".into());
        }
        for hash in &distinct_divergent {
            if hashes.get(hash).map_or(0, Vec::len) != 1 {
                return Err(format!("异内容副本缺失或重复：{hash}").into());
            }
            evidence.push(format!("divergent_content={hash} preserved"));
        }
        evidence.push(format!(
            "server_paths_retained={}",
            self.fixture.seeded.len()
        ));
        Ok(evidence)
    }

    async fn offline_edit(
        &mut self,
        path: &str,
        contents: &BTreeMap<String, String>,
    ) -> Result<Vec<String>> {
        // 离线：三个客户端都没有 agent 进程时直接改本地文件。
        for client in &self.clients {
            if client.child.is_some() {
                return Err(format!("{} 离线编辑前仍在运行", client.spec.id).into());
            }
        }
        for (client_id, content) in contents {
            let index = self.index(client_id)?;
            write_file(&self.clients[index].root, path, content)?;
        }
        self.converge().await?;
        let rows = self.entries().await?;
        let hashes = hash_index(&rows);
        let mut evidence = Vec::new();
        for (client_id, content) in contents {
            let hash = hash_of(content.as_bytes());
            let count = hashes.get(&hash).map_or(0, Vec::len);
            if count == 0 {
                return Err(format!("{client_id} 的离线修改内容丢失：{hash}").into());
            }
            evidence.push(format!("{client_id}_offline_hash={hash} entries={count}"));
        }
        let conflicts = rows.iter().filter(|entry| is_conflict(entry)).count();
        if conflicts + 1 < contents.len() {
            return Err(format!("离线并发修改缺少冲突副本：conflict_entries={conflicts}").into());
        }
        evidence.push(format!("conflict_entries={conflicts}"));
        // 再跑一整轮：重试/重复同步不能让副本数量增长，也不能让内容丢失。
        let before = rows.len();
        let before_counts: BTreeMap<String, usize> =
            hashes.iter().map(|(h, v)| (h.clone(), v.len())).collect();
        self.converge().await?;
        let again = self.entries().await?;
        if again.len() != before {
            return Err(format!("重复同步后条目数变化：{before} -> {}", again.len()).into());
        }
        let again_hashes = hash_index(&again);
        for (hash, count) in &before_counts {
            if again_hashes.get(hash).map_or(0, Vec::len) != *count {
                return Err(format!("重复同步后摘要 {hash} 数量变化").into());
            }
        }
        evidence.push(format!("stable_entry_count={}", again.len()));
        Ok(evidence)
    }

    async fn rename(
        &mut self,
        renamer: &str,
        placeholder_client: &str,
        from: &str,
        to: &str,
    ) -> Result<Vec<String>> {
        // 文件来自服务端初始内容：完整客户端已物化，按需端在首次导入后只有占位。
        let mover = self.index(renamer)?;
        if !self.clients[mover].root.join(from).exists() {
            return Err(format!("{} 本地缺少重命名源：{from}", self.clients[mover].spec.id).into());
        }
        let holder = self.index(placeholder_client)?;
        self.start_client(holder).await?;
        self.wait_idle(holder).await?;
        self.stop_client(holder).await?;
        let before = snapshot_entry(&self.entries().await?, from)?;
        let before_identity = self.clients[holder].root.inspect_or_err(from)?.identity;
        let on_disk = placeholder_on_disk(&self.clients[holder].root, from)?;
        if self.clients[holder].spec.mode == Mode::OnDemand && on_disk != Some(0) {
            return Err(format!("按需端在重命名前已下载内容：on_disk={on_disk:?}").into());
        }
        let entry_id = before.entry_id.clone();
        let version_id = before.version_id.clone();

        self.start_client(mover).await?;
        fs::rename(
            self.clients[mover].root.join(from),
            self.clients[mover].root.join(to),
        )?;
        self.wait_server("重命名提交", |rows| {
            let paths = path_map(rows).ok();
            paths.is_some_and(|paths| {
                paths
                    .get(to)
                    .is_some_and(|entry| entry.entry_id == entry_id)
                    && !paths.contains_key(from)
            })
        })
        .await?;
        self.stop_client(mover).await?;
        self.converge().await?;

        let after = snapshot_entry(&self.entries().await?, to)?;
        if after.entry_id != entry_id {
            return Err(format!("重命名改变了 EntryId：{entry_id} -> {}", after.entry_id).into());
        }
        if after.version_id != version_id {
            return Err("重命名改变了固定版本".into());
        }
        let after_identity = self.clients[holder].root.inspect_or_err(to)?.identity;
        if before_identity != after_identity {
            return Err("按需端重命名后本地文件身份变化".into());
        }
        let after_on_disk = placeholder_on_disk(&self.clients[holder].root, to)?;
        if self.clients[holder].spec.mode == Mode::OnDemand && after_on_disk != Some(0) {
            return Err(format!("按需端为重命名下载了内容：on_disk={after_on_disk:?}").into());
        }
        Ok(vec![
            format!("entry_id_stable={entry_id}"),
            format!("version_id_stable={version_id:?}"),
            format!(
                "placeholder_client={placeholder_client} on_disk_before={on_disk:?} on_disk_after={after_on_disk:?}"
            ),
            format!("local_identity_stable={before_identity:?}"),
        ])
    }

    async fn delete_modify(
        &mut self,
        path: &str,
        deleter: &str,
        modifier: &str,
        modified_content: &str,
    ) -> Result<Vec<String>> {
        let before = snapshot_entry(&self.entries().await?, path)?;
        let original_entry = before.entry_id;
        let original = before
            .hash
            .ok_or_else(|| format!("删除目标没有固定版本：{path}"))?;
        let modifier_index = self.index(modifier)?;
        write_file(&self.clients[modifier_index].root, path, modified_content)?;
        let modified = hash_of(modified_content.as_bytes());

        let deleter_index = self.index(deleter)?;
        self.start_client(deleter_index).await?;
        fs::remove_file(self.clients[deleter_index].root.join(path))?;
        self.wait_server("删除提交", |rows| {
            path_map(rows).is_ok_and(|paths| !paths.contains_key(path))
        })
        .await?;
        self.stop_client(deleter_index).await?;

        self.start_client(modifier_index).await?;
        self.wait_idle(modifier_index).await?;
        self.stop_client(modifier_index).await?;
        self.converge().await?;

        let rows = self.entries().await?;
        let paths = path_map(&rows)?;
        if let Some(current) = paths.get(path) {
            // 用户的新修改可以重新占用同名路径，但必须是新条目；旧条目不能被复活。
            if current.entry_id == original_entry {
                return Err("并发修改把已删除的旧条目复活了".into());
            }
        }
        let hashes = hash_index(&rows);
        let modified_entries = hashes.get(&modified).map_or(0, Vec::len);
        if modified_entries == 0 {
            return Err("并发删除静默清除了新修改内容".into());
        }
        if hashes.get(&original).map_or(0, Vec::len) != 0 {
            return Err("已删除内容不应出现在当前命名空间".into());
        }
        let trash = self.trash().await?;
        if !trash
            .iter()
            .any(|item| item.hash.as_deref() == Some(original.as_str()))
        {
            return Err("回收站没有保留被删除内容".into());
        }
        // 修改方本地必须真的保留那份字节（可能在冲突副本名下）。
        if !self.clients[modifier_index].root.find_content(&modified)? {
            return Err("修改方本地找不到保留的并发修改内容".into());
        }
        Ok(vec![
            format!("deleted_entry_retired={original_entry}"),
            format!("deleted_content_in_trash={original}"),
            format!("modified_content_preserved={modified} entries={modified_entries}"),
            format!("path_may_be_reused_by_new_entry={path}"),
        ])
    }

    async fn lost_response(
        &mut self,
        put: &crate::scenario::LostPut,
        delete: &crate::scenario::LostDelete,
    ) -> Result<Vec<String>> {
        let mut evidence = Vec::new();
        // 先让 put 目标以初始内容存在并收敛。
        let client = self.index(&put.client)?;
        self.start_client(client).await?;
        write_file(&self.clients[client].root, &put.path, &put.initial_content)?;
        let initial = hash_of(put.initial_content.as_bytes());
        self.wait_server("lost-response put 初始内容", |rows| {
            hash_index(rows)
                .get(&initial)
                .is_some_and(|paths| paths.len() == 1)
        })
        .await?;
        self.stop_client(client).await?;
        self.converge().await?;
        let before_entry = snapshot_entry(&self.entries().await?, &put.path)?;
        let before = self.proxy.events().len();
        self.start_client(client).await?;
        self.proxy.arm_drop("put_content", 1);
        write_file(&self.clients[client].root, &put.path, &put.content)?;
        let dropped = self.wait_drop("put_content", before).await?;
        if dropped.state.as_deref() != Some("committed") || !dropped.dropped {
            return Err(format!("put 丢响应前的提交不是 committed：{dropped:?}").into());
        }
        // 产品的响应丢失恢复先用原 OpId 查询；查询结果必须与丢掉的 ACK 完全一致。
        let query = self.wait_query(&dropped.op_id).await?;
        if query.state.as_deref() != Some("committed")
            || query.revision != dropped.revision
            || query.version_id != dropped.version_id
            || query.root_seq != dropped.root_seq
        {
            return Err(format!(
                "put 查询结果与丢掉的 ACK 不同：dropped={dropped:?} query={query:?}"
            )
            .into());
        }
        self.wait_idle(client).await?;
        self.stop_client(client).await?;
        self.converge().await?;
        let final_hash = hash_of(put.content.as_bytes());
        let request = replay_request(&dropped)?;
        let replayed = sync::submit(&self.api, &self.root, &request).await?;
        if replayed.replayed != Some(true)
            || replayed.state != OperationState::Committed
            || replayed.revision != dropped.revision
            || replayed.version_id != dropped.version_id
            || replayed.root_seq != dropped.root_seq
        {
            return Err(format!("put 按原 OpId 重试不是相同结果：{replayed:?}").into());
        }
        let rows = self.entries().await?;
        let hashes = hash_index(&rows);
        if hashes.get(&final_hash).map_or(0, Vec::len) != 1 {
            return Err("put 丢响应重试产生了重复内容".into());
        }
        let after_entry = snapshot_entry(&rows, &put.path)?;
        if after_entry.entry_id != before_entry.entry_id {
            return Err("put 丢响应改变了 EntryId".into());
        }
        evidence.push(format!(
            "put op_id={} dropped_ack_revision={:?} query_same=true replay_same=true version_id={:?} root_seq={:?}",
            dropped.op_id, dropped.revision, dropped.version_id, dropped.root_seq
        ));

        // delete：让目标存在并收敛，然后丢弃删除提交的响应。
        self.start_client(client).await?;
        write_file(&self.clients[client].root, &delete.path, &delete.content)?;
        let deleted = hash_of(delete.content.as_bytes());
        self.wait_server("lost-response delete 初始内容", |rows| {
            hash_index(rows)
                .get(&deleted)
                .is_some_and(|paths| paths.len() == 1)
        })
        .await?;
        self.stop_client(client).await?;
        self.converge().await?;
        let before = self.proxy.events().len();
        self.start_client(client).await?;
        self.proxy.arm_drop("delete", 1);
        fs::remove_file(self.clients[client].root.join(&delete.path))?;
        let dropped = self.wait_drop("delete", before).await?;
        if dropped.state.as_deref() != Some("committed") || !dropped.dropped {
            return Err("delete 丢响应前的提交不是 committed".into());
        }
        let query = self.wait_query(&dropped.op_id).await?;
        if query.state.as_deref() != Some("committed")
            || query.revision != dropped.revision
            || query.root_seq != dropped.root_seq
        {
            return Err(format!(
                "delete 查询结果与丢掉的 ACK 不同：dropped={dropped:?} query={query:?}"
            )
            .into());
        }
        self.wait_idle(client).await?;
        self.stop_client(client).await?;
        self.converge().await?;
        let request = replay_request(&dropped)?;
        let replayed = sync::submit(&self.api, &self.root, &request).await?;
        if replayed.replayed != Some(true)
            || replayed.state != OperationState::Committed
            || replayed.revision != dropped.revision
            || replayed.root_seq != dropped.root_seq
        {
            return Err(format!("delete 按原 OpId 重试不是相同结果：{replayed:?}").into());
        }
        let rows = self.entries().await?;
        if path_map(&rows)?.contains_key(&delete.path) {
            return Err("delete 丢响应重试复活了已删除条目".into());
        }
        if hash_index(&rows).get(&deleted).map_or(0, Vec::len) != 0 {
            return Err("delete 丢响应重试在当前命名空间留下重复内容".into());
        }
        let trash = self.trash().await?;
        if !trash
            .iter()
            .any(|item| item.hash.as_deref() == Some(deleted.as_str()))
        {
            return Err("delete 丢响应后回收站没有保留内容".into());
        }
        evidence.push(format!(
            "delete op_id={} dropped_ack_revision={:?} query_same=true replay_same=true root_seq={:?}",
            dropped.op_id, dropped.revision, dropped.root_seq
        ));
        Ok(evidence)
    }

    async fn wait_drop(&self, kind: &str, before: usize) -> Result<OperationEvent> {
        let end = Instant::now() + ACTION_TIMEOUT;
        loop {
            let events = self.proxy.events();
            if let Some(event) = events
                .iter()
                .skip(before)
                .find(|event| event.dropped && event.kind == kind)
            {
                if !self.proxy.armed() {
                    return Ok(event.clone());
                }
            }
            if Instant::now() >= end {
                return Err(format!(
                    "等待 {kind} 响应丢失超时（proxy requests={}）",
                    self.proxy.requests()
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// 提交响应丢失后产品会按原 OpId 查询；等待那条查询应答并返回它的证据。
    async fn wait_query(&self, op_id: &str) -> Result<OperationEvent> {
        let end = Instant::now() + ACTION_TIMEOUT;
        loop {
            if let Some(event) = self
                .proxy
                .events()
                .into_iter()
                .find(|event| event.op_id == op_id && event.kind == "query" && !event.dropped)
            {
                return Ok(event);
            }
            if Instant::now() >= end {
                return Err(format!(
                    "等待 {op_id} 的恢复查询超时；events={:?}",
                    self.proxy.events()
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn converge(&mut self) -> Result<u32> {
        for pass in 0..4 {
            for index in 0..self.clients.len() {
                self.sync_client(index).await?;
            }
            let rows = self.entries().await?;
            let server: BTreeSet<String> = path_map(&rows)?.keys().cloned().collect();
            let mut matched = true;
            for index in 0..self.clients.len() {
                let local = self.clients[index].root.paths()?;
                if local != server {
                    matched = false;
                    let missing: Vec<_> = server.difference(&local).take(5).cloned().collect();
                    let extra: Vec<_> = local.difference(&server).take(5).cloned().collect();
                    println!(
                        "check=info converge pass={pass} client={} missing={missing:?} extra={extra:?}",
                        self.clients[index].spec.id
                    );
                }
            }
            if matched {
                println!(
                    "check=pass clients_converged pass={} paths={}",
                    pass + 1,
                    server.len()
                );
                return Ok(pass + 1);
            }
        }
        Err("四轮同步后客户端命名空间仍未收敛".into())
    }

    /// 最终逐端核对：路径集合必须与服务端一致；完整模式逐字节核对，按需端只允许
    /// `on_disk=0` 的占位，已经水合的文件仍必须与固定版本摘要一致。
    async fn verify_namespaces(&self, passes: u32) -> Result<Convergence> {
        let rows = self.entries().await?;
        let paths = path_map(&rows)?;
        let server_keys: BTreeSet<String> = paths.keys().cloned().collect();
        let mut client_paths = BTreeMap::new();
        let mut materialized = 0usize;
        let mut placeholders = 0usize;
        for client in &self.clients {
            let local = client.root.paths()?;
            client_paths.insert(client.spec.id.clone(), local.len());
            if local != server_keys {
                let missing: Vec<_> = server_keys.difference(&local).take(8).cloned().collect();
                let extra: Vec<_> = local.difference(&server_keys).take(8).cloned().collect();
                return Err(format!(
                    "{} 的本地命名空间未收敛：missing={missing:?} extra={extra:?}",
                    client.spec.id
                )
                .into());
            }
            let root = RootHandle::open(&client.root)?;
            let report = root.scan();
            if !report.permits_missing_comparison() {
                return Err(format!("{} 的最终扫描不完整", client.spec.id).into());
            }
            for entry in report.entries {
                let Some(metadata) = entry.metadata else {
                    return Err(format!("{}:{} 缺少元数据", client.spec.id, entry.path).into());
                };
                if metadata.directory {
                    continue;
                }
                let expected = paths
                    .get(&entry.path)
                    .ok_or_else(|| format!("{}:{} 不在服务端快照", client.spec.id, entry.path))?;
                let on_disk = metadata.cloud.as_ref().map(|cloud| cloud.on_disk);
                match on_disk {
                    Some(0) => {
                        if client.spec.mode != Mode::OnDemand {
                            return Err(format!(
                                "完整模式客户端 {} 的文件仍是占位：{}",
                                client.spec.id, entry.path
                            )
                            .into());
                        }
                        placeholders += 1;
                    }
                    _ => {
                        let bytes = fs::read(client.root.join(&entry.path))?;
                        let actual = hash_of(&bytes);
                        if Some(actual.as_str()) != expected.hash.as_deref() {
                            return Err(format!(
                                "{}:{} 本地字节与服务端摘要不同",
                                client.spec.id, entry.path
                            )
                            .into());
                        }
                        materialized += 1;
                    }
                }
            }
        }
        Ok(Convergence {
            passes,
            server_paths: server_keys.len(),
            client_paths,
            matched: true,
            full_materialized_bytes_verified: materialized,
            on_demand_placeholders: placeholders,
        })
    }

    async fn final_entries(&self) -> Result<Vec<EntryReport>> {
        let rows = self.entries().await?;
        let paths = path_map(&rows)?;
        Ok(paths
            .into_iter()
            .filter(|(_, entry)| entry.kind == EntryKind::File)
            .map(|(path, entry)| EntryReport {
                path,
                entry_id: entry.entry_id,
                hash: entry.hash.unwrap_or_default(),
                version_id: entry.version_id.unwrap_or_default(),
                size: entry.size.unwrap_or_default(),
            })
            .collect())
    }

    async fn coverage(&mut self, scenario: &Scenario) -> Result<Coverage> {
        let rows = self.entries().await?;
        let mut sources: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for entry in &rows {
            if let Some(hash) = &entry.hash {
                sources
                    .entry(hash.clone())
                    .or_default()
                    .insert(format!("current:{}", entry.name));
            }
        }
        for item in self.trash().await? {
            if let Some(hash) = &item.hash {
                sources
                    .entry(hash.clone())
                    .or_default()
                    .insert(format!("trash:{}", item.name));
            }
        }
        for entry in rows.iter().filter(|entry| entry.kind == EntryKind::File) {
            for item in self.history(&entry.entry_id).await? {
                if let Some(hash) = &item.hash {
                    sources
                        .entry(hash.clone())
                        .or_default()
                        .insert(format!("history:{}", item.id));
                }
            }
        }
        let mut covered = Vec::new();
        let mut missing = Vec::new();
        for (label, content) in scenario.inputs() {
            let hash = hash_of(content.as_bytes());
            let found = sources.get(&hash).cloned().unwrap_or_default();
            if found.is_empty() {
                missing.push(format!("{label} sha256={hash}"));
            }
            covered.push(Covered {
                label,
                hash,
                found_in: found.into_iter().collect(),
            });
        }
        Ok(Coverage {
            inputs: covered,
            missing,
        })
    }

    /// 成功收尾：停止 agent、注销 CFAPI 根并删除本 run 自己的客户端目录。
    ///
    /// 只处理 `run-e2e-<pid>` 下的三个客户端；不触碰其他卡的历史样本或端口。
    fn cleanup(&mut self, root_id: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        for index in 0..self.clients.len() {
            if let Err(error) = self.stop_client_blocking(index) {
                warnings.push(format!("stop {}: {error}", self.clients[index].spec.id));
            }
            if let Err(error) = remove_client(&self.clients[index], root_id) {
                warnings.push(format!("cleanup {}: {error}", self.clients[index].spec.id));
            }
        }
        warnings
    }

    fn stop_client_blocking(&mut self, index: usize) -> Result<()> {
        stop(&mut self.clients[index])
    }

    fn index(&self, id: &str) -> Result<usize> {
        self.clients
            .iter()
            .position(|client| client.spec.id == id)
            .ok_or_else(|| format!("未知客户端 {id}").into())
    }

    async fn entries(&self) -> Result<Vec<SnapshotEntry>> {
        entries(&self.api, &self.root).await
    }

    async fn trash(&self) -> Result<Vec<RecoveryItem>> {
        recovery(&self.api, &format!("/v1/roots/{}/trash", self.root)).await
    }

    async fn history(&self, entry: &str) -> Result<Vec<RecoveryItem>> {
        recovery(
            &self.api,
            &format!("/v1/roots/{}/entries/{entry}/history", self.root),
        )
        .await
    }

    async fn wait_server(
        &self,
        what: &str,
        predicate: impl Fn(&[SnapshotEntry]) -> bool,
    ) -> Result<()> {
        let end = Instant::now() + ACTION_TIMEOUT;
        loop {
            let rows = self.entries().await?;
            if predicate(&rows) {
                return Ok(());
            }
            if Instant::now() >= end {
                let paths: Vec<String> = path_map(&rows)?.keys().cloned().collect();
                return Err(format!("等待 {what} 超时；当前路径 {paths:?}").into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn start_client(&mut self, index: usize) -> Result<()> {
        self.stop_client(index).await?;
        write_config(&self.clients[index], &self.fixture)?;
        login(&self.clients[index], &self.agent, &self.fixture)?;
        spawn(&mut self.clients[index], &self.agent)?;
        let status = wait_ipc(&mut self.clients[index]).await?;
        let id = self.clients[index].spec.id.clone();
        self.clients[index].agent_version = Some(status.agent_version.clone());
        println!(
            "check=info client_started id={id} agent={} ipc={}.{}",
            status.agent_version, status.ipc_protocol.major, status.ipc_protocol.minor
        );
        Ok(())
    }

    async fn wait_idle(&mut self, index: usize) -> Result<()> {
        match wait_idle(&mut self.clients[index]).await {
            Ok(()) => Ok(()),
            Err(error) => {
                let status = request(&CommandName::GetStatus, None).await.ok();
                let problems = request(&CommandName::ListProblems, None).await.ok();
                let failure = json!({
                    "client": self.clients[index].spec.id,
                    "error": error.to_string(),
                    "bindings": status.and_then(|response| response.result).and_then(|result| result.bindings),
                    "problems": problems.and_then(|response| response.result).and_then(|result| result.management),
                    "proxy_events": self.proxy.events(),
                });
                let path = self.run_dir.join("failure.json");
                let _ = fs::write(
                    &path,
                    serde_json::to_vec_pretty(&failure).unwrap_or_default(),
                );
                let detail = once_diagnostic(&mut self.clients[index], &self.agent, &self.fixture);
                Err(format!("{error}; 一次性诊断：{detail}").into())
            }
        }
    }

    async fn stop_client(&mut self, index: usize) -> Result<()> {
        stop(&mut self.clients[index])
    }

    async fn sync_client(&mut self, index: usize) -> Result<()> {
        self.start_client(index).await?;
        self.wait_idle(index).await?;
        self.stop_client(index).await
    }
}

fn replay_request(
    event: &OperationEvent,
) -> Result<vaultsyncd_protocol::generated::OperationRequest> {
    let body = event
        .request_body
        .as_deref()
        .ok_or("代理没有记录首次提交的请求体")?;
    Ok(serde_json::from_str(body)?)
}

fn block_kind(block: &Block) -> &'static str {
    match block {
        Block::Import { .. } => "import",
        Block::OfflineEdit { .. } => "offline_edit",
        Block::Rename { .. } => "rename",
        Block::DeleteModify { .. } => "delete_modify",
        Block::LostResponse { .. } => "lost_response",
    }
}

fn final_conflicts(entries: &[EntryReport]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.path.contains("（冲突-"))
        .map(|entry| entry.path.clone())
        .collect()
}

fn is_conflict(entry: &SnapshotEntry) -> bool {
    entry.name.contains("（冲突-")
}

fn hash_index(rows: &[SnapshotEntry]) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        if let Some(hash) = &row.hash {
            index
                .entry(hash.clone())
                .or_default()
                .push(row.name.clone());
        }
    }
    index
}

fn snapshot_entry(rows: &[SnapshotEntry], path: &str) -> Result<SnapshotEntry> {
    let paths = path_map(rows)?;
    paths
        .get(path)
        .cloned()
        .ok_or_else(|| format!("服务端缺少路径 {path}").into())
}

fn path_map(rows: &[SnapshotEntry]) -> Result<BTreeMap<String, SnapshotEntry>> {
    let by_id: BTreeMap<&str, &SnapshotEntry> = rows
        .iter()
        .map(|entry| (entry.entry_id.as_str(), entry))
        .collect();
    let mut out = BTreeMap::new();
    for entry in rows {
        if entry.name.is_empty() {
            continue;
        }
        let path = path_of(entry, &by_id)?;
        if out.insert(path.clone(), entry.clone()).is_some() {
            return Err(format!("快照路径重复：{path}").into());
        }
    }
    Ok(out)
}

fn path_of(entry: &SnapshotEntry, by_id: &BTreeMap<&str, &SnapshotEntry>) -> Result<String> {
    let mut components = vec![entry.name.clone()];
    let mut current = entry;
    let mut depth = 0;
    while let Some(parent_id) = &current.parent_id {
        depth += 1;
        if depth > 64 {
            return Err("快照父链过深".into());
        }
        let parent = by_id
            .get(parent_id.as_str())
            .ok_or_else(|| format!("快照父条目缺失：{parent_id}"))?;
        if parent.name.is_empty() {
            break;
        }
        components.push(parent.name.clone());
        current = parent;
    }
    components.reverse();
    Ok(components.join("/"))
}

async fn entries(api: &Api, root: &str) -> Result<Vec<SnapshotEntry>> {
    let (snapshot, _) = sync::snapshot(api, root).await?;
    let mut all = Vec::new();
    let mut cursor = None;
    loop {
        let page = sync::snapshot_page(api, root, &snapshot.snapshot_id, cursor.as_deref()).await?;
        all.extend(page.entries);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(all);
        }
    }
}

async fn recovery(api: &Api, path: &str) -> Result<Vec<RecoveryItem>> {
    let mut items = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut url = path.to_owned();
        if let Some(after) = &after {
            url.push('?');
            url.push_str(&format!("after={after}"));
        }
        let response = api
            .send(ApiRequest::get(url).with_auth(AuthMode::Bearer))
            .await?;
        let page: RecoveryPage = response.json()?;
        items.extend(page.items);
        after = page.next_cursor;
        if after.is_none() {
            return Ok(items);
        }
    }
}

fn login(client: &Client, agent: &Path, fixture: &Fixture) -> Result<()> {
    let mut child = Command::new(agent)
        .arg("--config")
        .arg(&client.config)
        .arg("--login-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let request = json!({
        "login": fixture.login,
        "password": fixture.password,
        "device_label": format!("S074 {}", client.spec.label),
    });
    child
        .stdin
        .take()
        .ok_or("agent login stdin 不可用")?
        .write_all(&serde_json::to_vec(&request)?)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "{} 登录失败（exit {:?}）：{}",
            client.spec.id,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

fn write_config(client: &Client, fixture: &Fixture) -> Result<()> {
    let config = json!({
        "server": {
            "id": SERVER_ID,
            "url": fixture.url,
            "ca_file": fixture.ca_file,
        },
        "device_id": client.device_id,
        "state_dir": client.state,
        "poll_millis": 250,
        "bindings": [{
            "id": client.binding_id,
            "root_id": fixture.root,
            "path": client.root,
            "cache": client.cache,
            "mode": client.spec.mode.as_str(),
        }],
    });
    fs::write(&client.config, serde_json::to_vec(&config)?)?;
    Ok(())
}

fn spawn(client: &mut Client, agent: &Path) -> Result<()> {
    let log = fs::File::create(&client.log)?;
    let child = Command::new(agent)
        .arg("--config")
        .arg(&client.config)
        .creation_flags(0x0000_0200)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()?;
    client.child = Some(ChildGuard(child));
    Ok(())
}

async fn wait_ipc(client: &mut Client) -> Result<GetStatusResult> {
    let end = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(child) = client.child.as_mut() {
            if let Some(status) = child.0.try_wait()? {
                let log = fs::read_to_string(&client.log).unwrap_or_default();
                return Err(format!(
                    "{} 的 agent 提前退出（{:?}）：{}",
                    client.spec.id,
                    status.code(),
                    log.chars().take(2000).collect::<String>()
                )
                .into());
            }
        }
        match request(&CommandName::GetStatus, None).await {
            Ok(response) if response.kind == ResponseKind::Ok => {
                let result = response.result.ok_or("get_status 缺少结果")?;
                let bindings = result
                    .bindings
                    .as_ref()
                    .ok_or("get_status 缺少真实 bindings")?;
                if bindings
                    .iter()
                    .any(|binding| binding.binding_id == client.binding_id)
                {
                    return Ok(result);
                }
            }
            Ok(response) => {
                return Err(format!(
                    "{} 的 get_status 失败：{:?}",
                    client.spec.id,
                    response.error.map(|error| error.message)
                )
                .into());
            }
            Err(_) => {}
        }
        if Instant::now() >= end {
            return Err(format!(
                "{} 的 named pipe IPC 在 {START_TIMEOUT:?} 内不可用",
                client.spec.id
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_idle(client: &mut Client) -> Result<()> {
    let end = Instant::now() + START_TIMEOUT;
    let mut idle_since = None;
    loop {
        if let Some(child) = client.child.as_mut() {
            if let Some(status) = child.0.try_wait()? {
                return Err(format!(
                    "{} 的 agent 在同步中退出（{:?}）",
                    client.spec.id,
                    status.code()
                )
                .into());
            }
        }
        let response = request(&CommandName::GetStatus, None).await;
        let binding = response.ok().and_then(|response| {
            if response.kind != ResponseKind::Ok {
                return None;
            }
            let result = response.result?;
            let bindings = result.bindings?;
            let binding = bindings
                .into_iter()
                .find(|binding| binding.binding_id == client.binding_id)?;
            Some(binding)
        });
        if let Some(binding) = binding {
            if idle(&binding) {
                let since = idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(900) {
                    println!(
                        "check=pass client_idle id={} pending={} intents={} event_lag={} dirty={}",
                        client.spec.id,
                        binding.pending,
                        binding.intents,
                        binding.event_lag,
                        binding.dirty
                    );
                    return Ok(());
                }
            } else {
                idle_since = None;
            }
        }
        if Instant::now() >= end {
            if let Ok(problems) = request(&CommandName::ListProblems, None).await
                && let Some(result) = problems.result
                && let Some(management) = result.management
            {
                eprintln!(
                    "check=info problems id={} {:?}",
                    client.spec.id, management.problems
                );
            }
            let last = request(&CommandName::GetStatus, None).await.ok();
            return Err(format!(
                "{} 在 {START_TIMEOUT:?} 内未收敛：{:?}",
                client.spec.id,
                last.and_then(|response| response.result)
                    .and_then(|result| result.bindings)
                    .map(|bindings| bindings.into_iter().next().map(|b| (
                        b.pending,
                        b.intents,
                        b.event_lag,
                        b.dirty,
                        b.unknown,
                        b.recovery,
                        b.failed,
                        b.transfers,
                        b.offline,
                        b.partial,
                        b.synchronized,
                        b.mode_pending
                    )))
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 失败诊断：停掉后台 agent 后以 `--once` 再跑一轮，产品会把真实错误 detail 打到 stderr。
fn once_diagnostic(client: &mut Client, agent: &Path, fixture: &Fixture) -> String {
    let _ = stop(client);
    let _ = write_config(client, fixture);
    match Command::new(agent)
        .arg("--config")
        .arg(&client.config)
        .arg("--once")
        .output()
    {
        Ok(output) => format!(
            "exit={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => format!("spawn failed: {error}"),
    }
}

fn idle(binding: &BindingStatus) -> bool {
    [
        "dirty",
        "pending",
        "failed",
        "intents",
        "transfers",
        "offline",
        "partial",
        "recovery",
        "unknown",
        "event_lag",
    ]
    .iter()
    .all(|field| match *field {
        "dirty" => binding.dirty == "0",
        "pending" => binding.pending == "0",
        "failed" => binding.failed == "0",
        "intents" => binding.intents == "0",
        "transfers" => binding.transfers == "0",
        "offline" => binding.offline == "0",
        "partial" => binding.partial == "0",
        "recovery" => binding.recovery == "0",
        "unknown" => binding.unknown == "0",
        "event_lag" => binding.event_lag == "0",
        _ => false,
    }) && binding.synchronized
        && !binding.mode_pending
        && !binding.paused
}

async fn request(name: &CommandName, parameters: Option<CommandParameters>) -> Result<Response> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let request_id = format!(
        "s074-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let endpoint = Endpoint::current("vaultsyncd-agent")?;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        endpoint.request(&Request {
            request_id,
            ipc_version: version::supported(),
            command: IpcCommand {
                name: *name,
                parameters,
            },
        }),
    )
    .await
    .map_err(|_| "IPC 请求超时")??;
    Ok(response)
}

fn stop(client: &mut Client) -> Result<()> {
    let Some(mut child) = client.child.take() else {
        return Ok(());
    };
    let pid = child.0.id();
    unsafe {
        GenerateConsoleCtrlEvent(1, pid);
    }
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                println!(
                    "check=warn agent_stop_status id={} code={:?}",
                    client.spec.id,
                    status.code()
                );
            }
            return Ok(());
        }
        if Instant::now() >= end {
            println!("check=warn agent_forced_kill id={}", client.spec.id);
            child.0.kill()?;
            child.0.wait()?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn write_file(root: &Path, relative: &str, content: &str) -> Result<()> {
    let target = root.join(relative);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target, content.as_bytes())?;
    Ok(())
}

fn guest_version() -> String {
    unsafe {
        let mut info: OsVersionInfoW = std::mem::zeroed();
        info.size = std::mem::size_of::<OsVersionInfoW>() as u32;
        if RtlGetVersion(&mut info) == 0 {
            format!(
                "Windows {}.{} build {} {}",
                info.major,
                info.minor,
                info.build,
                std::env::consts::ARCH
            )
        } else {
            format!("Windows unknown {}", std::env::consts::ARCH)
        }
    }
}

#[repr(C)]
struct OsVersionInfoW {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AllocConsole() -> i32;
    fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
}

fn hash_of(bytes: &[u8]) -> String {
    to_hex(&digest_of(bytes))
}

fn id32() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut seed = Vec::new();
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    seed.extend_from_slice(
        &SEQUENCE
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    seed.extend_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
            .to_le_bytes(),
    );
    let digest = digest_of(&seed);
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

trait RootExt {
    fn paths(&self) -> Result<BTreeSet<String>>;
    fn inspect_or_err(
        &self,
        relative: &str,
    ) -> Result<vaultsyncd_platform_windows::files::EntryMetadata>;
    fn find_content(&self, hash: &str) -> Result<bool>;
}

impl RootExt for PathBuf {
    fn paths(&self) -> Result<BTreeSet<String>> {
        let root = RootHandle::open(self)?;
        let report = root.scan();
        if !report.permits_missing_comparison() {
            return Err(format!("本地扫描不完整：{:?}", report.health).into());
        }
        Ok(report
            .entries
            .into_iter()
            .filter(|entry| entry.issue.is_none())
            .map(|entry| entry.path)
            .collect())
    }

    fn inspect_or_err(
        &self,
        relative: &str,
    ) -> Result<vaultsyncd_platform_windows::files::EntryMetadata> {
        Ok(RootHandle::open(self)?.inspect(relative)?)
    }

    fn find_content(&self, hash: &str) -> Result<bool> {
        let root = RootHandle::open(self)?;
        let report = root.scan();
        for entry in report.entries {
            let Some(metadata) = entry.metadata else {
                continue;
            };
            if metadata.directory {
                continue;
            }
            if metadata
                .cloud
                .as_ref()
                .is_some_and(|cloud| cloud.on_disk == 0)
            {
                continue;
            }
            if let Ok(bytes) = fs::read(self.join(&entry.path))
                && hash_of(&bytes) == hash
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn placeholder_on_disk(root: &Path, relative: &str) -> Result<Option<u64>> {
    let metadata = RootHandle::open(root)?.inspect(relative)?;
    Ok(metadata.cloud.map(|cloud| cloud.on_disk))
}

fn remove_client(client: &Client, root_id: &str) -> Result<()> {
    use std::sync::Arc;
    use vaultsyncd_domain::ids::{BindingId, RootId};
    use vaultsyncd_platform_windows::cfapi::{
        adapter::{Adapter, MetadataOnly},
        catalog::Catalog,
    };

    if let Ok(root) = RootHandle::open(&client.root) {
        let report = root.scan();
        let mut entries = report.entries;
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.path.matches('/').count()));
        for entry in entries {
            let target = client.root.join(&entry.path);
            let is_directory = entry
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.directory);
            if is_directory {
                let _ = fs::remove_dir(target);
            } else {
                let _ = fs::remove_file(target);
            }
        }
        drop(root);
    }
    let binding = BindingId::from_hex(&client.binding_id)?;
    let root = RootId::from_hex(root_id)?;
    let mut adapter = Adapter::connect(
        &client.root,
        Catalog::new(binding, root),
        Arc::new(MetadataOnly),
    )?;
    adapter.unregister()?;
    drop(adapter);
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match fs::remove_dir_all(&client.root) {
            Ok(()) => break,
            Err(error) if Instant::now() >= end => return Err(error.into()),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    for path in [&client.cache, &client.state] {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::remove_dir_all(path) {
                Ok(()) => break,
                Err(error) if Instant::now() >= end => return Err(error.into()),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
    Ok(())
}
