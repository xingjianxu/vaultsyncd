//! 远端事件接收、重放与应用进度（design §9.3、I09）。
//!
//! 这里把“收到事件”和“应用到本地”明确分成两个阶段：接收阶段只在一个 SQLite 短事务中
//! 写入批次、远端视图、待执行任务和 `received_seq`；应用阶段完成文件/平台动作后，才在另一个
//! 短事务中写基线并推进 `applied_seq`。因此任意持久化点崩溃都能从 `received_changes` 重放，
//! 不会把提示当成应用完成。
//!
//! SSE（或其它通知）只调用 [`EventReceiver::run_cycle`] 作为唤醒；该方法始终可以被周期调度器
//! 调用，不依赖提示是否到达。事件内容是数据，解码器必须显式把它映射成远端条目和任务，未知
//! 或损坏的内容不会被猜成空批次。

use std::fmt;
use std::sync::Arc;

use vaultsyncd_client_store::db::Database;
use vaultsyncd_client_store::entries::{self, BaselineEntry, RemoteEntry};
use vaultsyncd_client_store::events as store_events;
use vaultsyncd_domain::error::ErrorCode;
use vaultsyncd_domain::ids::{BindingId, RootId, TxId};
use vaultsyncd_domain::version::RootSeq;
use vaultsyncd_protocol::generated::{ChangeBatch, ChangeList};
use vaultsyncd_transfer::ports::{PortError, PortFuture};

use vaultsyncd_client_store::queue::NewPendingOp;

/// 拉取端口：SSE 不属于这个端口；提示只会促使调用方更早再次调用 `pull`。
pub trait ChangeSource: Send + Sync {
    /// 拉取 `after` 之后的一页完整事务批次。
    fn pull(
        &self,
        root: RootId,
        after: RootSeq,
        limit: u32,
    ) -> PortFuture<'_, Result<ChangeList, PortError>>;
}

/// 一批待解码的事件数据。
///
/// 该形态同时用于 wire 批次和重启后从 `received_changes` 读出的批次，确保恢复路径不会依赖
/// 进程内缓存。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventEnvelope {
    /// 批次序号。
    pub seq: RootSeq,
    /// 服务端事务身份。
    pub tx: TxId,
    /// 服务端原样事件数组文本。
    pub events_json: String,
}

impl EventEnvelope {
    fn from_wire(batch: &ChangeBatch) -> Result<Self, DecodeError> {
        let seq = parse_seq(&batch.seq, "seq")?;
        let tx = parse_id(&batch.tx_id, "tx_id", TxId::from_hex)?;
        Ok(Self {
            seq,
            tx,
            events_json: batch.events_json.clone(),
        })
    }

    fn from_stored(batch: &store_events::ReceivedBatch) -> Self {
        Self {
            seq: batch.seq,
            tx: batch.tx,
            events_json: batch.events_json.clone(),
        }
    }
}

/// 解码器把服务端事件数据转换为 client-store 的事务输入。
///
/// 该 trait 必须同时支持 wire 和重启重放；实现不得通过“无法解析就返回空 `changes`/`tasks`”
/// 吞掉事件。`IncomingBatch` 的 `binding`、`seq`、`tx`、`events_json` 必须与 envelope 一致。
pub trait EventDecoder: Send + Sync {
    /// 解码一批事件。业务事件格式由提交协调定义，核心不猜字段含义。
    fn decode(
        &self,
        binding: BindingId,
        envelope: &EventEnvelope,
    ) -> Result<store_events::IncomingBatch, DecodeError>;
}

/// 应用器的结果：`Blocked` 只阻塞有依赖的任务，应用器应继续处理同批次中无依赖的任务。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    /// 所有本批次任务与本地落盘均已完成，可以推进 baseline/applied_cursor。
    Applied,
    /// 至少一个任务未完成；事件仍保留，应用游标不越过它。
    Blocked(ErrorCode),
}

/// 本地应用端口。实现通常把 `tasks` 交给 S055 执行器，并按依赖关系独立推进无关任务。
pub trait EventApplier: Send + Sync {
    /// 应用一批已经持久化的远端状态。
    fn apply(
        &self,
        binding: BindingId,
        changes: &[RemoteEntry],
        tasks: &[NewPendingOp],
    ) -> PortFuture<'_, Result<ApplyResult, PortError>>;
}

