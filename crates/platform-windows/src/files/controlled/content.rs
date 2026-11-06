use super::{ProtectedFile, rename};
use crate::files::{
    FileIdentity, RootHandle,
    native::{metadata, open_child_shared},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    os::windows::fs::FileExt,
    path::Path,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use vaultsyncd_domain::version::Generation;
use vaultsyncd_fs_common::path::RelativePath;
use vaultsyncd_transfer::hash::ContentHasher;

/// Content evidence derived under a continuous exclusive oplock, never from mtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Monotonic local content generation, seeded from durable state at startup.
    pub generation: u64,
    /// Physical volume serial.
    pub volume: u64,
    /// Physical NTFS file ID.
    pub file: [u8; 16],
    /// Full SHA-256.
    pub digest: [u8; 32],
    /// Logical length.
    pub size: u64,
}
impl Evidence {
    /// Stable identity bytes for the core's LocalFacts and ContentEvidence.
    pub fn identity(&self) -> Vec<u8> {
        let mut v = self.volume.to_le_bytes().to_vec();
        v.extend(self.file);
        v
    }
    fn same_content(&self, other: &Self) -> bool {
        self.volume == other.volume
            && self.file == other.file
            && self.digest == other.digest
            && self.size == other.size
    }
}
/// A private disk snapshot. The read-only handle denies both writes and deletion.
pub struct FixedSnapshot {
    file: File,
    evidence: Evidence,
    name: String,
}
impl FixedSnapshot {
    /// Full immutable content evidence.
    pub fn evidence(&self) -> &Evidence {
        &self.evidence
    }
    /// Relative internal state filename; never expose this through wire DTOs.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Read exactly a bounded range from the immutable file.
    pub fn read_at(&self, offset: u64, length: u64) -> io::Result<Vec<u8>> {
        if offset
            .checked_add(length)
            .is_none_or(|n| n > self.evidence.size)
            || length > 1024 * 1024
        {
            return Err(invalid("invalid snapshot read range"));
        }
        let mut bytes = vec![0; length as usize];
        read_exact_at(&self.file, &mut bytes, offset)?;
        Ok(bytes)
    }
}
/// Missing is reported only after healthy pinned-root and parent verification.
pub enum ObservedContent {
    /// File does not exist in a healthy parent.
    Absent,
    /// Immutable content and the active object's generation/identity.
    File(Arc<FixedSnapshot>),
}
/// Durable platform publication, paired with the core local_intent by OpId.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    /// Original core operation identity (hex only).
    pub op: String,
    /// Validated relative destination.
    pub target: String,
    /// Real before evidence, or absent for non-overwriting first installation.
    pub before: Option<Evidence>,
    /// Private staged filename in the pinned internal state directory.
    pub staged: String,
    /// Preserved previous object, never automatically deleted.
    pub backup: String,
    /// Full new content hash.
    pub digest: [u8; 32],
    /// New length.
    pub size: u64,
    /// Physical identity of the verified staged object; equal bytes alone do not prove publication.
    pub new_volume: u64,
    /// Physical NTFS identity of the staged object.
    pub new_file: [u8; 16],
    /// Pinned original sync root volume; logs cannot be transplanted across roots.
    pub root_volume: u64,
    /// Pinned original sync root physical ID.
    pub root_file: [u8; 16],
}
/// Recovery classification. Unknown states preserve all three copies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// New complete contents already occupy the target; core may finish its journal.
    Published(String),
    /// Target was absent after preserving old object; old object was restored.
    Restored(String),
    /// No platform mutation had occurred; core can retry its original intent.
    Prepared(String),
    /// Evidence does not explain current state, so no destructive operation is admitted.
    Blocked(String),
}
/// Windows native content adapter; application code maps it to core ports.
/// All methods are blocking and must run outside UI threads and SQLite transactions.
pub struct WindowsFiles {
    root: RootHandle,
    state: RootHandle,
    generations: Mutex<HashMap<String, Evidence>>,
    gate: Mutex<()>,
}
impl WindowsFiles {
    /// Open disjoint existing local directories on the same volume. No files are created implicitly.
    pub fn open(root: &Path, state: &Path) -> io::Result<Self> {
        let root = RootHandle::open(root)?;
        let state = RootHandle::open(state)?;
        if root.overlaps(&state) || root.identity().volume != state.identity().volume {
            return Err(invalid("state/root must be disjoint on same volume"));
        }
        Ok(Self {
            root,
            state,
            generations: Mutex::new(HashMap::new()),
            gate: Mutex::new(()),
        })
    }
    /// Load durable prior evidence. The next observation still hashes the real file.
    pub fn seed(&self, path: &str, evidence: Evidence) -> io::Result<()> {
        RelativePath::parse(path).map_err(invalid)?;
        Generation::new(evidence.generation).map_err(invalid)?;
        let mut map = self.generations.lock().map_err(poison)?;
        if map.contains_key(path) {
            return Err(invalid("generation already seeded"));
        }
        map.insert(path.into(), evidence);
        Ok(())
    }
    /// Snapshot active contents while an exclusive RH oplock stays continuously valid.
    pub fn observe(&self, path: &str) -> io::Result<ObservedContent> {
        let _gate = self.gate.lock().map_err(poison)?;
        self.observe_inner(path)
    }
    fn observe_inner(&self, path: &str) -> io::Result<ObservedContent> {
        self.root.with_parent(path, |_, _| Ok(()))?;
        match self.root.inspect(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ObservedContent::Absent),
            Err(e) => return Err(e),
            Ok(meta) => {
                if meta.directory || meta.links != 1 || !meta.supported() {
                    return Err(invalid("unsupported content object"));
                }
            }
        }
        let (guard, first) = self.protect(path, false)?;
        let name = self.fresh("snapshot");
        let mut output = self
            .new_state(&name)
            .map_err(|e| io::Error::other(format!("snapshot create: {e}")))?;
        let mut hasher = ContentHasher::new();
        let mut offset = 0;
        while offset < first.size {
            let lease = guard.lease()?;
            let mut bytes = vec![0; (first.size - offset).min(256 * 1024) as usize];
            read_exact_at(lease.file(), &mut bytes, offset)
                .map_err(|e| io::Error::other(format!("snapshot read: {e}")))?;
            drop(lease);
            hasher.update(&bytes);
            output.write_all(&bytes)?;
            offset += bytes.len() as u64;
        }
        let lease = guard.lease()?;
        let end = metadata(lease.file())
            .map_err(|e| io::Error::other(format!("snapshot metadata: {e}")))?;
        if end.identity != first.identity || end.size != first.size {
            return Err(changed());
        }
        output.sync_all()?;
        let mut evidence = Evidence {
            generation: 0,
            volume: first.identity.volume,
            file: first.identity.file,
            digest: hasher.finish(),
            size: first.size,
        };
        evidence.generation = self.generation(path, &evidence)?;
        let file = output;
        Ok(ObservedContent::File(Arc::new(FixedSnapshot {
            file,
            evidence,
            name,
        })))
    }
    fn generation(&self, path: &str, evidence: &Evidence) -> io::Result<u64> {
        let mut map = self.generations.lock().map_err(poison)?;
        let generation = match map.get(path) {
            Some(old) if old.same_content(evidence) => old.generation,
            Some(old) => old
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("generation exhausted"))?,
            None => 1,
        };
        Generation::new(generation).map_err(invalid)?;
        let mut e = evidence.clone();
        e.generation = generation;
        map.insert(path.into(), e);
        Ok(generation)
    }
    /// Acquire a continuous exclusive CFAPI oplock after pinned no-follow metadata validation.
    /// Partially hydrated files, unknown reparses, hard links and identity races are refused.
    /// Callers must lease and recheck actual content/generation before destructive operations.
    pub fn protect(
        &self,
        path: &str,
        write: bool,
    ) -> io::Result<(ProtectedFile, crate::files::EntryMetadata)> {
        protect_under(&self.root, path, write)
    }

    /// Preserve a file under another validated relative name without overwriting anything.
    /// Caller must persist the same source/target/evidence before calling and recover by identity.
    /// The original object remains protected continuously through full-content recheck and rename.
    pub fn preserve(&self, from: &str, to: &str, expected: &Evidence) -> io::Result<Evidence> {
        let _gate = self.gate.lock().map_err(poison)?;
        self.root.with_parent(to, |_, _| Ok(()))?;
        let (guard, meta) = self.protect(from, true)?;
        let lease = guard.lease()?;
        let actual = self.evidence_handle(from, lease.file(), meta.identity)?;
        if &actual != expected {
            return Err(changed());
        }
        self.root
            .with_parent(to, |parent, name| rename(lease.file(), parent, name, 0))?;
        lease.file().sync_all()?;
        let mut map = self.generations.lock().map_err(poison)?;
        map.remove(from);
        map.insert(to.into(), actual.clone());
        Ok(actual)
    }

    /// Publish verified contents with durable before/after evidence and preserved original object.
    /// Namespace publication has a recoverable two-rename interval, not a single atomic rename.
    /// Caller must first persist its core local_intent using the same OpId.
    pub fn replace(
        &self,
        op: &str,
        target: &str,
        staged: &Path,
        before: Option<&Evidence>,
        digest: [u8; 32],
    ) -> io::Result<Evidence> {
        let _gate = self.gate.lock().map_err(poison)?;
        validate_op(op)?;
        RelativePath::parse(target).map_err(invalid)?;
        self.state.verify()?;
        self.root.verify()?;
        let stage_name = format!("publication-{op}.data");
        let backup = format!("preserved-{op}.data");
        let journal = format!("publication-{op}.json");
        let existing = match self.read_state(&journal) {
            Ok(mut log) => {
                if log.metadata()?.len() > 64 * 1024 {
                    return Err(invalid("publication log too large"));
                }
                let mut bytes = vec![];
                log.read_to_end(&mut bytes)?;
                let p: Publication = serde_json::from_slice(&bytes).map_err(invalid)?;
                validate_record(&p, &journal)?;
                self.check_root(&p)?;
                if p.target != target || p.before.as_ref() != before || p.digest != digest {
                    return Err(invalid("OP_ID_REUSED: publication inputs differ"));
                }
                Some(p)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let already_logged = existing.is_some();
        let record = if let Some(p) = existing {
            p
        } else {
            let meta = self.prepare_staged(staged, &stage_name, digest)?;
            Publication {
                op: op.into(),
                target: target.into(),
                before: before.cloned(),
                staged: stage_name.clone(),
                backup: backup.clone(),
                digest,
                size: meta.size,
                new_volume: meta.identity.volume,
                new_file: meta.identity.file,
                root_volume: self.root.identity().volume,
                root_file: self.root.identity().file,
            }
        };
        let size = record.size;
        if already_logged {
            if let ObservedContent::File(snapshot) = self.observe_inner(target)? {
                if published(&record, snapshot.evidence()) {
                    return Ok(snapshot.evidence().clone());
                }
            }
        }
        let guard = if let Some(expected) = before {
            let (guard, meta) = self.protect(target, true)?;
            let lease = guard.lease()?;
            let actual = self.evidence_handle(target, lease.file(), meta.identity)?;
            if &actual != expected {
                return Err(changed());
            }
            drop(lease);
            Some(guard)
        } else {
            self.root.with_parent(target, |parent, leaf| {
                match open_child_shared(parent, leaf, false, false, 0x00100080, false, 7) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e),
                    Ok(_) => Err(changed()),
                }
            })?;
            None
        };
        if !already_logged {
            let mut log = self.new_state(&journal)?;
            log.write_all(&serde_json::to_vec(&record).map_err(invalid)?)?;
            log.sync_all()?;
        }
        let (stage_guard, stage_meta) = protect_under(&self.state, &stage_name, true)?;
        let stage = stage_guard.lease()?;
        if stage_meta.identity
            != (FileIdentity {
                volume: record.new_volume,
                file: record.new_file,
            })
            || hash_file(stage.file())? != (record.digest, record.size)
        {
            return Err(changed());
        }
        // Keep old object protected until the new name is installed. No unchecked overwrite occurs.
        let held = guard.as_ref().map(ProtectedFile::lease).transpose()?;
        if let Some(lease) = &held {
            self.state.with_parent(&backup, |parent, leaf| {
                rename(lease.file(), parent, leaf, 0)
            })?;
        }
        checkpoint("preserved");
        let result = self
            .root
            .with_parent(target, |parent, leaf| rename(stage.file(), parent, leaf, 0));
        if let Err(error) = result {
            if let Some(lease) = &held {
                if let Err(restore_error) = self
                    .root
                    .with_parent(target, |parent, leaf| rename(lease.file(), parent, leaf, 0))
                {
                    return Err(io::Error::other(format!(
                        "NEEDS_RECOVERY: publication failed ({error}); restore refused ({restore_error}); old object and stage retained"
                    )));
                }
            }
            return Err(error);
        }
        stage
            .file()
            .sync_all()
            .map_err(|e| io::Error::other(format!("stage sync: {e}")))?;
        checkpoint("published");
        drop(held);
        drop(guard);
        drop(stage);
        drop(stage_guard);
        let snapshot = match self.observe_inner(target)? {
            ObservedContent::File(s) => s,
            ObservedContent::Absent => return Err(changed()),
        };
        if !published(&record, snapshot.evidence()) || snapshot.evidence.size != size {
            return Err(changed());
        }
        // Keep evidence until the core has committed its baseline; never remove preserved contents automatically.
        Ok(snapshot.evidence.clone())
    }
    /// Recover platform journals before core local_intents or any synchronization is admitted.
    /// Third-party target contents are never overwritten, even if an old move was interrupted.
    pub fn recover_publications(&self) -> io::Result<Vec<RecoveryOutcome>> {
        let _gate = self.gate.lock().map_err(poison)?;
        let report = self.state.scan();
        if !report.permits_missing_comparison() {
            return Err(invalid("internal state scan incomplete"));
        }
        let mut results = vec![];
        for item in report.entries {
            if !item.path.starts_with("publication-") || !item.path.ends_with(".json") {
                continue;
            }
            let mut file = self.read_state(&item.path)?;
            if file.metadata()?.len() > 64 * 1024 {
                return Err(invalid("publication journal too large"));
            }
            let mut bytes = vec![];
            file.read_to_end(&mut bytes)?;
            let p: Publication = serde_json::from_slice(&bytes).map_err(invalid)?;
            validate_record(&p, &item.path)?;
            self.check_root(&p)?;
            let state = match self.observe_inner(&p.target) {
                Ok(ObservedContent::File(snapshot)) if published(&p, snapshot.evidence()) => {
                    RecoveryOutcome::Published(p.op)
                }
                Ok(ObservedContent::File(snapshot))
                    if p.before
                        .as_ref()
                        .is_some_and(|old| old.same_content(&snapshot.evidence)) =>
                {
                    RecoveryOutcome::Prepared(p.op)
                }
                Ok(ObservedContent::Absent) => {
                    if let Some(before) = &p.before {
                        match protect_under(&self.state, &p.backup, true) {
                            Ok((protected, meta)) => {
                                let lease = protected.lease()?;
                                let file = lease.file();
                                let actual = hash_file(file)?;
                                if meta.identity
                                    == (FileIdentity {
                                        volume: before.volume,
                                        file: before.file,
                                    })
                                    && actual == (before.digest, before.size)
                                {
                                    self.root.with_parent(&p.target, |parent, leaf| {
                                        rename(file, parent, leaf, 0)
                                    })?;
                                    file.sync_all()?;
                                    RecoveryOutcome::Restored(p.op)
                                } else {
                                    RecoveryOutcome::Blocked(p.op)
                                }
                            }
                            Err(_) => RecoveryOutcome::Blocked(p.op),
                        }
                    } else {
                        RecoveryOutcome::Prepared(p.op)
                    }
                }
                _ => RecoveryOutcome::Blocked(p.op),
            };
            results.push(state);
        }
        Ok(results)
    }
    /// Remove only the matching publication journal after the core baseline transaction commits.
    /// Preserved old objects and snapshots remain available for explicit recovery/export policy.
    pub fn acknowledge_publication(&self, op: &str) -> io::Result<()> {
        validate_op(op)?;
        let _gate = self.gate.lock().map_err(poison)?;
        checkpoint("before_ack");
        // Delete by opened handle under pinned parent, never by an unchecked textual path.
        self.state
            .with_parent(&format!("publication-{op}.json"), |parent, leaf| {
                let file = open_child_shared(parent, leaf, false, false, 0x00110080, false, 0)?;
                #[link(name = "kernel32")]
                unsafe extern "system" {
                    fn SetFileInformationByHandle(
                        h: *mut std::ffi::c_void,
                        class: i32,
                        buffer: *const u8,
                        len: u32,
                    ) -> i32;
                }
                use std::os::windows::io::AsRawHandle;
                if unsafe { SetFileInformationByHandle(file.as_raw_handle(), 4, &1, 1) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })
    }
    fn check_root(&self, p: &Publication) -> io::Result<()> {
        if p.root_volume != self.root.identity().volume || p.root_file != self.root.identity().file
        {
            return Err(invalid("publication belongs to another root"));
        }
        Ok(())
    }
    fn prepare_staged(
        &self,
        source: &Path,
        name: &str,
        digest: [u8; 32],
    ) -> io::Result<crate::files::EntryMetadata> {
        match self.read_state(name) {
            Ok(file) => {
                if hash_file(&file)?.0 != digest {
                    return Err(invalid("OP_ID_REUSED: private stage differs"));
                }
                return metadata(&file);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let parent = source
            .parent()
            .ok_or_else(|| invalid("staged parent missing"))?;
        let leaf = source
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| invalid("staged name invalid"))?;
        let source_root = RootHandle::open(parent)?;
        source_root.with_parent(leaf, |_, _| {
            let initial = source_root.inspect(leaf)?;
            if initial.directory || !initial.supported() || initial.links != 1 {
                return Err(invalid("unsafe stage"));
            }
            let protected = ProtectedFile::open(source, false)?;
            let lease = protected.lease()?;
            if metadata(lease.file())?.identity != initial.identity {
                return Err(changed());
            }
            drop(lease);
            let temp = self.fresh("incoming");
            let mut output = self.state.with_parent(&temp, |parent, leaf| {
                open_child_shared(parent, leaf, false, false, 0x00110083, true, 1)
            })?;
            let mut h = ContentHasher::new();
            let mut offset = 0;
            while offset < initial.size {
                let lease = protected.lease()?;
                let mut bytes = vec![0; (initial.size - offset).min(256 * 1024) as usize];
                read_exact_at(lease.file(), &mut bytes, offset)?;
                drop(lease);
                h.update(&bytes);
                output.write_all(&bytes)?;
                offset += bytes.len() as u64;
            }
            let _lease = protected.lease()?;
            if h.finish() != digest {
                return Err(io::Error::other("HASH_MISMATCH: staged contents differ"));
            }
            output.sync_all()?;
            self.state
                .with_parent(name, |parent, leaf| rename(&output, parent, leaf, 0))?;
            metadata(&output)
        })
    }
    fn evidence_handle(
        &self,
        path: &str,
        file: &File,
        identity: FileIdentity,
    ) -> io::Result<Evidence> {
        let (digest, size) = hash_file(file)?;
        let mut e = Evidence {
            generation: 0,
            volume: identity.volume,
            file: identity.file,
            digest,
            size,
        };
        e.generation = self.generation(path, &e)?;
        Ok(e)
    }
    fn new_state(&self, name: &str) -> io::Result<File> {
        self.state.with_parent(name, |parent, leaf| {
            open_child_shared(parent, leaf, false, false, 0x00100083, true, 1)
        })
    }
    fn read_state(&self, name: &str) -> io::Result<File> {
        self.state.with_parent(name, |parent, leaf| {
            open_child_shared(parent, leaf, false, false, 0x00100081, false, 1)
        })
    }
    fn fresh(&self, prefix: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "{prefix}-{}-{}-{}.data",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }
}
fn hash_file(file: &File) -> io::Result<([u8; 32], u64)> {
    let size = metadata(file)?.size;
    let mut h = ContentHasher::new();
    let mut offset = 0;
    while offset < size {
        let mut bytes = vec![0; (size - offset).min(256 * 1024) as usize];
        read_exact_at(file, &mut bytes, offset)?;
        h.update(&bytes);
        offset += bytes.len() as u64;
    }
    Ok((h.finish(), size))
}
fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = file.seek_read(bytes, offset)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "snapshot short read",
            ));
        }
        offset += n as u64;
        bytes = &mut bytes[n..];
    }
    Ok(())
}
fn validate_op(op: &str) -> io::Result<()> {
    if op.len() != 32 || !op.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("invalid operation identity"));
    }
    Ok(())
}
fn validate_record(p: &Publication, name: &str) -> io::Result<()> {
    validate_op(&p.op)?;
    RelativePath::parse(&p.target).map_err(invalid)?;
    if name != format!("publication-{}.json", p.op)
        || p.staged != format!("publication-{}.data", p.op)
        || p.backup != format!("preserved-{}.data", p.op)
    {
        return Err(invalid("invalid publication paths"));
    }
    Ok(())
}
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
}
fn poison<T>(e: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other(e.to_string())
}
fn changed() -> io::Error {
    io::Error::other("LOCAL_CHANGED: actual identity/generation/content differs")
}
#[cfg(test)]
fn checkpoint(name: &str) {
    if std::env::var("VAULTSYNCD_S068_CRASH").as_deref() == Ok(name) {
        std::process::exit(match name {
            "preserved" => 121,
            "published" => 122,
            _ => 123,
        });
    }
}
#[cfg(not(test))]
fn checkpoint(_: &str) {}

fn published(p: &Publication, e: &Evidence) -> bool {
    e.digest == p.digest && e.size == p.size && e.volume == p.new_volume && e.file == p.new_file
}

pub(super) fn protect_under(
    root: &RootHandle,
    path: &str,
    write: bool,
) -> io::Result<(ProtectedFile, crate::files::EntryMetadata)> {
    root.with_parent(path, |_, _| {
        let before = root.inspect(path)?;
        if before.directory || !before.supported() || before.links != 1 {
            return Err(invalid("unsafe content object"));
        }
        if before
            .cloud
            .as_ref()
            .is_some_and(|c| c.on_disk < before.size)
        {
            return Err(io::Error::other("CONTENT_UNAVAILABLE: partial placeholder"));
        }
        let guard = ProtectedFile::open(&root.root_path().join(path.replace('/', "\\")), write)?;
        let lease = guard.lease()?;
        let after = metadata(lease.file())?;
        if before.identity != after.identity || !after.supported() || after.links != 1 {
            return Err(changed());
        }
        drop(lease);
        Ok((guard, after))
    })
}
