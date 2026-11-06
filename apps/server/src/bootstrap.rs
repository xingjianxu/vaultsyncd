//! 依赖组装：把配置、数据库、启动恢复、写入门与 HTTP 路由接在一起（design §5.3 的
//! `apps/server/src/bootstrap.rs`：组装，不重新实现冲突规则或内容提交算法）。
//!
//! 启动顺序就是设计里“服务端文件”的启动顺序（design §11.3 末段）：
//!
//! 1. 打开服务端数据库（种类/版本门禁、迁移、WAL 校验都在 `crates/server-store` 里，任一步失败
//!    就不给可写句柄 → 本进程拒绝启动）；
//! 2. 逐个已注册 Root 评估启动恢复（[`crate::recovery`]），把结论写进 [`Lifecycle`]；
//! 3. 只有所有 Root 都给出“不需要停写”的结论时，readiness 才变真；
//! 4. 绑定监听地址后才开始服务请求。
//!
//! 停止顺序相反：先进入 `draining`（拒绝新的提交），再让 HTTP 收尾，最后统计并打印仍然保留的
//! 未完成意图条数后关闭数据库。整个停止流程不删除意图行、暂存文件或已发布内容。

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tracing::{error, info, warn};

use vaultsyncd_server_store::db::{Database, DbError};
use vaultsyncd_server_store::state_lock::{StateLock, StateLockError};

use vaultsyncd_platform_linux_server::mounts::probe::ScanPolicy;

use crate::auth;
use crate::config::Config;
use crate::content::ContentContext;
use crate::http;
use crate::lifecycle::{Lifecycle, Phase};
use crate::recovery::{self, RecoveryAssessment, RecoveryAssessmentError};
use crate::roots::RootContext;

/// 启动失败：进程必须以非零退出码结束，且从未开放写入。
#[derive(Debug)]
pub enum StartupError {
    /// 状态目录已被另一个进程占用（另一个服务端，或正在执行的离线 init）。
    StateLock(StateLockError),
    /// 数据库打开/迁移失败（含种类与版本门禁）。
    Database(DbError),
    /// 启动恢复无法给出结论。
    Recovery(RecoveryAssessmentError),
    /// 监听地址绑定失败（端口被占用、权限不足等）。
    Bind {
        /// 配置里的监听地址。
        addr: SocketAddr,
        /// 原始 I/O 错误。
        source: io::Error,
    },
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupError::StateLock(error) => write!(f, "{error}"),
            StartupError::Database(error) => write!(f, "打开服务端数据库失败：{error}"),
            StartupError::Recovery(error) => write!(f, "{error}"),
            StartupError::Bind { addr, source } => {
                write!(f, "绑定监听地址 {addr} 失败：{source}")
            }
        }
    }
}

impl std::error::Error for StartupError {}

/// 运行期失败（服务循环本身出错）。
#[derive(Debug)]
pub enum RunError {
    /// HTTP 服务循环返回错误。
    Serve(io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Serve(source) => write!(f, "HTTP 服务循环失败：{source}"),
        }
    }
}

impl std::error::Error for RunError {}

impl From<io::Error> for RunError {
    fn from(source: io::Error) -> Self {
        RunError::Serve(source)
    }
}

/// 已准备好对外服务的进程状态。
pub struct Service {
    config: Config,
    database: Database,
    /// 状态目录锁：活到进程停止为止，让离线 init 与第二个服务端都无法同时进入同一状态目录。
    _state_lock: StateLock,
    lifecycle: Arc<Lifecycle>,
    listener: TcpListener,
    assessment: RecoveryAssessment,
}

impl Service {
    /// 当前生命周期（供进程入口打印与测试观察）。
    #[must_use]
    pub fn lifecycle(&self) -> &Arc<Lifecycle> {
        &self.lifecycle
    }

    /// 启动恢复的结论。
    #[must_use]
    pub fn assessment(&self) -> &RecoveryAssessment {
        &self.assessment
    }