/// 一轮接收/应用的统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveReport {
    /// 新接收并持久化的批次数。
    pub received: usize,
    /// 本轮应用完成的批次数（重复批次不重复计数）。
    pub applied: usize,
    /// 因任务失败而保持 pending 的批次数。
    pub blocked: usize,
    /// 已发现但没有再次写入的重复/陈旧提示或批次。
    pub duplicates: usize,
}

/// 接收引擎错误。错误发生时不会把内存里的“成功”状态写入数据库。
#[derive(Debug)]
pub enum ReceiveError {
    /// 远端拉取失败。
    Source(PortError),
    /// wire 或重放数据不符合领域形状。
    Decode(DecodeError),
    /// client-store 事务失败。
    Store(store_events::EventError),
    /// 应用阶段的端口失败。
    Apply(PortError),
    /// 服务端返回了错误 Root 或不递增的批次页。
    Protocol {
        /// 具体协议错误。
        detail: String,
    },
}

impl fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "拉取事件失败：{error}"),
            Self::Decode(error) => write!(f, "解码事件失败：{error}"),
            Self::Store(error) => write!(f, "持久化事件失败：{error}"),
            Self::Apply(error) => write!(f, "应用事件失败：{error}"),
            Self::Protocol { detail } => write!(f, "事件协议错误：{detail}"),
        }
    }
}

impl std::error::Error for ReceiveError {}

impl From<PortError> for ReceiveError {
    fn from(error: PortError) -> Self {
        Self::Source(error)
    }
}

impl From<DecodeError> for ReceiveError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<store_events::EventError> for ReceiveError {
    fn from(error: store_events::EventError) -> Self {
        Self::Store(error)
    }
}

/// wire 数据或数据库重放数据的形状错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    /// 出错字段。
    pub field: &'static str,
    /// 人类可读原因。
    pub detail: String,
}

impl DecodeError {
    /// 构造一个明确的解码错误。
    #[must_use]
    pub fn new(field: &'static str, detail: impl Into<String>) -> Self {
        Self {
            field,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.field, self.detail)
    }
}

impl std::error::Error for DecodeError {}

/// 绑定作用域的事件接收器。
pub struct EventReceiver<S, D, A> {
    database: Arc<Database>,
    root: RootId,
    binding: BindingId,
    source: Arc<S>,
    decoder: Arc<D>,
    applier: Arc<A>,
    page_limit: u32,
}

