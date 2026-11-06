//! 外部内容采纳的 SQLx 适配器与后台观察循环（S052）。
//!
//! S049/S050 落地了观察规则、平台监视器与端口，但进程里没有后台任务、`ExternalAdoptStore` 也只有
//! 测试替身：注册 Root 之后外部修改永远不会产生版本与事件。本模块补上两件事：
//!
//! - [`SqlExternalStore`]（端口 `ExternalAdoptStore`）：按路径查已知条目；在一个
//!   `BEGIN IMMEDIATE` 事务里登记对象（幂等）、追加事件批次、写版本与条目 revision；移动只改父目录
//!   与名称（EntryId 不变、不搬内容）；删除按可恢复删除写回收站清单与墓碑并保留版本历史。
//! - [`ObserverRuntime`]：每个 Root 一条后台循环，用 `RootHandle` + `LinuxWorkTree` +
//!   `LinuxObjectPort` 周期性跑一轮 `ExternalObserver::run_pass`。**没有 inotify 接线**（`RootWatcher`
//!   的 `spawn_blocking` 循环属后续卡）：周期扫描是 S050 三层兜底里的正确性层，事件只是线索，因此
//!   不接监视只会更慢，不会更不准。Root 由每次循环从库里重新枚举，新注册的 Root 下一轮自动开始观察。
//!
//! 服务端自己的写入通过 `ExternalObserver::expect_own_write` 登记（设备 + inode + 内容）：观察到同一
//! inode 同一内容时不会把这次发布再采纳成“外部修改”；同一 inode 内容变了就是真的外部修改。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};
use vaultsyncd_domain::ids::{EntryId, RootId, TxId, UserId, VersionId};
use vaultsyncd_domain::version::{Revision, UtcTimestamp};
use vaultsyncd_platform_linux_server::files::RootHandle;
use vaultsyncd_server_core::commit::put::RootCommitGate;
use vaultsyncd_server_core::objects::{ContentDigest, ExpectedContent, ObjectLayout};
use vaultsyncd_server_core::observer::{
    ExternalObserver, ObserverPorts, OwnWriteIntent, PassClock, PassLimits, ScanLimits,
    StabilityWindow,
};
use vaultsyncd_server_core::ports::external_store::{
    AdoptKind, ExternalAdoptStore, ExternalDelete, ExternalDeleteRecord, ExternalMove,
    ExternalMoveRecord, ExternalRecord, ExternalStoreError, ExternalWrite, KnownEntry,
};
use vaultsyncd_server_core::ports::object_store::FileKey;
use vaultsyncd_server_core::ports::put_store::EntryKind;
use vaultsyncd_server_store::changes as store_changes;
use vaultsyncd_server_store::db::Database;
use vaultsyncd_server_store::entries as store_entries;
use vaultsyncd_server_store::entries::{EntryError, NewEntry};
use vaultsyncd_server_store::roots as store_roots;
use vaultsyncd_server_store::trash as store_trash;
use vaultsyncd_server_store::versions as store_versions;
use vaultsyncd_server_store::versions::{NewObject, NewVersion, VersionError, VersionOrigin};

use crate::executor::{events_json, server_now};
use crate::observer::{LinuxObjectPort, LinuxWorkTree};
use crate::uploads::random_bytes;

/// 内容历史保留期（与 design §9.4 的 30 天一致）：回收站条目的最早回收时间。
const TRASH_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

/// 外部采纳的 SQLx 实现。
pub struct SqlExternalStore {
    database: Database,
    owner: UserId,
}

impl SqlExternalStore {
    /// 以数据库与 Root 所属账号装配。
    #[must_use]
    pub fn new(database: Database, owner: UserId) -> Self {
        Self { database, owner }
    }
}

fn store_error(detail: impl std::fmt::Display) -> ExternalStoreError {
    ExternalStoreError::Storage {
        detail: detail.to_string(),
    }
}

async fn connect(
    database: &Database,
) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, ExternalStoreError> {
    database.pool().acquire().await.map_err(store_error)
}

