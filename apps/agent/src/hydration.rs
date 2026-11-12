//! Nonblocking CFAPI callbacks and bounded, dedicated worker runtime. No SQLite transaction
//! or user interface thread spans network I/O, file hashing, or a CFAPI reply.
use crate::bootstrap::{HydrationConfig, PinnedVersion, progress};
use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use vaultsyncd_agent_core::engine::scheduler::{UserReadGuard, UserReads};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::{BindingId, EntryId, OpId, VaultId};
use vaultsyncd_platform_windows::{
    cfapi::{
        adapter::{Callbacks, ReadDemand, Reply},
        catalog::Identity,
        raw::*,
    },
    files::staging::StagingFile,
};
use vaultsyncd_transfer::{
    concurrency::CancelToken,
    download,
    ports::{LocalErrorKind, PortError, PortFuture, TempContent, VersionPin, VersionedContent},
};

const CANCELLED: i32 = 0xc0000120u32 as i32;
const RESOURCES: i32 = 0xc000009au32 as i32;
const CORRUPT: i32 = 0xc000003fu32 as i32;
// A cloud I/O can request multiple ranges before the preceding range worker has retired.
// Routing keys identify the OS operation; the range identifies one bounded work item.
type Key = (i64, i64, i64, u64, u64);
fn key(demand: ReadDemand) -> Key {
    (
        demand.reply.connection,
        demand.reply.transfer,
        demand.reply.request,
        demand.offset,
        demand.length,
    )
}
struct Active {
    identity: Identity,
    cancel: CancelToken,
}
#[derive(Default)]
struct Shared {
    closed: AtomicBool,
    active: Mutex<HashMap<Key, Active>>,
    fetches: AtomicU64,
    served: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    cancel_callbacks: AtomicU64,
    bytes: AtomicU64,
}
/// Cumulative observations. A download completion is never synchronization completion.
#[derive(Clone, Copy, Debug)]
pub struct HydrationStats {
    /// Accepted native requests.
    pub fetches: u64,
    /// Successfully returned verified ranges.
    pub served: u64,
    /// Explicit request failures.
    pub failed: u64,
    /// Requests stopped by cancellation.
    pub cancelled: u64,
    /// Actual OS CANCEL_FETCH_DATA invocations, separate from explicit application cancellation.
    pub cancel_callbacks: u64,
    /// Verified bytes successfully acknowledged by CFAPI.
    pub bytes: u64,
    /// Accepted requests still queued or running.
    pub active: usize,
}
/// Clone through Arc into Adapter. fetch/cancel only touch a bounded in-memory queue/map.
/// Drop/disconnect cancels pending work; wait_idle allows orderly shutdown before teardown.
pub struct HydrationCallbacks {
    sender: mpsc::Sender<ReadDemand>,
    shared: Arc<Shared>,
    limit: usize,
    pins: Arc<RwLock<HashMap<[u8; 72], VersionPin>>>,
    verified: Arc<Mutex<HashMap<[u8; 72], Arc<StagingFile>>>>,
    binding: BindingId,
    vault: VaultId,
}
impl HydrationCallbacks {
    pub(crate) fn start(
        db: Arc<Database>,
        content: Arc<dyn VersionedContent>,
        reads: UserReads,
        pins: HashMap<[u8; 72], VersionPin>,
        owner: (BindingId, VaultId),
        config: HydrationConfig,
    ) -> io::Result<Arc<Self>> {
        let limit = config.outstanding;
        let pins = Arc::new(RwLock::new(pins));
        let worker_pins = pins.clone();
        let verified = Arc::new(Mutex::new(HashMap::new()));
        let worker_verified = verified.clone();
        let (sender, mut receiver) = mpsc::channel::<ReadDemand>(config.outstanding);
        let shared = Arc::new(Shared::default());
        let state = shared.clone();
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new().name("vaultsyncd-hydration".into()).spawn(move||{
            let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
            match runtime {Err(error)=>{let _=started.send(Err(error));},Ok(runtime)=>{
                let _=started.send(Ok(()));
                runtime.block_on(async move {
                    let worker=Arc::new(Worker{db,content,reads,pins:worker_pins,config,shared:state.clone(),verified:worker_verified});
                    let mut tasks=tokio::task::JoinSet::new();
                    loop {
                        tokio::select! {
                            next=receiver.recv()=>match next {Some(demand)=>{let worker=worker.clone();tasks.spawn(async move {worker.run(demand).await;});},None=>break},
                            _=tasks.join_next(),if !tasks.is_empty()=>{}
                        }
                    }
                    while tasks.join_next().await.is_some() {}
                });
            }}
        })?;
        ready.recv().map_err(io::Error::other)??;
        Ok(Arc::new(Self {
            sender,
            shared,
            limit,
            pins,
            verified,
            binding: owner.0,
            vault: owner.1,
        }))
    }
    /// Add immutable metadata before publishing a new placeholder identity to its Catalog.
    /// A running request retains its own pin even when another head is installed.
    pub fn register_version(&self, version: PinnedVersion) -> io::Result<()> {
        PinnedVersion::new(version.identity, version.pin.clone())?;
        if version.identity.binding != self.binding || version.identity.vault != self.vault {
            return Err(io::Error::other("foreign fixed version"));
        }
        let mut pins = self
            .pins
            .write()
            .map_err(|_| io::Error::other("fixed versions poisoned"))?;
        let key = version.identity.encode();
        if pins.get(&key).is_some_and(|pin| *pin != version.pin) {
            return Err(io::Error::other("immutable version facts changed"));
        }
        pins.insert(key, version.pin);
        Ok(())
    }
    /// Close idle remote staging leases before recovery scans; keep every cached byte.
    /// Active readers retain their own Arc, so a race can only make the scan refuse.
    pub fn close_idle_staging(&self) -> io::Result<()> {
        if !self
            .shared
            .active
            .lock()
            .map_err(|_| io::Error::other("hydration requests poisoned"))?
            .is_empty()
        {
            return Err(io::Error::other("recovery scan requires idle hydration"));
        }
        self.verified
            .lock()
            .map_err(|_| io::Error::other("verified staging poisoned"))?
            .clear();
        Ok(())
    }
    /// Current counters. No file or database reads on the caller thread.
    pub fn stats(&self) -> HydrationStats {
        let s = &self.shared;
        HydrationStats {
            fetches: s.fetches.load(Ordering::SeqCst),
            served: s.served.load(Ordering::SeqCst),
            failed: s.failed.load(Ordering::SeqCst),
            cancelled: s.cancelled.load(Ordering::SeqCst),
            cancel_callbacks: s.cancel_callbacks.load(Ordering::SeqCst),
            bytes: s.bytes.load(Ordering::SeqCst),
            active: s.active.lock().expect("hydration requests").len(),
        }
    }
    /// Cancel every current range for the specified entry, preserving recoverable blocks.
    pub fn cancel_entry(&self, binding: BindingId, entry: EntryId) {
        for active in self
            .shared
            .active
            .lock()
            .expect("hydration requests")
            .values()
        {
            if active.identity.binding == binding && active.identity.entry == entry {
                active.cancel.cancel();
            }
        }
    }
    /// Bounded drain; callers disconnect before awaiting this at shutdown.
    pub async fn wait_idle(&self, timeout: Duration) -> io::Result<()> {
        let start = Instant::now();
        while self.stats().active != 0 {
            if start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "hydration drain timeout",
                ));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    }
}
impl Callbacks for HydrationCallbacks {
    fn fetch(&self, demand: ReadDemand) -> Result<(), i32> {
        let mut active = self.shared.active.lock().map_err(|_| RESOURCES)?;
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE);
        }
        if active.len() >= self.limit || active.contains_key(&key(demand)) {
            return Err(RESOURCES);
        }
        active.insert(
            key(demand),
            Active {
                identity: demand.identity,
                cancel: CancelToken::new(),
            },
        );
        if self.sender.try_send(demand).is_err() {
            active.remove(&key(demand));
            return Err(RESOURCES);
        }
        self.shared.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn cancel(&self, reply: Reply) {
        self.shared.cancel_callbacks.fetch_add(1, Ordering::SeqCst);
        for (key, active) in self
            .shared
            .active
            .lock()
            .expect("hydration requests")
            .iter()
        {
            if key.0 == reply.connection && key.1 == reply.transfer {
                active.cancel.cancel();
            }
        }
    }
    fn disconnect(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        for active in self
            .shared
            .active
            .lock()
            .expect("hydration requests")
            .values()
        {
            active.cancel.cancel();
        }
        // Do not leave exclusive staging handles alive while the worker runtime retires.
        // In-flight requests own separate references and must still reach wait_idle.
        self.verified.lock().expect("verified staging").clear();
    }
}
impl Drop for HydrationCallbacks {
    fn drop(&mut self) {
        self.disconnect();
    }
}
struct Worker {
    db: Arc<Database>,
    content: Arc<dyn VersionedContent>,
    reads: UserReads,
    pins: Arc<RwLock<HashMap<[u8; 72], VersionPin>>>,
    config: HydrationConfig,
    verified: Arc<Mutex<HashMap<[u8; 72], Arc<StagingFile>>>>,
    shared: Arc<Shared>,
}
impl Worker {
    async fn run(&self, demand: ReadDemand) {
        let cancel = {
            self.shared
                .active
                .lock()
                .expect("hydration requests")
                .get(&key(demand))
                .expect("accepted request")
                .cancel
                .clone()
        };
        let work = self.perform(demand, &cancel);
        tokio::pin!(work);
        let result = tokio::select! {
            result=&mut work=>result,
            _=tokio::time::sleep(self.config.timeout)=>{cancel.cancel();let _=work.await;Err(STATUS_IO_TIMEOUT)},
        };
        match result {
            Ok(()) => {
                self.shared.served.fetch_add(1, Ordering::SeqCst);
            }
            Err(status) => {
                #[cfg(test)]
                eprintln!(
                    "hydration failed status={:08x} offset={} length={}",
                    status as u32, demand.offset, demand.length
                );
                cancel.cancel();
                if status == CANCELLED {
                    self.shared.cancelled.fetch_add(1, Ordering::SeqCst);
                } else {
                    self.shared.failed.fetch_add(1, Ordering::SeqCst);
                }
                // Failure returns status only, never fabricated/partial bytes. It may fail after disconnect.
                let _ =
                    demand
                        .reply
                        .transfer(demand.offset as i64, demand.length as i64, None, status);
            }
        }
        self.shared
            .active
            .lock()
            .expect("hydration requests")
            .remove(&key(demand));
    }
    async fn admit(&self, demand: ReadDemand, cancel: &CancelToken) -> Result<UserReadGuard, i32> {
        let request = OpId::from_bytes(
            vaultsyncd_transfer::hash::digest_of(
                format!(
                    "{}:{}:{}",
                    demand.reply.connection, demand.reply.transfer, demand.reply.request
                )
                .as_bytes(),
            )[..16]
                .try_into()
                .expect("digest"),
        );
        loop {
            if cancel.is_cancelled() {
                return Err(CANCELLED);
            }
            match self
                .reads
                .try_acquire(request, demand.identity.binding, demand.identity.entry)
                .await
                .map_err(port_status)?
            {
                Some(guard) => return Ok(guard),
                None => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    }
    async fn perform(&self, demand: ReadDemand, cancel: &CancelToken) -> Result<(), i32> {
        #[cfg(test)]
        eprintln!(
            "hydration demand offset={} length={}",
            demand.offset, demand.length
        );
        let pin = self
            .pins
            .read()
            .map_err(|_| CORRUPT)?
            .get(&demand.identity.encode())
            .cloned()
            .ok_or(STATUS_CLOUD_FILE_INVALID_REQUEST)?;
        if pin.size != demand.file_size
            || demand.length == 0
            || demand
                .offset
                .checked_add(demand.length)
                .is_none_or(|end| end > pin.size)
        {
            return Err(STATUS_CLOUD_FILE_INVALID_REQUEST);
        }
        let _guard = self.admit(demand, cancel).await?;
        if cancel.is_cancelled() {
            return Err(CANCELLED);
        }
        // Short SQL statement, completed before opening/downloading. Do not infer clean or committed.
        sqlx::query("UPDATE local_entries SET residency='partial' WHERE binding_id=?1 AND entry_id=?2 AND residency='metadata_only'")
            .bind(demand.identity.binding.as_bytes().as_slice()).bind(demand.identity.entry.as_bytes().as_slice()).execute(self.db.pool()).await.map_err(|_|CORRUPT)?;
        let cached = self
            .verified
            .lock()
            .map_err(|_| CORRUPT)?
            .get(&demand.identity.encode())
            .cloned();
        let temp = if let Some(temp) = cached {
            temp
        } else {
            let name = format!("{}-{}-{}.part", pin.binding, pin.entry, pin.version);
            let temp = Arc::new(
                StagingFile::open(&self.config.staging, &name).map_err(|_e| {
                    #[cfg(test)]
                    eprintln!("staging open failed: {_e}");
                    RESOURCES
                })?,
            );
            let options = self.config.download.clone().with_cancel(cancel.clone());
            download::run(
                &CancelableContent {
                    content: &*self.content,
                    cancel,
                },
                &*temp,
                &*progress(self.db.clone()),
                &pin,
                &options,
            )
            .await
            .map_err(|error| {
                #[cfg(test)]
                eprintln!("download failed {error:?}");
                match error {
                    download::DownloadError::Cancelled => CANCELLED,
                    download::DownloadError::Port(e) => port_status(e),
                    download::DownloadError::NoSpace { .. } => 0xc000007fu32 as i32,
                    _ => CORRUPT,
                }
            })?;

            let mut verified = self.verified.lock().map_err(|_| CORRUPT)?;
            // Serialize with disconnect's clear: a finishing download cannot repin
            // its exclusive staging handle after maintenance has closed admission.
            if cancel.is_cancelled() || self.shared.closed.load(Ordering::SeqCst) {
                return Err(CANCELLED);
            }
            if verified.len() >= 16 {
                if let Some(key) = verified.keys().next().copied() {
                    verified.remove(&key);
                }
            }
            verified.insert(demand.identity.encode(), temp.clone());
            temp
        };
        // Whole-file digest has passed. Range output remains bounded and cancellation-safe.
        let mut offset = demand.offset;
        let end = offset + demand.length;
        while offset < end {
            if cancel.is_cancelled() || self.shared.closed.load(Ordering::SeqCst) {
                return Err(CANCELLED);
            }
            let length = (end - offset).min(64 * 1024);
            let bytes = temp.read_at(offset, length).await.map_err(port_status)?;
            demand
                .reply
                .transfer(offset as i64, length as i64, Some(&bytes), 0)
                .map_err(|_e| {
                    #[cfg(test)]
                    eprintln!("CfExecute failed: {_e}");
                    STATUS_CLOUD_FILE_INVALID_REQUEST
                })?;
            self.shared.bytes.fetch_add(length, Ordering::SeqCst);
            offset += length;
        }
        Ok(())
    }
}
struct CancelableContent<'a> {
    content: &'a dyn VersionedContent,
    cancel: &'a CancelToken,
}
impl VersionedContent for CancelableContent<'_> {
    fn read_range<'a>(
        &'a self,
        pin: &'a VersionPin,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            tokio::select! {result=self.content.read_range(pin,offset,length)=>result,_=cancelled(self.cancel)=>Err(PortError::Cancelled)}
        })
    }
}
async fn cancelled(token: &CancelToken) {
    while !token.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
fn port_status(error: PortError) -> i32 {
    match error {
        PortError::Cancelled => CANCELLED,
        PortError::Unavailable { .. } => STATUS_CLOUD_FILE_NETWORK_UNAVAILABLE,
        PortError::Local {
            kind: LocalErrorKind::NoSpace,
            ..
        } => 0xc000007fu32 as i32,
        PortError::ReauthRequired { .. } => 0xc0000022u32 as i32,
        PortError::Rejected { .. } => 0xc0000022u32 as i32,
        PortError::Protocol { .. } => CORRUPT,
        PortError::Local { .. } => 0xc0000185u32 as i32,
    }
}