impl<S, D, A> EventReceiver<S, D, A>
where
    S: ChangeSource + 'static,
    D: EventDecoder + 'static,
    A: EventApplier + 'static,
{
    /// 构造接收器；`page_limit` 必须在服务端 changes 上限内且非零。
    pub fn new(
        database: Arc<Database>,
        root: RootId,
        binding: BindingId,
        source: Arc<S>,
        decoder: Arc<D>,
        applier: Arc<A>,
        page_limit: u32,
    ) -> Result<Self, ReceiveError> {
        if page_limit == 0 || page_limit > 500 {
            return Err(ReceiveError::Protocol {
                detail: format!("page_limit 必须在 1..=500 内：{page_limit}"),
            });
        }
        Ok(Self {
            database,
            root,
            binding,
            source,
            decoder,
            applier,
            page_limit,
        })
    }

    /// 运行一次周期轮询。`hint` 只是 SSE 唤醒提示，过旧/重复提示不会重复应用；即使没有提示也
    /// 会拉取，保证断线后的周期兜底。
    pub async fn run_cycle(&self, hint: Option<RootSeq>) -> Result<ReceiveReport, ReceiveError> {
        let mut report = self.resume_pending().await?;
        let mut after = self.current_received().await?;
        if hint.is_some_and(|hint| hint <= after) {
            // 仍然已经完成了 resume_pending；提示没有新游标，不必发重复网络请求。
            return Ok(report);
        }

        loop {
            let page = self
                .source
                .pull(self.root, after, self.page_limit)
                .await
                .map_err(ReceiveError::Source)?;
            validate_page(&page, self.root, after)?;
            let page_last = page.batches.last().map(|batch| batch.seq.as_str());
            for wire in &page.batches {
                let envelope = EventEnvelope::from_wire(wire)?;
                let current = self.current_received().await?;
                if envelope.seq <= current {
                    report.duplicates += 1;
                    continue;
                }
                // changes 接口保证严格升序；出现空洞时不接收，避免收到 5 后永久丢掉迟到的 4。
                let expected = current
                    .get()
                    .checked_add(1)
                    .and_then(|value| RootSeq::new(value).ok())
                    .ok_or_else(|| ReceiveError::Protocol {
                        detail: format!("批次 {} 溢出", envelope.seq),
                    })?;
                if envelope.seq != expected {
                    return Err(ReceiveError::Protocol {
                        detail: format!("批次乱序或有空洞：期望 {expected}，收到 {}", envelope.seq),
                    });
                }
                let incoming = self.decoder.decode(self.binding, &envelope)?;
                self.receive_and_apply(incoming, &mut report).await?;
                after = envelope.seq;
            }
            if !page.has_more {
                break;
            }
            let next = page
                .next_after
                .parse::<u64>()
                .ok()
                .and_then(|value| RootSeq::new(value).ok());
            let next = next.ok_or_else(|| ReceiveError::Protocol {
                detail: "next_after 不是合法 RootSeq".to_owned(),
            })?;
            if next <= after || page_last.is_none() {
                return Err(ReceiveError::Protocol {
                    detail: "has_more=true 但 next_after 没有前进".to_owned(),
                });
            }
            after = next;
        }
        Ok(report)
    }

    /// 重启恢复：先处理所有已接收但尚未应用的批次，再由周期拉取接续。
    pub async fn resume_pending(&self) -> Result<ReceiveReport, ReceiveError> {
        let mut report = ReceiveReport::default();
        let batches =
            {
                let mut conn = self.database.pool().acquire().await.map_err(|error| {
                    ReceiveError::Protocol {
                        detail: format!("读取未应用批次连接失败：{error}"),
                    }
                })?;
                store_events::unapplied_batches(&mut conn, self.binding).await?
            };
        for stored in batches {
            let envelope = EventEnvelope::from_stored(&stored);
            let incoming = self.decoder.decode(self.binding, &envelope)?;
            self.apply_existing(stored, incoming, &mut report).await?;
        }
        Ok(report)
    }

    async fn current_received(&self) -> Result<RootSeq, ReceiveError> {
        let mut conn =
            self.database
                .pool()
                .acquire()
                .await
                .map_err(|error| ReceiveError::Protocol {
                    detail: format!("读取接收游标连接失败：{error}"),
                })?;
        Ok(store_events::cursors(&mut conn, self.binding)
            .await?
            .received)
    }

    async fn receive_and_apply(
        &self,
        incoming: store_events::IncomingBatch,
        report: &mut ReceiveReport,
    ) -> Result<(), ReceiveError> {
        let mut tx = self
            .database
            .begin_write()
            .await
            .map_err(|error| ReceiveError::Protocol {
                detail: format!("开始接收事务失败：{error}"),
            })?;
        let stored = store_events::receive_batch(&mut tx, &incoming).await?;
        tx.commit().await.map_err(|error| ReceiveError::Protocol {
            detail: format!("提交接收事务失败：{error}"),
        })?;
        if stored.state == store_events::BatchState::Applied {
            report.duplicates += 1;
            return Ok(());
        }
        report.received += 1;
        self.apply_existing(stored, incoming, report).await
    }

    async fn apply_existing(
        &self,
        stored: store_events::ReceivedBatch,
        incoming: store_events::IncomingBatch,
        report: &mut ReceiveReport,
    ) -> Result<(), ReceiveError> {
        match self
            .applier
            .apply(self.binding, &incoming.changes, &incoming.tasks)
            .await
            .map_err(ReceiveError::Apply)?
        {
            ApplyResult::Applied => {
                let mut tx =
                    self.database
                        .begin_write()
                        .await
                        .map_err(|error| ReceiveError::Protocol {
                            detail: format!("开始应用事务失败：{error}"),
                        })?;
                for remote in &incoming.changes {
                    entries::put_baseline(&mut tx, self.binding, &baseline(remote))
                        .await
                        .map_err(store_events::EventError::Entry)?;
                }
                store_events::mark_batch_applied(&mut tx, self.binding, stored.seq).await?;
                tx.commit().await.map_err(|error| ReceiveError::Protocol {
                    detail: format!("提交应用事务失败：{error}"),
                })?;
                report.applied += 1;
            }
            ApplyResult::Blocked(code) => {
                let mut tx =
                    self.database
                        .begin_write()
                        .await
                        .map_err(|error| ReceiveError::Protocol {
                            detail: format!("开始阻塞标记事务失败：{error}"),
                        })?;
                store_events::mark_batch_blocked(&mut tx, self.binding, stored.seq, code).await?;
                tx.commit().await.map_err(|error| ReceiveError::Protocol {
                    detail: format!("提交阻塞标记失败：{error}"),
                })?;
                report.blocked += 1;
            }
        }
        Ok(())
    }
}

