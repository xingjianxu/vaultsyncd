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

use crate::config::Config;
use crate::http;
use crate::lifecycle::{Lifecycle, Phase};
use crate::recovery::{self, RecoveryAssessment, RecoveryAssessmentError};

/// 启动失败：进程必须以非零退出码结束，且从未开放写入。
#[derive(Debug)]
pub enum StartupError {
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

        let router = http::router(Arc::clone(&lifecycle));
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
        lifecycle,
        listener,
        assessment,
    })
}