impl ExternalAdoptStore for SqlExternalStore {
    async fn lookup_path(
        &mut self,
        root: RootId,
        path: &str,
    ) -> Result<Option<KnownEntry>, ExternalStoreError> {
        let mut conn = connect(&self.database).await?;
        let record = store_entries::entry_by_path(&mut conn, self.owner, root, path)
            .await
            .map_err(store_error)?;
        let Some(record) = record else {
            return Ok(None);
        };
        let content = if record.kind == store_entries::EntryKind::File {
            store_versions::current_version(&mut conn, self.owner, root, record.id)
                .await
                .map_err(store_error)?
                .map(|version| {
                    ExpectedContent::new(
                        ContentDigest::from_bytes(version.object_hash),
                        version.size,
                    )
                })
        } else {
            None
        };
        Ok(Some(KnownEntry {
            entry: record.id,
            revision: record.revision,
            kind: match record.kind {
                store_entries::EntryKind::File => EntryKind::File,
                store_entries::EntryKind::Directory => EntryKind::Directory,
            },
            content,
        }))
    }

    async fn apply_external(
        &mut self,
        write: &ExternalWrite,
    ) -> Result<ExternalRecord, ExternalStoreError> {
        let mut tx = self.database.begin_write().await.map_err(store_error)?;
        let parts: Vec<&str> = write.path.split('/').filter(|p| !p.is_empty()).collect();
        let Some((file_name, parents)) = parts.split_last() else {
            return Err(ExternalStoreError::Storage {
                detail: "采纳路径为空".to_owned(),
            });
        };
        let file_name = *file_name;

        // 缺失的父目录按目录条目补建（外部程序可以在服务端没见过的目录里建文件）。
        let mut parent = store_entries::root_entry(&mut tx, self.owner, write.root)
            .await
            .map_err(store_error)?
            .ok_or(ExternalStoreError::UnknownRoot { root: write.root })?;
        let mut walked = String::new();
        for part in parents {
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(part);
            match store_entries::entry_by_path(&mut tx, self.owner, write.root, &walked)
                .await
                .map_err(store_error)?
            {
                Some(existing) => parent = existing,
                None => {
                    parent = store_entries::insert_entry(
                        &mut tx,
                        self.owner,
                        &NewEntry {
                            id: EntryId::from_bytes(random_bytes()),
                            root: write.root,
                            parent: parent.id,
                            name: (*part).to_owned(),
                            name_key: (*part).to_owned(),
                            kind: store_entries::EntryKind::Directory,
                            revision: Revision::ZERO,
                            content_version: None,
                        },
                    )
                    .await
                    .map_err(store_error)?;
                }
            }
        }

        let (entry, revision) = match write.kind {
            AdoptKind::Create => {
                let record = store_entries::insert_entry(
                    &mut tx,
                    self.owner,
                    &NewEntry {
                        id: EntryId::from_bytes(random_bytes()),
                        root: write.root,
                        parent: parent.id,
                        name: file_name.to_owned(),
                        name_key: file_name.to_owned(),
                        kind: store_entries::EntryKind::File,
                        revision: Revision::ZERO,
                        content_version: None,
                    },
                )
                .await
                .map_err(store_error)?;
                (record.id, Revision::ZERO)
            }
            AdoptKind::Update {
                entry,
                expected_revision,
            } => {
                let record = store_entries::entry_in_scope(&mut tx, self.owner, entry)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| ExternalStoreError::Rejected {
                        code: "entry_missing",
                        detail: format!("条目 {entry} 不存在"),
                    })?;
                if record.revision != expected_revision {
                    return Err(ExternalStoreError::RevisionChanged {
                        expected: expected_revision,
                        actual: record.revision,
                    });
                }
                (entry, expected_revision.next().map_err(store_error)?)
            }
        };

        let tx_id = TxId::from_bytes(random_bytes());
        let version = VersionId::from_bytes(random_bytes());
        let batch = store_changes::append_change(
            &mut tx,
            self.owner,
            write.root,
            tx_id,
            &events_json(
                "external",
                entry,
                Some(&write.path),
                revision,
                Some((version, write.content.digest, write.content.bytes)),
                "external",
            ),
            write.at,
        )
        .await
        .map_err(store_error)?;

        store_versions::record_object(
            &mut tx,
            self.owner,
            &NewObject {
                root: write.root,
                hash: *write.content.digest.as_bytes(),
                size: write.content.bytes,
                storage_path: write.storage_path.clone(),
                verified_at: write.at,
            },
        )
        .await
        .map_err(store_error)?;