fn baseline(remote: &RemoteEntry) -> BaselineEntry {
    BaselineEntry {
        entry: remote.entry,
        parent: remote.parent,
        name: remote.name.clone(),
        kind: remote.kind,
        revision: remote.revision,
        content_version: remote.content_version,
        deleted: remote.deleted,
    }
}

fn validate_page(page: &ChangeList, root: RootId, after: RootSeq) -> Result<(), ReceiveError> {
    if page.root_id != root.to_hex() {
        return Err(ReceiveError::Protocol {
            detail: format!("响应 Root {} 与绑定 Root {} 不同", page.root_id, root),
        });
    }
    let mut previous = after;
    for batch in &page.batches {
        let seq = parse_seq(&batch.seq, "seq")?;
        if seq <= previous {
            return Err(ReceiveError::Protocol {
                detail: format!("changes 页不是严格递增：{seq} <= {previous}"),
            });
        }
        previous = seq;
    }
    Ok(())
}

fn parse_seq(text: &str, field: &'static str) -> Result<RootSeq, DecodeError> {
    let value = text
        .parse::<u64>()
        .map_err(|_| DecodeError::new(field, "不是规范十进制"))?;
    RootSeq::new(value).map_err(|_| DecodeError::new(field, "必须大于 0 且不超过上限"))
}