    /// 实际绑定的地址（配置用 `:0` 时由内核分配）。
    ///
    /// # Errors
    ///
    /// 取本地地址失败（套接字已关闭）。
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 运行到 `shutdown` 完成为止，然后按顺序优雅停止。
    ///
    /// 顺序：`draining`（拒绝新的提交）→ HTTP 等待进行中的请求结束 → 打印仍保留的未完成意图条数
    /// → 关闭数据库。证据不在这里清理：下次启动的恢复流程仍然看得到它们（design §11.3）。
    ///
    /// # Errors
    ///
    /// HTTP 服务循环出错时返回 [`RunError::Serve`]；停止流程本身仍会执行。
    pub async fn serve(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), RunError> {
        let Service {
            config,
            database,
            _state_lock,
            lifecycle,
            listener,
            assessment,
        } = self;

        let addr = listener.local_addr()?;
        info!(
            listen = %addr,
            state_dir = %config.state_dir.display(),
            schema_version = database.schema_version(),
            phase = lifecycle.phase().as_str(),
            open_roots = assessment.open_roots,
            blocked_roots = assessment.blocked_roots.len(),
            "服务端开始监听"
        );
        if !assessment.is_complete() {
            warn!(
                detail = %assessment.detail(),
                "启动恢复没有完成：进程保持只读，写入不开放"
            );
        }

        let auth = Arc::new(auth::AuthContext::new(database.clone()));
        let uploads = Arc::new(crate::uploads::UploadsContext::new(
            Arc::clone(&auth),
            &config.state_dir,
        ));
        // 允许基目录来自配置；没有配置时注册端点一律拒绝（不是“不限制”）。
        let allowed_root_bases = config.allowed_root_bases.clone();
        info!(
            allowed_root_bases = allowed_root_bases.as_ref().map_or(0, |bases| bases.len()),
            "Root 注册的允许基目录"
        );
        if allowed_root_bases.is_none() {
            warn!("没有配置 [roots].allowed_bases：Root 注册端点会拒绝一切路径");
        }
        let roots = Arc::new(RootContext::new(
            Arc::clone(&auth),
            allowed_root_bases,
            config.state_dir.clone(),
            ScanPolicy::default(),
        ));
        let content = Arc::new(ContentContext::new(Arc::clone(&auth), &config.state_dir));
        // 操作命令端点（S047/S052）：命令走真实适配器；缺少整组证据的目录恢复仍显式未就绪。
        // 外部观察循环（S052）：周期扫描每个健康 Root，把外部改动采纳成版本与事件。
        let observer = Arc::new(crate::external::ObserverRuntime::new(
            database.clone(),
            &config.state_dir,
            crate::external::ObserverRuntimeConfig::DEFAULT,
        ));
        let executor = Arc::new(crate::executor::ExecutorContext::new(
            database.clone(),
            &config.state_dir,
            Some(Arc::clone(&observer)),
        ));
        let operations = Arc::new(crate::operations::OperationsContext::new(
            Arc::clone(&auth),
            executor,
        ));
        // 快照、changes 与 SSE（S048）：SSE 在 draining 时主动结束，不拖住优雅停止。
        let sync = Arc::new(crate::sync::SyncContext::new(
            Arc::clone(&auth),
            Arc::clone(&lifecycle),
        ));
        let router = http::router(
            Arc::clone(&lifecycle),
            auth,
            uploads,
            roots,
            content,
            operations,
            sync,
        );
        // 观察循环用独立线程 + current-thread runtime：端口实现（`RootHandle` 与 trait 对象）不要求
        // `Send`，而 HTTP 服务的多线程 runtime 不能承载非 `Send` 的 future。
        let stop_observer = Arc::new(tokio::sync::Notify::new());
        let observer_thread = {
            let observer = Arc::clone(&observer);
            let stop = Arc::clone(&stop_observer);
            std::thread::Builder::new()
                .name("vaultsyncd-observer".to_owned())
                .spawn(move || {
                    match tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                    {
                        Ok(runtime) => runtime.block_on(observer.run(stop)),
                        Err(error) => warn!(error = %error, "外部观察循环 runtime 建立失败"),
                    }
                })
                .map_err(|error| warn!(error = %error, "外部观察线程启动失败"))
                .ok()
        };
        let draining = Arc::clone(&lifecycle);
        let graceful = async move {
            shutdown.await;
            draining.begin_shutdown();
            info!(
                phase = Phase::Draining.as_str(),
                "收到停止信号：停止接受新的提交，等待进行中的请求结束"
            );
        };

        let serve_result = axum::serve(listener, router)
            .with_graceful_shutdown(graceful)
            .await;

        // 观察循环在关闭数据库之前停下：它的每一轮都在自己的短事务里提交/回滚。
        stop_observer.notify_one();
        if let Some(thread) = observer_thread
            && thread.join().is_err()
        {
            warn!("外部观察线程没有正常退出");
        }

        // 证据账目放在关闭数据库之前：这是“停止不丢任务”的可核对输出。
        match recovery::count_unfinished_intents(&database).await {
            Ok(kept) => info!(
                unfinished_intents = kept,
                "仍保留未完成意图（下次启动继续恢复，停止流程不清理证据）"
            ),
            Err(error) => warn!(error = %error, "停止时统计未完成意图失败"),
        }
        database.close().await;
        info!(phase = "stopped", "服务端已停止");

        serve_result.map_err(RunError::Serve)
    }
}

/// 组装并准备就绪：打开数据库、评估启动恢复、绑定监听地址。
///
/// 恢复没完成不是启动失败（进程仍可提供健康探测），但 readiness 保持为假、写入保持关闭。
///
/// # Errors
///
/// 见 [`StartupError`]：数据库打不开、恢复读不出结论或监听地址绑不上时返回错误，调用方必须以
/// 非零退出码结束，不能降级成“没恢复也先接写入”。
pub async fn prepare(config: Config) -> Result<Service, StartupError> {
    // 先抢状态目录锁，再开数据库：顺序反了就会出现“两个进程都已经打开数据库、再去争锁”的窗口，
    // 而 init 也正好会在这个窗口里看到“没有锁”就动手改账号表。
    let state_lock = StateLock::try_acquire_exclusive(&config.state_dir).map_err(|error| {
        error!(error = %error, state_dir = %config.state_dir.display(), "状态目录不可独占，拒绝启动");
        StartupError::StateLock(error)
    })?;
    info!(
        lock = %state_lock.path().display(),
        "已独占状态目录"
    );
    info!(
        config_database = %config.database.display(),
        "打开服务端数据库"
    );
    let database = Database::open(&config.database)
        .await
        .map_err(StartupError::Database)?;

    let lifecycle = Arc::new(Lifecycle::new());
    let assessment = recovery::assess_registered_roots(&database, &lifecycle)
        .await
        .map_err(|error| {
            error!(error = %error, "启动恢复无法给出结论，拒绝启动");
            StartupError::Recovery(error)
        })?;
    if assessment.is_complete() {
        lifecycle.mark_ready();
        info!(open_roots = assessment.open_roots, "启动恢复完成，写入开放");
    } else {
        lifecycle.mark_needs_recovery(assessment.detail());
    }

    let listener = TcpListener::bind(config.listen).await.map_err(|source| {
        error!(addr = %config.listen, error = %source, "绑定监听地址失败");
        StartupError::Bind {
            addr: config.listen,
            source,
        }
    })?;

    Ok(Service {
        config,
        database,
        _state_lock: state_lock,
        lifecycle,
        listener,
        assessment,
    })
}