        store_versions::record_version(
            &mut tx,
            self.owner,
            &NewVersion {
                id: version,
                root: write.root,
                entry,
                object_hash: *write.content.digest.as_bytes(),
                size: write.content.bytes,
                created_seq: batch.seq,
                origin: VersionOrigin::External,
                at: write.at,
            },
        )
        .await
        .map_err(|error: VersionError| store_error(error))?;

        if let AdoptKind::Update {
            expected_revision, ..
        } = write.kind
        {
            let next = expected_revision.next().map_err(store_error)?;
            store_entries::advance_entry_revision(
                &mut tx,
                self.owner,
                entry,
                expected_revision,
                next,
            )
            .await
            .map_err(|error: EntryError| match error {
                EntryError::RevisionChanged { expected, actual } => {
                    ExternalStoreError::RevisionChanged { expected, actual }
                }
                other => store_error(other),
            })?;
        }

        tx.commit().await.map_err(store_error)?;

        Ok(ExternalRecord {
            entry,
            version,
            revision,
            seq: batch.seq,
            tx: batch.tx,
            digest: write.content.digest,
            size: write.content.bytes,
        })
    }

    async fn apply_external_move(
        &mut self,
        request: &ExternalMove,
    ) -> Result<ExternalMoveRecord, ExternalStoreError> {
        let mut tx = self.database.begin_write().await.map_err(store_error)?;
        let current = store_entries::entry_in_scope(&mut tx, self.owner, request.entry)
            .await
            .map_err(store_error)?
            .ok_or_else(|| ExternalStoreError::Rejected {
                code: "entry_missing",
                detail: format!("条目 {} 不存在", request.entry),
            })?;
        if current.revision != request.expected_revision {
            return Err(ExternalStoreError::RevisionChanged {
                expected: request.expected_revision,
                actual: current.revision,
            });
        }
        let parts: Vec<&str> = request.to.split('/').filter(|p| !p.is_empty()).collect();
        let Some((name, parent_parts)) = parts.split_last() else {
            return Err(ExternalStoreError::Rejected {
                code: "invalid_path",
                detail: "移动目标路径为空".to_owned(),
            });
        };
        let name = *name;
        let parent_path = parent_parts.join("/");
        let parent = store_entries::entry_by_path(&mut tx, self.owner, request.root, &parent_path)
            .await
            .map_err(store_error)?
            .ok_or_else(|| ExternalStoreError::PathConflict {
                detail: format!("目标父目录 {:?} 不存在", request.to),
            })?;
        let next = request.expected_revision.next().map_err(store_error)?;
        let tx_id = TxId::from_bytes(random_bytes());
        let batch = store_changes::append_change(
            &mut tx,
            self.owner,
            request.root,
            tx_id,
            &events_json(
                "move",
                request.entry,
                Some(&request.to),
                next,
                None,
                "external",
            ),
            request.at,
        )
        .await
        .map_err(store_error)?;
        store_entries::move_entry(
            &mut tx,
            self.owner,
            request.entry,
            parent.id,
            name,
            name,
            next,
        )
        .await
        .map_err(|error: EntryError| match error {
            EntryError::DuplicateName { parent, name } => ExternalStoreError::PathConflict {
                detail: format!("父目录 {parent} 下已有 {name:?}（{}）", request.to),
            },
            EntryError::RevisionChanged { expected, actual } => {
                ExternalStoreError::RevisionChanged { expected, actual }
            }
            other => store_error(other),
        })?;
        tx.commit().await.map_err(store_error)?;
        Ok(ExternalMoveRecord {
            entry: request.entry,
            revision: next,
            seq: batch.seq,
            tx: batch.tx,
        })
    }

    async fn apply_external_delete(
        &mut self,
        request: &ExternalDelete,
    ) -> Result<ExternalDeleteRecord, ExternalStoreError> {
        let mut tx = self.database.begin_write().await.map_err(store_error)?;
        let current = store_entries::entry_in_scope(&mut tx, self.owner, request.entry)
            .await
            .map_err(store_error)?
            .ok_or_else(|| ExternalStoreError::Rejected {
                code: "entry_missing",
                detail: format!("条目 {} 不存在", request.entry),
            })?;
        if current.revision != request.expected_revision {
            return Err(ExternalStoreError::RevisionChanged {
                expected: request.expected_revision,
                actual: current.revision,
            });
        }
        let parent = current.parent.ok_or_else(|| ExternalStoreError::Rejected {
            code: "root_entry",
            detail: "Root 根条目不能删除".to_owned(),
        })?;
        let next = request.expected_revision.next().map_err(store_error)?;
        let content = match current.content_version {
            Some(version) => {
                let facts =
                    store_versions::version_facts(&mut tx, self.owner, request.root, version)
                        .await
                        .map_err(store_error)?
                        .ok_or_else(|| ExternalStoreError::Rejected {
                            code: "missing_version",
                            detail: format!("删除版本 {version} 缺少版本事实"),
                        })?;
                Some((
                    version,
                    vaultsyncd_server_core::objects::ContentDigest::from_bytes(facts.object_hash),
                    facts.size,
                ))
            }
            None => None,
        };
        let tx_id = TxId::from_bytes(random_bytes());
        let batch = store_changes::append_change(
            &mut tx,
            self.owner,
            request.root,
            tx_id,
            &events_json(
                "delete",
                request.entry,
                Some(&request.path),
                next,
                content,
                "external",
            ),
            request.at,
        )
        .await
        .map_err(store_error)?;
        store_entries::advance_entry_revision(
            &mut tx,
            self.owner,
            request.entry,
            request.expected_revision,
            next,
        )
        .await
        .map_err(store_error)?;
        store_entries::soft_delete_entry(&mut tx, self.owner, request.entry, request.at)
            .await
            .map_err(store_error)?;
        let expires_at = UtcTimestamp::from_unix_seconds(
            request
                .at
                .as_unix_seconds()
                .saturating_add(TRASH_RETENTION_SECONDS),
        )
        .map_err(store_error)?;
        let trash = store_trash::NewTrashEntry {
            entry: request.entry,
            root: request.root,
            parent,
            path: request.path.clone(),
            kind: match current.kind {
                store_entries::EntryKind::File => store_entries::EntryKind::File,
                store_entries::EntryKind::Directory => store_entries::EntryKind::Directory,
            },
            revision: next,
            content_version: current.content_version,
            deleted_at: request.at,
            expires_at,
        };
        store_trash::record_trash_entry(&mut tx, self.owner, &trash)
            .await
            .map_err(store_error)?;
        store_trash::insert_tombstone(&mut tx, self.owner, &trash)
            .await
            .map_err(store_error)?;
        tx.commit().await.map_err(store_error)?;
        Ok(ExternalDeleteRecord {
            entry: request.entry,
            revision: next,
            seq: batch.seq,
            tx: batch.tx,
        })
    }
}