fn parse_id<T>(
    text: &str,
    field: &'static str,
    parse: fn(&str) -> Result<T, vaultsyncd_domain::ids::IdParseError>,
) -> Result<T, DecodeError> {
    parse(text).map_err(|error| DecodeError::new(field, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use vaultsyncd_client_store::bindings::{self, NewBinding, SyncMode};
    use vaultsyncd_domain::ids::{DeviceId, ServerId};
    use vaultsyncd_domain::version::ServerEpoch;

    struct NoSource;
    impl ChangeSource for NoSource {
        fn pull(
            &self,
            _: RootId,
            _: RootSeq,
            _: u32,
        ) -> PortFuture<'_, Result<ChangeList, PortError>> {
            Box::pin(async {
                Err(PortError::local(
                    vaultsyncd_transfer::ports::LocalErrorKind::Read,
                    "不应拉取",
                ))
            })
        }
    }

    struct StaticSource {
        used: AtomicBool,
        root: String,
    }

    impl ChangeSource for StaticSource {
        fn pull(
            &self,
            _: RootId,
            after: RootSeq,
            _: u32,
        ) -> PortFuture<'_, Result<ChangeList, PortError>> {
            let first = after == RootSeq::ZERO && !self.used.swap(true, Ordering::SeqCst);
            let root = self.root.clone();
            Box::pin(async move {
                Ok(ChangeList {
                    batches: if first {
                        vec![ChangeBatch {
                            created_at: "1".into(),
                            events_json: "[{\"kind\":\"changed\"}]".into(),
                            seq: "1".into(),
                            tx_id: "01010101010101010101010101010101".into(),
                        }]
                    } else {
                        Vec::new()
                    },
                    has_more: false,
                    head_seq: "1".into(),
                    next_after: "1".into(),
                    root_id: root,
                    server_epoch: "1".into(),
                })
            })
        }
    }

    struct Decoder;
    impl EventDecoder for Decoder {
        fn decode(
            &self,
            binding: BindingId,
            envelope: &EventEnvelope,
        ) -> Result<store_events::IncomingBatch, DecodeError> {
            Ok(store_events::IncomingBatch {
                binding,
                seq: envelope.seq,
                tx: envelope.tx,
                events_json: envelope.events_json.clone(),
                changes: vec![],
                tasks: vec![],
            })
        }
    }

    struct Applier;
    impl EventApplier for Applier {
        fn apply(
            &self,
            _: BindingId,
            _: &[RemoteEntry],
            _: &[NewPendingOp],
        ) -> PortFuture<'_, Result<ApplyResult, PortError>> {
            Box::pin(async { Ok(ApplyResult::Applied) })
        }
    }

    fn ids(marker: u8) -> (BindingId, RootId) {
        (
            BindingId::from_bytes([marker; 16]),
            RootId::from_bytes([marker + 1; 16]),
        )
    }

    async fn database(marker: u8) -> (Arc<Database>, BindingId, RootId, std::path::PathBuf) {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时间")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("vaultsyncd-s057-{marker:02x}-{suffix}"));
        std::fs::create_dir_all(&dir).expect("临时目录");
        let database = Arc::new(Database::open(dir.join("client.db")).await.expect("数据库"));
        let (binding, root) = ids(marker);
        let mut tx = database.begin_write().await.expect("事务");
        bindings::insert_binding(
            &mut tx,
            &NewBinding {
                id: binding,
                server: ServerId::from_bytes([0x01; 16]),
                device: DeviceId::from_bytes([0x02; 16]),
                root,
                local_path: "local".into(),
                cache_path: "cache".into(),
                mode: SyncMode::Full,
                rules_version: "1".into(),
                server_epoch: ServerEpoch::new(1).expect("epoch"),
            },
        )
        .await
        .expect("绑定");
        tx.commit().await.expect("提交");
        (database, binding, root, dir)
    }

    #[tokio::test]
    async fn a_batch_is_received_then_applied_and_persisted() {
        let (database, binding, root, dir) = database(0x42).await;
        let source = Arc::new(StaticSource {
            used: AtomicBool::new(false),
            root: root.to_hex(),
        });
        let receiver = EventReceiver::new(
            Arc::clone(&database),
            root,
            binding,
            source,
            Arc::new(Decoder),
            Arc::new(Applier),
            100,
        )
        .expect("接收器");
        let report = receiver
            .run_cycle(Some(RootSeq::new(1).expect("提示")))
            .await
            .expect("周期");
        assert_eq!(report.received, 1);
        assert_eq!(report.applied, 1);
        let mut conn = database.pool().acquire().await.expect("连接");
        assert_eq!(
            store_events::cursors(&mut conn, binding)
                .await
                .expect("游标")
                .applied,
            RootSeq::new(1).expect("seq")
        );
        drop(conn);
        drop(database);
        std::fs::remove_dir_all(dir).expect("清理临时目录");
    }

    #[tokio::test]
    async fn a_repeated_hint_only_resumes_and_does_not_pull_again() {
        let (database, binding, root, dir) = database(0x41).await;
        let receiver = EventReceiver::new(
            Arc::clone(&database),
            root,
            binding,
            Arc::new(NoSource),
            Arc::new(Decoder),
            Arc::new(Applier),
            100,
        )
        .expect("接收器");
        let report = receiver.run_cycle(Some(RootSeq::ZERO)).await.expect("周期");
        assert_eq!(report, ReceiveReport::default());
        drop(database);
        std::fs::remove_dir_all(dir).expect("清理临时目录");
    }

    #[test]
    fn stored_envelope_keeps_the_wire_identity() {
        let batch = store_events::ReceivedBatch {
            seq: RootSeq::new(2).expect("seq"),
            tx: TxId::from_bytes([3; 16]),
            events_json: "[{\"x\":1}]".into(),
            state: store_events::BatchState::Received,
            last_error: None,
        };
        assert_eq!(
            EventEnvelope::from_stored(&batch).tx,
            TxId::from_bytes([3; 16])
        );
    }
}
