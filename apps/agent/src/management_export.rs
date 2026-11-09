//! Native recovery export. Pinned ancestry and no-write/no-delete file handles protect both ends.
use crate::{
    config::AgentConfig,
    local::{Local, error},
    work::{local_metadata, paths},
};
use std::{
    collections::HashMap,
    fs::File,
    io::Write,
    os::windows::fs::FileExt,
    path::Path,
    sync::{Arc, Mutex},
};
use vaultsyncd_agent_core::recovery::export::{
    ExportDestination, ExportDirectory, ExportRecovery, ExportSource, Target,
};
use vaultsyncd_domain::ids::BindingId;
use vaultsyncd_platform_windows::files::VaultHandle;
use vaultsyncd_transfer::{
    hash::{ContentHasher, digest_of, to_hex},
    ports::{PortError, PortFuture, SnapshotSource},
};

struct Fixed {
    file: File,
    identity: String,
    size: u64,
}
impl SnapshotSource for Fixed {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn total_size(&self) -> u64 {
        self.size
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            if length > 256 * 1024 || offset.checked_add(length).is_none() {
                return Err(error("invalid export read"));
            }
            let n = length.min(self.size.saturating_sub(offset));
            let mut bytes = vec![0; n as usize];
            let mut done = 0;
            while done < bytes.len() {
                let n = self
                    .file
                    .seek_read(&mut bytes[done..], offset + done as u64)
                    .map_err(error)?;
                if n == 0 {
                    return Err(error("export source short read"));
                }
                done += n;
            }
            Ok(bytes)
        })
    }
}
struct Shared(Arc<Fixed>);
impl SnapshotSource for Shared {
    fn identity(&self) -> &str {
        self.0.identity()
    }
    fn total_size(&self) -> u64 {
        self.0.total_size()
    }
    fn read(&self, o: u64, n: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        self.0.read(o, n)
    }
}
struct Source {
    local: Arc<Local>,
    paths: HashMap<String, String>,
    cache: Arc<VaultHandle>,
    files: Mutex<HashMap<(String, String), Arc<Fixed>>>,
}
impl Source {
    fn fixed(vault: &VaultHandle, path: &str, identity: String) -> Result<Arc<Fixed>, PortError> {
        let meta = vault.inspect(path).map_err(error)?;
        if meta.directory || meta.cloud.as_ref().is_some_and(|c| c.on_disk < meta.size) {
            return Err(error("incomplete bytes cannot be exported as complete"));
        }
        let file = vault.open_read(path).map_err(error)?;
        if file.metadata().map_err(error)?.len() != meta.size {
            return Err(error("source changed"));
        }
        Ok(Arc::new(Fixed {
            file,
            identity,
            size: meta.size,
        }))
    }
    async fn digest(file: &Fixed) -> Result<String, PortError> {
        let mut hash = ContentHasher::new();
        let mut offset = 0;
        while offset < file.size {
            let n = (file.size - offset).min(256 * 1024);
            hash.update(&file.read(offset, n).await?);
            offset += n;
        }
        Ok(to_hex(&hash.finish()))
    }
}
impl ExportSource for Source {
    fn discover(&self, binding: BindingId) -> PortFuture<'_, Result<Vec<Target>, PortError>> {
        Box::pin(async move {
            if binding != self.local.binding {
                return Err(error("export binding mismatch"));
            }
            let local = &self.local;
            let ids:Vec<String>=sqlx::query_scalar("SELECT lower(hex(entry_id)) FROM local_entries WHERE binding_id=? AND (residency!='metadata_only' OR dirty!='clean')")
            .bind(binding.as_bytes().as_slice()).fetch_all(local.db.pool()).await.map_err(error)?;
            let known: Vec<&String> = ids.iter().filter_map(|id| self.paths.get(id)).collect();
            let cloud_ids: Vec<String> = sqlx::query_scalar("SELECT lower(hex(entry_id)) FROM local_entries WHERE binding_id=? AND residency='metadata_only' AND dirty='clean'")
                .bind(binding.as_bytes().as_slice()).fetch_all(local.db.pool()).await.map_err(error)?;
            let versions: Vec<(String,String)> = sqlx::query_as("SELECT lower(hex(entry_id)),lower(hex(version_id)) FROM agent_versions WHERE binding_id=?")
                .bind(binding.as_bytes().as_slice()).fetch_all(local.db.pool()).await.map_err(error)?;
            let reconstructible: Vec<String> = versions
                .into_iter()
                .map(|(entry, version)| format!("{binding}-{entry}-{version}.part"))
                .collect();
            let mut result = vec![];
            for (kind, vault) in [("local", &*local.vault), ("cache", &*self.cache)] {
                let report = vault.scan();
                if !report.permits_missing_comparison() {
                    return Err(error("incomplete recovery scan"));
                }
                for entry in report.entries {
                    let meta = entry
                        .metadata
                        .ok_or_else(|| error("unknown export metadata"))?;
                    if meta.directory || (kind == "local" && known.contains(&&entry.path)) {
                        continue;
                    }
                    if kind == "cache" && reconstructible.contains(&entry.path) {
                        // This live staging file contains only a fixed, recoverable remote download.
                        continue;
                    }
                    if kind == "local"
                        && let Some(cloud) = meta.cloud.as_ref().filter(|c| c.on_disk < meta.size)
                    {
                        let identity =
                            vaultsyncd_platform_windows::cfapi::catalog::Identity::decode(
                                &cloud.identity,
                            )
                            .map_err(error)?;
                        if cloud.modified != 0
                            || identity.binding != binding
                            || !cloud_ids.iter().any(|id| {
                                id == &identity.entry.to_string()
                                    && self.paths.get(id) == Some(&entry.path)
                            })
                        {
                            return Err(error(
                                "unconfirmed or unknown partial content cannot be omitted",
                            ));
                        }
                        continue;
                    }
                    let id = to_hex(&digest_of(format!("{kind}:{}", entry.path).as_bytes()))[..32]
                        .to_owned();
                    let identity = format!(
                        "{}:{:02x?}:{}",
                        meta.identity.volume, meta.identity.file, meta.size
                    );
                    let key = ("candidate".into(), id.clone());
                    if !self.files.lock().map_err(error)?.contains_key(&key) {
                        let fixed = Self::fixed(vault, &entry.path, identity.clone())?;
                        self.files.lock().map_err(error)?.insert(key, fixed);
                    }
                    result.push(Target {
                        id,
                        kind: "candidate".into(),
                        identity,
                        digest: None,
                        size: Some(meta.size),
                    });
                }
            }
            result.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(result)
        })
    }
    fn open<'a>(
        &'a self,
        target: &'a Target,
    ) -> PortFuture<'a, Result<Box<dyn SnapshotSource>, PortError>> {
        Box::pin(async move {
            let key = (target.kind.clone(), target.id.clone());
            if let Some(file) = self.files.lock().map_err(error)?.get(&key).cloned() {
                return Ok(Box::new(Shared(file)) as Box<dyn SnapshotSource>);
            }
            let fixed = if target.kind == "current" {
                let path = self
                    .paths
                    .get(&target.id)
                    .ok_or_else(|| error("current path unknown; export cannot omit bytes"))?;
                Self::fixed(&self.local.vault, path, target.identity.clone())?
            } else if target.kind == "snapshot" {
                let expected = target
                    .digest
                    .as_ref()
                    .ok_or_else(|| error("fixed snapshot digest missing"))?;
                let files: Vec<Arc<Fixed>> = self
                    .files
                    .lock()
                    .map_err(error)?
                    .values()
                    .cloned()
                    .collect();
                let mut found = None;
                for file in files {
                    if target.size == Some(file.size) && Self::digest(&file).await? == *expected {
                        found = Some(Arc::new(Fixed {
                            file: file.file.try_clone().map_err(error)?,
                            identity: target.identity.clone(),
                            size: file.size,
                        }));
                        break;
                    }
                }
                found.ok_or_else(||error("unique upload snapshot bytes missing; source is never replaced with a remote download"))?
            } else {
                return Err(error("unknown candidate"));
            };
            self.files.lock().map_err(error)?.insert(key, fixed.clone());
            Ok(Box::new(Shared(fixed)) as Box<dyn SnapshotSource>)
        })
    }
    fn verify<'a>(&'a self, target: &'a Target) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let files = self.files.lock().map_err(error)?;
            let file = files
                .get(&(target.kind.clone(), target.id.clone()))
                .ok_or_else(|| error("source not held"))?;
            if file.file.metadata().map_err(error)?.len() != file.size {
                return Err(error("source changed"));
            }
            Ok(())
        })
    }
}
struct Destination {
    protected: Vec<VaultHandle>,
}
struct Directory {
    vault: VaultHandle,
    files: Mutex<HashMap<String, File>>,
}
impl ExportDestination for Destination {
    fn create_new<'a>(
        &'a self,
        destination: &'a str,
    ) -> PortFuture<'a, Result<Box<dyn ExportDirectory>, PortError>> {
        Box::pin(async move {
            let path = Path::new(destination);
            let parent = VaultHandle::open(
                path.parent()
                    .ok_or_else(|| error("destination parent missing"))?,
            )
            .map_err(error)?;
            if self.protected.iter().any(|r| parent.is_within(r)) {
                return Err(error("export overlaps managed/internal directory"));
            }
            let name = path
                .file_name()
                .and_then(|v| v.to_str())
                .ok_or_else(|| error("invalid destination"))?;
            let vault = parent.create_new_directory(name).map_err(error)?;
            Ok(Box::new(Directory {
                vault,
                files: Mutex::new(HashMap::new()),
            }) as Box<dyn ExportDirectory>)
        })
    }
}
impl ExportDirectory for Directory {
    fn create<'a>(&'a self, name: &'a str) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let file = self.vault.create_new_file(name).map_err(error)?;
            if self
                .files
                .lock()
                .map_err(error)?
                .insert(name.into(), file)
                .is_some()
            {
                return Err(error("export name reused"));
            }
            Ok(())
        })
    }
    fn append<'a>(
        &'a self,
        name: &'a str,
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let mut files = self.files.lock().map_err(error)?;
            let file = files.get_mut(name).ok_or_else(|| error("file not held"))?;
            file.write_all(bytes).map_err(error)?;
            file.sync_all().map_err(error)
        })
    }
    fn read<'a>(
        &'a self,
        name: &'a str,
        offset: u64,
        length: u64,
    ) -> PortFuture<'a, Result<Vec<u8>, PortError>> {
        Box::pin(async move {
            let files = self.files.lock().map_err(error)?;
            let file = files.get(name).ok_or_else(|| error("file not held"))?;
            let mut bytes = vec![
                0;
                length.min(file.metadata().map_err(error)?.len().saturating_sub(offset))
                    as usize
            ];
            let n = file.seek_read(&mut bytes, offset).map_err(error)?;
            bytes.truncate(n);
            Ok(bytes)
        })
    }
    fn finish(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            for file in self.files.lock().map_err(error)?.values() {
                file.sync_all().map_err(error)?;
            }
            self.vault.verify().map_err(error)
        })
    }
}
pub(crate) async fn export(
    local: Arc<Local>,
    config: &AgentConfig,
    destination: &str,
) -> Result<usize, PortError> {
    let mut protected = vec![VaultHandle::open(&config.state_dir).map_err(error)?];
    for binding in &config.bindings {
        protected.push(VaultHandle::open(&binding.path).map_err(error)?);
        protected.push(VaultHandle::open(&binding.cache).map_err(error)?);
    }
    let source = Source {
        local: local.clone(),
        paths: paths(&local_metadata(&local).await?)?,
        cache: Arc::new(VaultHandle::open(&local.cache).map_err(error)?),
        files: Mutex::new(HashMap::new()),
    };
    let manifest = ExportRecovery::new(local.db.clone())
        .run(
            local.binding,
            destination,
            &source,
            &Destination { protected },
        )
        .await?;
    Ok(manifest.files.len())
}