/// 后台观察循环的配置。
#[derive(Clone, Copy, Debug)]
pub struct ObserverRuntimeConfig {
    /// 两轮之间的间隔（毫秒）。
    pub interval_millis: u64,
    /// 稳定窗口（毫秒）：文件指纹保持不变这么久才采集。
    pub window_millis: u64,
    /// 一轮最多采集的文件数。
    pub max_captures: usize,
    /// 一次扫描最多检查的目录项数。
    pub max_scan_entries: usize,
}

impl ObserverRuntimeConfig {
    /// 默认值：1 s 间隔、1 s 稳定窗口（与 `StabilityWindow::DEFAULT` 同值）。
    pub const DEFAULT: ObserverRuntimeConfig = ObserverRuntimeConfig {
        interval_millis: 1000,
        window_millis: 1000,
        max_captures: 64,
        max_scan_entries: 16_384,
    };
}

struct RootState {
    handle: Arc<RootHandle>,
    observer: ExternalObserver,
}

/// 后台观察循环：每个 Root 一个 `ExternalObserver`，按周期跑一轮完整扫描与采纳。
pub struct ObserverRuntime {
    pub(crate) fences: Arc<crate::root_fences::RootFences>,
    database: Database,
    state_dir: PathBuf,
    roots: tokio::sync::Mutex<HashMap<RootId, RootState>>,
    /// Root 提交串行化：与内容 put 共用同一套语义（同一 Root 同时只允许一次提交）。
    gate: tokio::sync::Mutex<RootCommitGate>,
    config: ObserverRuntimeConfig,
    collection:
        tokio::sync::RwLock<HashMap<RootId, vaultsyncd_protocol::generated::RootCollection>>,
}

