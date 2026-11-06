//! Concrete HTTP/store/scheduler/CFAPI assembly. Application startup must finish recovery
//! and load immutable version facts before connecting; this module does not infer a head.
pub use crate::hydration::{HydrationCallbacks, HydrationStats};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use vaultsyncd_agent_core::{adapters::ClientStorePorts, engine::scheduler::UserReads};
use vaultsyncd_client_store::db::Database;
use vaultsyncd_domain::ids::{BindingId, EntryId};
use vaultsyncd_http_client::{ApiClient, ReqwestTransport, content::ContentEndpoint};
use vaultsyncd_platform_windows::{
    cfapi::{
        adapter::{Adapter, Callbacks},
        catalog::{Catalog, Identity},
    },
    files::RootHandle,
};
use vaultsyncd_transfer::{download::DownloadOptions, ports::VersionPin};

/// Authenticated immutable metadata loaded from a server snapshot/event or durable journal.
/// All four domain IDs are mandatory; callback size is only a consistency check.
#[derive(Clone, Debug)]
pub struct PinnedVersion {
    /// Domain identity including exact version.
    pub identity: Identity,
    /// Expected SHA-256/size from immutable version facts.
    pub pin: VersionPin,
}
impl PinnedVersion {
    /// Reject mismatched identities before admitting any CFAPI demand.
    pub fn new(identity: Identity, pin: VersionPin) -> io::Result<Self> {
        if pin.binding != identity.binding.to_string()
            || pin.entry != identity.entry.to_string()
            || pin.version != identity.version.to_string()
        {
            return Err(io::Error::other("fixed version identity mismatch"));
        }
        Ok(Self { identity, pin })
    }
}
/// One binding's live hydration configuration. Reuse the background scheduler's admission
/// handle. The staging directory must already exist and must be outside every sync root.
pub struct HydrationConfig {
    /// Safe internal staging directory, separate from user files.
    pub staging: PathBuf,
    /// Fixed immutable version metadata, including historical pins still used by placeholders.
    pub versions: Vec<PinnedVersion>,
    /// Download block geometry/concurrency; cancellation is supplied by each OS request.
    pub download: DownloadOptions,
    /// Bound for a whole OS request including admission, network, validation and reply.
    pub timeout: Duration,
    /// Maximum outstanding OS requests; a full queue returns an explicit resource error.
    pub outstanding: usize,
}
/// Build production HTTP/download/SQLite wiring on a dedicated worker runtime, then connect.
/// The returned callbacks must be retained for cancel/idle/drain during shutdown.
pub fn connect_hydration(
    root: &Path,
    catalog: Catalog,
    db: Arc<Database>,
    client: ApiClient<ReqwestTransport>,
    reads: UserReads,
    config: HydrationConfig,
) -> io::Result<(Adapter, Arc<HydrationCallbacks>)> {
    if config.timeout.is_zero() || config.outstanding == 0 || config.outstanding > 1024 {
        return Err(io::Error::other("invalid hydration bounds"));
    }
    let sync = RootHandle::open(root)?;
    let staging = RootHandle::open(&config.staging)?;
    if sync.overlaps(&staging) {
        return Err(io::Error::other("staging overlaps synchronization root"));
    }
    let mut pins = HashMap::new();
    for version in &config.versions {
        PinnedVersion::new(version.identity, version.pin.clone())?;
        if version.identity.binding != catalog.binding || version.identity.root != catalog.root {
            return Err(io::Error::other("foreign fixed version"));
        }
        let key = version.identity.encode();
        if pins
            .insert(key, version.pin.clone())
            .is_some_and(|prior| prior != version.pin)
        {
            return Err(io::Error::other("immutable version facts changed"));
        }
    }
    let content =
        ContentEndpoint::new(client, catalog.root.to_string()).map_err(io::Error::other)?;
    let callbacks = HydrationCallbacks::start(
        db,
        Arc::new(content),
        reads,
        pins,
        (catalog.binding, catalog.root),
        config,
    )?;
    let port: Arc<dyn Callbacks> = callbacks.clone();
    let adapter = Adapter::connect(root, catalog, port)?;
    Ok((adapter, callbacks))
}
/// Cancel active OS requests for an entry, for example when a user cancels a download.
/// It preserves stage bytes/bitmap and never dequeues synchronization work.
pub fn cancel_hydration(callbacks: &HydrationCallbacks, binding: BindingId, entry: EntryId) {
    callbacks.cancel_entry(binding, entry);
}

pub(crate) fn progress(db: Arc<Database>) -> Arc<ClientStorePorts> {
    Arc::new(ClientStorePorts::new(db))
}
