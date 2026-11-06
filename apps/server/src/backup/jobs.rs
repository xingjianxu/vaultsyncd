//! 持久备份作业；连接的10秒截止不会取消已接受的prepare/verify/release。
use crate::config::Config;
use sqlx::Row;
use std::sync::Arc;
use std::{collections::HashMap, io, path::Path};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use vaultsyncd_domain::ids::{BackupLeaseId, UserId};
use vaultsyncd_platform_linux_server::backup::Arena;
use vaultsyncd_platform_linux_server::files::{ResolveMode, RestrictedDir, hash::digest};
use vaultsyncd_protocol::generated::{
    BackupIntegrity, BackupSnapshotList, BackupSnapshotPhase, BackupSnapshotStatus,
};
use vaultsyncd_server_core::backup::export::{CHUNK, Index, Integrity, MAX_MANIFEST};
use vaultsyncd_server_store::{db::Database, leases};

pub(crate) fn failure(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s)
}
/// 每个在线实例一个；所有消费者共享作业和备份读/释放锁。
pub struct Context {
    pub(crate) db: Database,
    pub(crate) config: Config,
    locks: Mutex<HashMap<BackupLeaseId, Arc<RwLock<()>>>>,
    running: Mutex<std::collections::HashSet<(BackupLeaseId, String)>>,
    jobs: Mutex<Vec<JoinHandle<()>>>,
}
/// 已按owner与固定摘要验证的只读视图。
pub(crate) struct View {
    pub index: Index,
    pub manifest: Vec<u8>,
    pub directory: Arc<RestrictedDir>,
}
impl Context {
    /// 仅在线服务组装调用；DB克隆共享S086对象门。
    pub fn new(db: Database, config: Config) -> Self {
        Self {
            db,
            config,
            locks: Mutex::new(HashMap::new()),
            running: Mutex::new(Default::default()),
            jobs: Mutex::new(Vec::new()),
        }
    }
    pub(crate) async fn lock(&self, id: BackupLeaseId) -> Arc<RwLock<()>> {
        self.locks
            .lock()
            .await
            .entry(id)
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }
    /// owner与每个资源请求都重新校验，不能用备份ID或全局哈希绕过授权。
    pub async fn authorize(&self, owner: UserId) -> io::Result<()> {
        let owners: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT id FROM users WHERE status='active' LIMIT 2")
                .fetch_all(self.db.pool())
                .await
                .map_err(failure)?;
        if owners != vec![owner.as_bytes().to_vec()] {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unique active owner required",
            ));
        }
        Ok(())
    }
    /// 每次请求都返回当前持久状态，排队不冒充已准备。
    pub async fn status(
        &self,
        owner: UserId,
        id: BackupLeaseId,
    ) -> io::Result<BackupSnapshotStatus> {
        self.authorize(owner).await?;
        let row=sqlx::query("SELECT phase,created_at,expires_at,integrity,manifest_hash,detail FROM backup_preparations WHERE id=? AND owner_id=?")
            .bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).fetch_optional(self.db.pool()).await.map_err(failure)?;
        let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM backup_jobs WHERE backup_id=? AND owner_id=? AND phase IN ('queued','running'))")
            .bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).fetch_one(self.db.pool()).await.map_err(failure)?;
        let (phase, created, expires, integrity, hash, mut detail) = if let Some(r) = row {
            (
                r.try_get::<String, _>("phase").map_err(failure)?,
                r.try_get::<i64, _>("created_at").map_err(failure)?,
                r.try_get::<i64, _>("expires_at").map_err(failure)?,
                r.try_get::<String, _>("integrity").map_err(failure)?,
                r.try_get::<Option<Vec<u8>>, _>("manifest_hash")
                    .map_err(failure)?,
                r.try_get::<Option<String>, _>("detail")
                    .map_err(failure)?
                    .unwrap_or_default(),
            )
        } else {
            let r=sqlx::query("SELECT created_at,phase,detail FROM backup_jobs WHERE backup_id=? AND owner_id=? AND kind='prepare'")
                .bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).fetch_optional(self.db.pool()).await.map_err(failure)?.ok_or_else(||io::Error::new(io::ErrorKind::NotFound,"backup not found"))?;
            let created: i64 = r.try_get("created_at").map_err(failure)?;
            let phase: String = r.try_get("phase").map_err(failure)?;
            (
                if phase == "failed" {
                    "failed"
                } else {
                    "preparing"
                }
                .to_owned(),
                created,
                created + super::LEASE_SECONDS,
                "unverified".to_owned(),
                None,
                r.try_get::<Option<String>, _>("detail")
                    .map_err(failure)?
                    .unwrap_or_default(),
            )
        };
        if detail.is_empty() {
            detail = sqlx::query_scalar::<_, Option<String>>("SELECT detail FROM backup_jobs WHERE backup_id=? AND owner_id=? AND phase='failed' ORDER BY created_at DESC LIMIT 1")
                .bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).fetch_optional(self.db.pool()).await.map_err(failure)?.flatten().unwrap_or_default();
        }
        let phase = match phase.as_str() {
            "preparing" => BackupSnapshotPhase::Preparing,
            "ready" if expires <= crate::uploads::server_now().to_db_integer() => {
                BackupSnapshotPhase::Expired
            }
            "ready" => BackupSnapshotPhase::Ready,
            "failed" => BackupSnapshotPhase::Failed,
            "releasing" => BackupSnapshotPhase::Releasing,
            "released" => BackupSnapshotPhase::Released,
            _ => return Err(failure("invalid backup phase")),
        };
        let integrity = match integrity.as_str() {
            "unverified" => BackupIntegrity::Unverified,
            "verified" => BackupIntegrity::Verified,
            "incomplete" => BackupIntegrity::Incomplete,
            _ => return Err(failure("invalid integrity state")),
        };
        let manifest_hash = hash
            .map(|b| {
                b.try_into()
                    .map(|h: [u8; 32]| {
                        vaultsyncd_server_core::objects::ContentDigest::from_bytes(h).to_hex()
                    })
                    .map_err(|_| failure("invalid manifest hash"))
            })
            .transpose()?;
        Ok(BackupSnapshotStatus {
            backup_id: id.to_hex(),
            phase,
            integrity,
            created_at: created.to_string(),
            expires_at: expires.to_string(),
            detail,
            job_pending: pending,
            manifest_hash,
        })
    }
    /// 有界目录，不返回认证材料或任意服务器路径。
    pub async fn list(&self, owner: UserId, page: u32) -> io::Result<BackupSnapshotList> {
        self.authorize(owner).await?;
        if page > 1_000_000 {
            return Err(invalid("page out of range"));
        }
        let ids:Vec<Vec<u8>>=sqlx::query_scalar("SELECT id FROM (SELECT id FROM backup_preparations WHERE owner_id=? UNION SELECT backup_id AS id FROM backup_jobs WHERE owner_id=?) ORDER BY id LIMIT 101 OFFSET ?")
            .bind(owner.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).bind(i64::from(page)*100).fetch_all(self.db.pool()).await.map_err(failure)?;
        let more = ids.len() > 100;
        let mut items = Vec::new();
        for bytes in ids.into_iter().take(100) {
            let id = BackupLeaseId::from_bytes(
                bytes.try_into().map_err(|_| failure("invalid backup ID"))?,
            );
            items.push(self.status(owner, id).await?);
        }
        Ok(BackupSnapshotList {
            items,
            next_page: more.then(|| (page + 1).to_string()),
        })
    }
    /// 先持久入队再启动；幂等prepare复用同一ID，verify/release失败可以重试同一任务。
    pub async fn submit(
        self: &Arc<Self>,
        owner: UserId,
        id: BackupLeaseId,
        kind: &str,
    ) -> io::Result<BackupSnapshotStatus> {
        self.authorize(owner).await?;
        if !["prepare", "verify", "release"].contains(&kind) {
            return Err(invalid("unknown backup operation"));
        }
        if kind != "prepare" {
            self.status(owner, id).await?;
        }
        let mut tx = self.db.begin_write().await.map_err(failure)?;
        let foreign:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM backup_preparations WHERE id=? AND owner_id<>?) OR EXISTS(SELECT 1 FROM backup_jobs WHERE backup_id=? AND owner_id<>?)")
            .bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).fetch_one(&mut *tx).await.map_err(failure)?;
        if foreign {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup out of scope",
            ));
        }
        sqlx::query("INSERT INTO backup_jobs(backup_id,kind,owner_id,phase,created_at) VALUES(?,?,?,'queued',?) ON CONFLICT(backup_id,kind) DO UPDATE SET phase=CASE WHEN (backup_jobs.kind='verify' AND backup_jobs.phase IN ('failed','succeeded')) OR (backup_jobs.kind='release' AND backup_jobs.phase='failed') THEN 'queued' ELSE backup_jobs.phase END,detail=CASE WHEN (backup_jobs.kind='verify' AND backup_jobs.phase IN ('failed','succeeded')) OR (backup_jobs.kind='release' AND backup_jobs.phase='failed') THEN NULL ELSE backup_jobs.detail END")
            .bind(id.as_bytes().as_slice()).bind(kind).bind(owner.as_bytes().as_slice()).bind(crate::uploads::server_now().to_db_integer()).execute(&mut *tx).await.map_err(failure)?;
        if kind == "verify" {
            sqlx::query("UPDATE backup_preparations SET integrity='unverified',verified_at=NULL,detail=NULL WHERE id=? AND owner_id=? AND phase='ready' AND EXISTS(SELECT 1 FROM backup_jobs WHERE backup_id=? AND kind='verify' AND phase='queued')")
                .bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).bind(id.as_bytes().as_slice()).execute(&mut *tx).await.map_err(failure)?;
        }
        tx.commit().await.map_err(failure)?;
        self.launch(owner, id, kind.to_owned()).await?;
        self.status(owner, id).await
    }
    async fn launch(
        self: &Arc<Self>,
        owner: UserId,
        id: BackupLeaseId,
        kind: String,
    ) -> io::Result<()> {
        let mut running = self.running.lock().await;
        let phase: Option<String> = sqlx::query_scalar(
            "SELECT phase FROM backup_jobs WHERE backup_id=? AND kind=? AND owner_id=?",
        )
        .bind(id.as_bytes().as_slice())
        .bind(&kind)
        .bind(owner.as_bytes().as_slice())
        .fetch_optional(self.db.pool())
        .await
        .map_err(failure)?;
        if !phase.is_some_and(|p| p == "queued" || p == "running")
            || !running.insert((id, kind.clone()))
        {
            return Ok(());
        }
        let context = Arc::clone(self);
        let task = tokio::spawn(async move {
            let result = context.execute(owner, id, &kind).await;
            if let Err(error) = &result {
                tracing::warn!(backup=%id,operation=%kind,error=%error,"备份作业失败，保留持久证据");
            }
            let persist=async{
                let mut tx=context.db.begin_write().await.map_err(failure)?;
                sqlx::query("UPDATE backup_jobs SET phase=?,detail=? WHERE backup_id=? AND kind=? AND owner_id=?")
                    .bind(if result.is_ok(){"succeeded"}else{"failed"}).bind(result.as_ref().err().map(ToString::to_string)).bind(id.as_bytes().as_slice()).bind(&kind).bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.map_err(failure)?;tx.commit().await.map_err(failure)
            }.await;
            if let Err(error) = persist {
                tracing::error!(backup=%id,error=%error,"备份作业状态未能持久化，重启必须重新核对");
            }
            context.running.lock().await.remove(&(id, kind));
        });
        self.jobs.lock().await.push(task);
        Ok(())
    }
    async fn execute(
        self: &Arc<Self>,
        owner: UserId,
        id: BackupLeaseId,
        kind: &str,
    ) -> io::Result<()> {
        let _guard = self.lock(id).await.write_owned().await;
        self.authorize(owner).await?;
        let mut tx = self.db.begin_write().await.map_err(failure)?;
        sqlx::query(
            "UPDATE backup_jobs SET phase='running' WHERE backup_id=? AND kind=? AND owner_id=?",
        )
        .bind(id.as_bytes().as_slice())
        .bind(kind)
        .bind(owner.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(failure)?;
        tx.commit().await.map_err(failure)?;
        match kind {
            "prepare" => {
                let phase: Option<String> = sqlx::query_scalar(
                    "SELECT phase FROM backup_preparations WHERE id=? AND owner_id=?",
                )
                .bind(id.as_bytes().as_slice())
                .bind(owner.as_bytes().as_slice())
                .fetch_optional(self.db.pool())
                .await
                .map_err(failure)?;
                match phase.as_deref() {
                    Some("ready") => Ok(()),
                    Some(_) => Err(failure(
                        "previous preparation did not complete; use a new ID",
                    )),
                    None => super::prepare(
                        &self.db,
                        &self.config,
                        owner,
                        id,
                        crate::uploads::server_now(),
                    )
                    .await
                    .map(|_| ()),
                }
            }
            "verify" => {
                let result = self.verify_source(owner, id).await;
                if let Err(error) = &result {
                    self.incomplete(owner, id, &error.to_string()).await?;
                }
                result
            }
            "release" => self.release(owner, id).await,
            _ => Err(invalid("unknown job")),
        }
    }
    async fn verify_source(&self, owner: UserId, id: BackupLeaseId) -> io::Result<()> {
        use tokio::io::AsyncReadExt;
        let view = self.view(owner, id).await?;
        for item in &view.index.items {
            let opened = view
                .directory
                .open_read(Path::new(&item.path))
                .map_err(failure)?;
            let mut file = tokio::fs::File::from_std(opened.file);
            let mut check = Integrity::new(item.clone());
            let mut chunk = vec![0; CHUNK];
            loop {
                let n = file.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                check.push(&chunk[..n]).map_err(failure)?;
            }
            check.finish().map_err(failure)?;
        }
        let directory = Arc::clone(&view.directory);
        let index = view.index.clone();
        tokio::task::spawn_blocking(move || {
            vaultsyncd_server_store::backup::verify_database(
                &directory.proc_fd_path().join("database.sqlite"),
                &index,
            )
        })
        .await
        .map_err(failure)??;
        let mut tx = self.db.begin_write().await.map_err(failure)?;
        let changed=sqlx::query("UPDATE backup_preparations SET integrity='verified',verified_at=?,detail=NULL WHERE id=? AND owner_id=? AND phase='ready'")
            .bind(crate::uploads::server_now().to_db_integer()).bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.map_err(failure)?.rows_affected();
        if changed != 1 {
            return Err(failure("backup no longer ready"));
        }
        tx.commit().await.map_err(failure)
    }
    pub(crate) async fn incomplete(
        &self,
        owner: UserId,
        id: BackupLeaseId,
        detail: &str,
    ) -> io::Result<()> {
        let mut tx = self.db.begin_write().await.map_err(failure)?;
        sqlx::query("UPDATE backup_preparations SET integrity='incomplete',verified_at=NULL,detail=? WHERE id=? AND owner_id=? AND phase='ready'").bind(detail).bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.map_err(failure)?;
        tx.commit().await.map_err(failure)
    }
    async fn release(&self, owner: UserId, id: BackupLeaseId) -> io::Result<()> {
        let state = self.status(owner, id).await?;
        if state.phase == BackupSnapshotPhase::Released {
            return Ok(());
        }
        let _objects = self.db.object_maintenance().await;
        let mut tx = self.db.begin_write().await.map_err(failure)?;
        let changed = sqlx::query(
            "UPDATE backup_preparations SET phase='releasing' WHERE id=? AND owner_id=?",
        )
        .bind(id.as_bytes().as_slice())
        .bind(owner.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .map_err(failure)?
        .rows_affected();
        if changed != 1 {
            return Err(failure("backup has not been prepared"));
        }
        tx.commit().await.map_err(failure)?;
        Arena::open(&self.config.state_dir.join("backups"))?.remove(&id.to_hex())?;
        let mut tx = self.db.begin_write().await.map_err(failure)?;
        leases::release_backup_lease(&mut tx, id)
            .await
            .map_err(failure)?;
        sqlx::query("UPDATE backup_preparations SET phase='released',cleaned_at=? WHERE id=? AND owner_id=?").bind(crate::uploads::server_now().to_db_integer()).bind(id.as_bytes().as_slice()).bind(owner.as_bytes().as_slice()).execute(&mut *tx).await.map_err(failure)?;
        tx.commit().await.map_err(failure)
    }
    pub(crate) async fn view(&self, owner: UserId, id: BackupLeaseId) -> io::Result<View> {
        use std::io::Read;
        let state = self.status(owner, id).await?;
        if state.phase != BackupSnapshotPhase::Ready {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "backup source is not available",
            ));
        }
        let expected = vaultsyncd_server_core::objects::ContentDigest::from_hex(
            state
                .manifest_hash
                .as_deref()
                .ok_or_else(|| failure("manifest missing"))?,
        )
        .map_err(failure)?;
        let arena = Arena::open(&self.config.state_dir.join("backups"))?;
        if !arena.is_read_only(&id.to_hex())? {
            return Err(failure("backup snapshot is no longer read-only"));
        }
        let directory = Arc::new(
            RestrictedDir::open(&arena.snapshot_path(&id.to_hex())?, ResolveMode::Auto)
                .map_err(failure)?,
        );
        let file = directory
            .open_read(Path::new("manifest.json"))
            .map_err(failure)?;
        let mut manifest = Vec::new();
        file.file
            .take((MAX_MANIFEST + 1) as u64)
            .read_to_end(&mut manifest)?;
        let index = Index::parse(&manifest, *expected.as_bytes()).map_err(failure)?;
        if index.id != id || digest(&manifest) != *expected.as_bytes() {
            return Err(failure("manifest identity mismatch"));
        }
        Ok(View {
            index,
            manifest,
            directory,
        })
    }
    /// 启动重放作业，释放中断不会被当成released；过期ready按释放作业收尾。
    pub async fn resume(self: &Arc<Self>) -> io::Result<()> {
        let rows = sqlx::query(
            "SELECT backup_id,owner_id,kind FROM backup_jobs WHERE phase IN ('queued','running')",
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(failure)?;
        for r in rows {
            let id = BackupLeaseId::from_bytes(
                r.try_get::<Vec<u8>, _>("backup_id")
                    .map_err(failure)?
                    .try_into()
                    .map_err(|_| failure("invalid ID"))?,
            );
            let owner = UserId::from_bytes(
                r.try_get::<Vec<u8>, _>("owner_id")
                    .map_err(failure)?
                    .try_into()
                    .map_err(|_| failure("invalid owner"))?,
            );
            self.launch(owner, id, r.try_get("kind").map_err(failure)?)
                .await?;
        }
        let rows=sqlx::query("SELECT id,owner_id FROM backup_preparations WHERE phase='releasing' OR (phase='ready' AND expires_at<=?)").bind(crate::uploads::server_now().to_db_integer()).fetch_all(self.db.pool()).await.map_err(failure)?;
        for r in rows {
            let id = BackupLeaseId::from_bytes(
                r.try_get::<Vec<u8>, _>("id")
                    .map_err(failure)?
                    .try_into()
                    .map_err(|_| failure("invalid ID"))?,
            );
            let owner = UserId::from_bytes(
                r.try_get::<Vec<u8>, _>("owner_id")
                    .map_err(failure)?
                    .try_into()
                    .map_err(|_| failure("invalid owner"))?,
            );
            self.submit(owner, id, "release").await?;
        }
        Ok(())
    }
    /// 仅重放已持久接受的作业并收尾过期租约，不创建定期备份。
    /// 独立停止句柄由入口持有，停止接收后先结束pump，再等待已接受作业。
    pub fn pump(self: &Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) -> JoinHandle<()> {
        let context = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    biased;
                    _ = stop.changed() => break,
                    _ = interval.tick() => {
                        if *stop.borrow() { break; }
                        if let Err(error) = context.resume().await {
                            tracing::warn!(error=%error,"备份作业重放失败，持久队列保留待重试");
                        }
                    }
                }
            }
        })
    }
    /// 停止监听并结束现有导出连接后等待已接受作业；不会因socket请求超时中止它们。
    pub async fn wait(&self) {
        for task in std::mem::take(&mut *self.jobs.lock().await) {
            if let Err(error) = task.await {
                tracing::error!(error=%error,"备份后台作业异常结束，持久状态待重启核对");
            }
        }
    }
}