impl ObserverRuntime {
    /// 建立运行时（不启动任务）。
    #[must_use]
    pub fn new(database: Database, state_dir: &Path, config: ObserverRuntimeConfig) -> Self {
        Self {
            fences: Arc::new(crate::root_fences::RootFences::new(state_dir)),
            database,
            state_dir: state_dir.to_path_buf(),
            roots: tokio::sync::Mutex::new(HashMap::new()),
            gate: tokio::sync::Mutex::new(RootCommitGate::new()),
            config,
            collection: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn with_root_fences(mut self, fences: Arc<crate::root_fences::RootFences>) -> Self {
        self.fences = fences;
        self
    }
    /// 本进程最新扫描进度；未扫描和重启不得继承完成结论。
    pub async fn collection_status(
        &self,
        root: RootId,
    ) -> vaultsyncd_protocol::generated::RootCollection {
        self.collection
            .read()
            .await
            .get(&root)
            .cloned()
            .unwrap_or_else(crate::roots::not_scanned)
    }

    async fn collection_state(
        &self,
        root: RootId,
        state: vaultsyncd_protocol::generated::RootCollectionState,
        detail: &str,
    ) {
        let mut states = self.collection.write().await;
        let value = states.entry(root).or_insert_with(crate::roots::not_scanned);
        value.state = state;
        value.detail = detail.into();
    }

    /// HTTP 命令和观察器共用的提交门；锁顺序固定为 gate → roots。
    pub fn commit_gate(&self) -> &tokio::sync::Mutex<RootCommitGate> {
        &self.gate
    }

    /// 登记一次服务端自己的写入（设备 + inode + 内容），避免把发布本身采纳成外部修改。
    ///
    /// 观察器还没有为该 Root 建立时忽略：那一轮还没开始，之后的完整扫描会用库里的内容判断
    /// “内容相同 → 不产生版本”。
    pub async fn expect_own_write(
        &self,
        root: RootId,
        path: &str,
        identity: FileKey,
        content: ExpectedContent,
    ) {
        let _ = identity.links;
        let mut roots = self.roots.lock().await;
        let Some(state) = roots.get_mut(&root) else {
            return;
        };
        let intent = OwnWriteIntent {
            device: identity.device,
            inode: identity.inode,
            content,
        };
        if let Err(error) = state.observer.expect_own_write(path, intent) {
            warn!(root = %root.to_hex(), path, error = %error, "自写意图登记失败");
        }
    }

    /// 跑一轮（测试与运维诊断用）：枚举库里的 Root，逐个 Root 做一次完整扫描。
    ///
    /// 返回本轮 Root 数与采纳/失败的统计，便于日志与验收断言。
    pub async fn run_once(&self) -> PassSummary {
        let _objects = self.database.object_activity().await;
        let roots = {
            let mut conn = match self.database.pool().acquire().await {
                Ok(conn) => conn,
                Err(error) => {
                    for progress in self.collection.write().await.values_mut() {
                        progress.state =
                            vaultsyncd_protocol::generated::RootCollectionState::Blocked;
                        progress.detail =
                            "无法读取 Root 清单，请检查存储；上次完成结果已失效。".into();
                    }
                    warn!(error = %error, "观察器读 Root 清单失败");
                    return PassSummary::default();
                }
            };
            match store_roots::all_roots(&mut conn).await {
                Ok(roots) => roots,
                Err(error) => {
                    for progress in self.collection.write().await.values_mut() {
                        progress.state =
                            vaultsyncd_protocol::generated::RootCollectionState::Blocked;
                        progress.detail =
                            "无法读取 Root 清单，请检查存储；上次完成结果已失效。".into();
                    }
                    warn!(error = %error, "观察器读 Root 清单失败");
                    return PassSummary::default();
                }
            }
        };
        let mut summary = PassSummary::default();
        let mut gate = self.gate.lock().await;
        let mut states = self.roots.lock().await;
        let live: std::collections::BTreeSet<RootId> = roots.iter().map(|root| root.id).collect();
        states.retain(|root, _| live.contains(root));
        self.collection
            .write()
            .await
            .retain(|root, _| live.contains(root));
        for root in roots {
            use vaultsyncd_protocol::generated::RootCollectionState as Collection;
            self.collection_state(
                root.id,
                Collection::Scanning,
                "正在核验本轮扫描前提；尚不能判为完成。",
            )
            .await;
            if root.health != store_roots::RootHealth::Healthy {
                self.collection_state(
                    root.id,
                    Collection::Blocked,
                    "Root 数据库健康状态不可用，等待修复。",
                )
                .await;
                continue;
            }
            let admitted = match self.fences.open(&root) {
                Ok(handle) => handle,
                Err(reason) => {
                    self.collection_state(
                        root.id,
                        Collection::Blocked,
                        &format!("Root 围栏拒绝采集：{reason}；不推导删除。"),
                    )
                    .await;
                    continue;
                }
            };
            // 恢复之前观察器也不能把一次尚未提交的服务端发布误采纳为新的外部事实。
            let unfinished = match self.database.pool().acquire().await {
                Ok(mut conn) => {
                    vaultsyncd_server_store::intents::unfinished_intents(
                        &mut conn, root.owner, root.id,
                    )
                    .await
                }
                Err(error) => {
                    self.collection_state(
                        root.id,
                        Collection::Blocked,
                        "无法读取恢复意图，暂停采集；请检查存储。",
                    )
                    .await;
                    warn!(root = %root.id, error = %error, "无法读取意图，暂停观察采纳");
                    continue;
                }
            };
            match unfinished {
                Ok(intents) if intents.is_empty() => {}
                Ok(_) => {
                    self.collection_state(
                        root.id,
                        Collection::Blocked,
                        "仍有未完成文件意图，等待恢复后采集。",
                    )
                    .await;
                    continue;
                }
                Err(error) => {
                    self.collection_state(
                        root.id,
                        Collection::Blocked,
                        "无法读取恢复意图，暂停采集；请检查存储。",
                    )
                    .await;
                    warn!(root = %root.id, error = %error, "无法读取意图，暂停观察采纳");
                    continue;
                }
            }
            self.collection_state(
                root.id,
                Collection::Scanning,
                "正在进行本进程的目录扫描与稳定采集。",
            )
            .await;
            summary.roots += 1;
            let state = match states.get_mut(&root.id) {
                Some(state) => state,
                None => {
                    let handle = admitted;
                    let prefix = format!("obs-{}-{}", &root.id.to_hex()[..8], std::process::id());
                    let Ok(observer) = ExternalObserver::new(
                        root.id,
                        StabilityWindow::from_millis(self.config.window_millis)
                            .unwrap_or(StabilityWindow::DEFAULT),
                        &prefix,
                    ) else {
                        continue;
                    };
                    states
                        .entry(root.id)
                        .or_insert(RootState { handle, observer })
                }
            };
            let layout = ObjectLayout::new(self.state_dir.join("objects"));
            let mut work = LinuxWorkTree::new(Arc::clone(&state.handle));
            let mut blobs = LinuxObjectPort::new(Arc::clone(&state.handle), layout.root());
            let mut store = SqlExternalStore::new(self.database.clone(), root.owner);
            let Ok(limits) = PassLimits::new(
                ScanLimits::new(self.config.max_scan_entries)
                    .unwrap_or_else(|_| ScanLimits::new(1024).expect("非零")),
                self.config.max_captures,
            ) else {
                continue;
            };
            let clock = PassClock {
                now_ms: monotonic_millis(),
                at: server_now()
                    .unwrap_or_else(|_| UtcTimestamp::from_unix_seconds(0).expect("零时刻合法")),
            };
            let mut ports = ObserverPorts {
                work: &mut work,
                blobs: &mut blobs,
                layout: &layout,
                gate: &mut gate,
                store: &mut store,
            };
            let report = state.observer.run_pass(&mut ports, limits, clock).await;
            let pending = report
                .outcomes
                .iter()
                .filter(|outcome| {
                    matches!(
                        outcome.state,
                        vaultsyncd_server_core::observer::FileState::Pending(_)
                    )
                })
                .count();
            let blocked = report.root_unavailable
                || !report.inconclusive.is_empty()
                || !report.skipped.is_empty();
            let collection_state = if blocked {
                Collection::Blocked
            } else if report.initial_import_complete && report.scan_complete && pending == 0 {
                Collection::Complete
            } else {
                Collection::Pending
            };
            let detail = if blocked {
                format!(
                    "扫描存在不可采集项或权限/挂载故障，不能判定完整：{:?}；跳过 {} 项。请检查服务日志并修复。",
                    report.inconclusive,
                    report.skipped.len()
                )
            } else if collection_state == Collection::Complete {
                "本进程已完成一次初始稳定采集；这不代表客户端已同步。".into()
            } else {
                "等待文件稳定或后续扫描；当前项数不是总文件数，也不是同步完成百分比。".into()
            };
            self.collection.write().await.insert(
                root.id,
                vaultsyncd_protocol::generated::RootCollection {
                    state: collection_state,
                    examined: report.entries_examined.to_string(),
                    pending: pending.to_string(),
                    detail,
                },
            );

            summary.adopted += report
                .outcomes
                .iter()
                .filter(|outcome| {
                    matches!(
                        outcome.state,
                        vaultsyncd_server_core::observer::FileState::Adopted { .. }
                    )
                })
                .count();
            summary.pending += report
                .outcomes
                .iter()
                .filter(|outcome| {
                    matches!(
                        outcome.state,
                        vaultsyncd_server_core::observer::FileState::Pending(_)
                    )
                })
                .count();
            summary.root_unavailable |= report.root_unavailable;
            summary.moves += report.reconcile.moves.len();
            summary.deletes += report.reconcile.deletions.len();
            debug!(
                root = %root.id.to_hex(),
                scanned = report.entries_examined,
                complete = report.scan_complete,
                "观察器完成一轮"
            );
        }
        summary
    }

    /// 后台循环：直到 `stop` 收到通知。任务退出前不会遗留数据库事务（事务都在本轮内提交/回滚）。
    pub async fn run(self: Arc<Self>, stop: Arc<tokio::sync::Notify>) {
        info!(
            interval_ms = self.config.interval_millis,
            window_ms = self.config.window_millis,
            "外部观察循环启动"
        );
        loop {
            let summary = self.run_once().await;
            // run_once 的对象读门已释放；维护必须先拿独占门再开短事务，不能嵌套升级锁。
            match crate::executor::server_now() {
                Ok(now) => match crate::gc::run_once(&self.database, &self.state_dir, now).await {
                    Ok(report)
                        if report.objects > 0 || report.parts > 0 || report.temporary_files > 0 =>
                    {
                        info!(
                            objects = report.objects,
                            parts = report.parts,
                            temporary_files = report.temporary_files,
                            "GC 清理完成"
                        )
                    }
                    Ok(_) => {}
                    Err(error) => warn!(error=%error,"GC 未完成；保留持久证据，下轮重试"),
                },
                Err(error) => warn!(error=%error,"时钟无效，跳过 GC"),
            }
            if summary.roots > 0 {
                debug!(
                    roots = summary.roots,
                    adopted = summary.adopted,
                    pending = summary.pending,
                    moves = summary.moves,
                    deletes = summary.deletes,
                    "外部观察一轮结束"
                );
            }
            let wait = tokio::time::sleep(Duration::from_millis(self.config.interval_millis));
            tokio::pin!(wait);
            tokio::select! {
                () = &mut wait => {}
                () = stop.notified() => break,
            }
        }
        info!("外部观察循环停止");
    }
}

/// 一轮的统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassSummary {
    /// 本轮观察的 Root 数。
    pub roots: usize,
    /// 采纳的文件数。
    pub adopted: usize,
    /// 仍在等待稳定窗口/重试的文件数。
    pub pending: usize,
    /// 传播的外部移动数。
    pub moves: usize,
    /// 传播的外部删除数。
    pub deletes: usize,
    /// 是否遇到 Root 不可用。
    pub root_unavailable: bool,
}

fn monotonic_millis() -> u64 {
    // 稳定窗口只需要单调读数；用进程启动以来的时长，避免墙钟跳变。
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}
