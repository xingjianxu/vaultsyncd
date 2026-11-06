//! Historical storage identity is durable; mount/device numbers fence one process only.
//! No historical identity is rewritten. All consumers share the admitted open directory.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use vaultsyncd_domain::ids::RootId;
use vaultsyncd_platform_linux_server::{
    files::{ResolveMode, RootHandle, RootSnapshot, root::RootSpecVerifyError},
    mounts::{health, probe::ScanPolicy},
};
use vaultsyncd_server_core::api::roots::CandidateFacts;
use vaultsyncd_server_store::{
    db::Database,
    roots::{RootHealth, RootRecord},
};

type Entry = Result<Arc<RootHandle>, String>;
pub(crate) struct RootFences {
    state: PathBuf,
    roots: Mutex<HashMap<RootId, Entry>>,
}
impl RootFences {
    pub(crate) fn new(state: &Path) -> Self {
        Self {
            state: state.to_owned(),
            roots: Mutex::new(HashMap::new()),
        }
    }
    /// Called exactly once before startup replay, listeners or background work.
    pub(crate) async fn load(db: &Database, state: &Path) -> Result<Self, String> {
        let mut conn = db.pool().acquire().await.map_err(|e| e.to_string())?;
        let records = vaultsyncd_server_store::roots::all_roots(&mut conn)
            .await
            .map_err(|e| e.to_string())?;
        drop(conn);
        let table = Self::new(state);
        for record in records {
            let entry = table.admit(&record, true);
            table
                .roots
                .lock()
                .map_err(|_| "root_fence_poisoned")?
                .insert(record.id, entry);
        }
        Ok(table)
    }
    fn admit(&self, record: &RootRecord, startup: bool) -> Entry {
        let old = std::str::from_utf8(&record.root_file_identity)
            .ok()
            .and_then(|s| RootSnapshot::parse_spec(s).ok())
            .ok_or("invalid_root_identity")?;
        let path = Path::new(&record.absolute_path);
        let (_, candidate) =
            crate::roots::probe_candidate(path, &self.state, &[], &ScanPolicy::default());
        let CandidateFacts::Accepted {
            volume_identity,
            root_identity,
        } = candidate
        else {
            return Err("storage_admission_failed".into());
        };
        if volume_identity != record.volume_identity {
            return Err("filesystem_changed".into());
        }
        let current = RootSnapshot::parse_spec(
            std::str::from_utf8(&root_identity).map_err(|_| "invalid_root_identity")?,
        )
        .map_err(|_| "invalid_root_identity")?;
        persistent_match(&old, &current)?;
        // New registrations in a live process never get a namespace/device rebase.
        let handle = RootHandle::open(path, ResolveMode::Auto).map_err(|e| e.code().to_owned())?;
        current
            .compare(handle.facts())
            .map_err(|e| e.code().to_owned())?;
        if !startup {
            old.compare(handle.facts())
                .map_err(|e| e.code().to_owned())?;
        }
        Self::check(&handle)?;
        Ok(Arc::new(handle))
    }
    pub(crate) fn open(&self, record: &RootRecord) -> Entry {
        if record.health != RootHealth::Healthy {
            return Err("stored_unavailable".into());
        }
        let cached = self
            .roots
            .lock()
            .map_err(|_| "root_fence_poisoned")?
            .get(&record.id)
            .cloned();
        let entry = match cached {
            Some(entry) => entry,
            None => {
                let candidate = self.admit(record, false);
                self.roots
                    .lock()
                    .map_err(|_| "root_fence_poisoned")?
                    .entry(record.id)
                    .or_insert(candidate)
                    .clone()
            }
        };
        let handle = entry?;
        if handle.path() != Path::new(&record.absolute_path) {
            return Err("root_path_changed".into());
        }
        Self::check(&handle)?;
        Ok(handle)
    }
    fn check(handle: &RootHandle) -> Result<(), String> {
        handle.verify().map_err(|e| match e {
            RootSpecVerifyError::Drift(d) => d.code().to_owned(),
            RootSpecVerifyError::Mount(m) => m.detail_code().to_owned(),
            _ => "root_unreadable".into(),
        })?;
        let facts = health::verify_root_mount(handle.path(), handle.snapshot())
            .map_err(|e| e.detail_code().to_owned())?
            .facts;
        if facts.mount.has_option("ro")
            || facts.mount.has_option("nodatacow")
            || facts.mount.has_option("nodatasum")
            || facts.inode_flags.is_none_or(|v| v & 0x0080_0000 != 0)
        {
            return Err("storage_admission_failed".into());
        }
        health::check_access_dir(handle.path()).map_err(|_| "permission_denied".to_owned())
    }
}
fn persistent_match(old: &RootSnapshot, current: &RootSnapshot) -> Result<(), &'static str> {
    if old.backend != current.backend || old.fs_magic != current.fs_magic {
        return Err("backend_changed");
    }
    if old.fsid.is_none() || old.fsid != current.fsid {
        return Err("filesystem_changed");
    }
    if old.subvolume_id.is_none() || old.subvolume_id != current.subvolume_id {
        return Err("subvolume_changed");
    }
    if old.resolved_path != current.resolved_path
        || old.inode != current.inode
        || old.btime.is_none()
        || old.btime != current.btime
    {
        return Err("directory_changed");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> RootSnapshot {
        RootSnapshot::parse_spec("resolved_path=/fixture\nbackend=btrfs\nfs_magic=2435016766\nfsid=01010101010101010101010101010101\nsubvolume_id=5\nmount_id=100\ndev_major=0\ndev_minor=8\ninode=258\nbtime=100.123\n").unwrap()
    }
    #[test]
    fn only_startup_persistent_match_excludes_transient_numbers() {
        let old = snapshot();
        let mut current = old.clone();
        current.mount_id += 1;
        current.dev_minor += 1;
        assert_eq!(persistent_match(&old, &current), Ok(()));
        for variant in 0..6 {
            let mut bad = current.clone();
            match variant {
                0 => bad.fsid = Some([2; 16]),
                1 => bad.subvolume_id = Some(6),
                2 => bad.inode += 1,
                3 => bad.btime = Some((101, 123)),
                4 => bad.resolved_path = "/replacement".into(),
                _ => bad.btime = None,
            };
            assert!(persistent_match(&old, &bad).is_err());
        }
    }
}
