//! 外部观察测试共用的内存文件系统、对象端口与采纳存储（仅 `cfg(test)`）。
//!
//! 内存文件系统用一个单调 `tick` 模拟时间：每次写入推进 `ctime`，`mtime` 默认跟随，也可以被测试
//! 显式“保留”（模拟 `touch -r` 或保留 mtime 的改写）。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::rc::Rc;

use sha2::{Digest, Sha256};
use vaultsyncd_domain::ids::{EntryId, ID_LEN, TxId, VaultId, VersionId};
use vaultsyncd_domain::version::{Revision, VaultSeq};

use crate::objects::ContentDigest;
use crate::ports::external_store::{
    AdoptKind, ExternalAdoptStore, ExternalDelete, ExternalDeleteRecord, ExternalMove,
    ExternalMoveRecord, ExternalRecord, ExternalStoreError, ExternalWrite, KnownEntry,
};
use crate::ports::object_store::{
    BlobError, CopyFallback, CopyMethod, FileKey, ObjectBlobPort, PublishedBlob, StagedBlob,
    StorageFailure, StoredBytes, StoredFacts, StoredSlice,
};
use crate::ports::put_store::EntryKind;
use crate::ports::work_tree::{
    DirIdentity, FileStamp, WorkDirEntry, WorkDirListing, WorkEntryKind, WorkObservation,
    WorkTreeError, WorkTreePort,
};

pub(crate) fn digest_of(data: &[u8]) -> ContentDigest {
    ContentDigest::from_bytes(Sha256::digest(data).into())
}

#[derive(Clone)]
pub(crate) struct FakeFile {
    pub(crate) data: Vec<u8>,
    pub(crate) inode: u64,
    pub(crate) mtime: i64,
    pub(crate) ctime: i64,
}

#[derive(Default)]
pub(crate) struct FsState {
    pub(crate) files: BTreeMap<String, FakeFile>,
    pub(crate) dirs: BTreeSet<String>,
    pub(crate) symlinks: BTreeSet<String>,
    pub(crate) unreadable_dirs: BTreeSet<String>,
    /// 目录读失败但不是权限问题（`EIO` 等）：健康结论必须是 `io_error`。
    pub(crate) io_error_dirs: BTreeSet<String>,
    /// 目录项仍被列出，但进入时已确认不存在（并发删除）。
    pub(crate) gone_dirs: BTreeSet<String>,
    pub(crate) unreadable_files: BTreeSet<String>,
    pub(crate) vault_unavailable: bool,
    pub(crate) unrepresentable_in: BTreeMap<String, usize>,
    /// 目录身份（设备 1 + inode）；首次读取时分配。
    pub(crate) dir_inodes: BTreeMap<String, u64>,
    pub(crate) tick: i64,
    next_inode: u64,
    next_dir_inode: u64,
}

impl FsState {
    fn bump(&mut self) -> i64 {
        self.tick += 1;
        self.tick
    }

    fn ensure_parents(&mut self, path: &str) {
        let mut prefix = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            self.dirs.insert(prefix.clone());
        }
    }

    /// 新建或原地改写（原地改写保留 inode，推进 `ctime` 与 `mtime`）。
    pub(crate) fn write(&mut self, path: &str, data: &[u8]) {
        self.ensure_parents(path);
        let now = self.bump();
        if let Some(file) = self.files.get_mut(path) {
            file.data = data.to_vec();
            file.mtime = now;
            file.ctime = now;
        } else {
            self.next_inode += 1;
            let inode = 100 + self.next_inode;
            self.files.insert(
                path.to_owned(),
                FakeFile {
                    data: data.to_vec(),
                    inode,
                    mtime: now,
                    ctime: now,
                },
            );
        }
    }

    /// 改写内容但把 `mtime` 恢复成旧值（`ctime` 无法恢复）。
    pub(crate) fn write_preserving_mtime(&mut self, path: &str, data: &[u8]) {
        let old = self.files.get(path).expect("文件存在").mtime;
        self.write(path, data);
        self.files.get_mut(path).expect("文件存在").mtime = old;
    }

    /// 只改 mtime/ctime，不改内容（`touch`）。
    pub(crate) fn touch(&mut self, path: &str) {
        let now = self.bump();
        let file = self.files.get_mut(path).expect("文件存在");
        file.mtime = now;
        file.ctime = now;
    }

    /// 用 rename-over 风格替换：新 inode，同路径。
    pub(crate) fn replace(&mut self, path: &str, data: &[u8]) {
        self.files.remove(path);
        self.write(path, data);
    }

    pub(crate) fn unlink(&mut self, path: &str) {
        self.files.remove(path);
    }

    /// 改名（同一文件对象换路径）：inode 与 `ctime` 都不变，与 `rename(2)` 对文件本身的效果一致。
    pub(crate) fn rename(&mut self, from: &str, to: &str) {
        self.ensure_parents(to);
        let file = self.files.remove(from).expect("源文件存在");
        self.files.insert(to.to_owned(), file);
    }

    /// 目录身份（设备 1，inode 首次读取时分配，之后保持）。
    fn dir_identity(&mut self, path: &str) -> DirIdentity {
        let next = self.next_dir_inode + 1;
        let inode = *self.dir_inodes.entry(path.to_owned()).or_insert_with(|| {
            self.next_dir_inode = next;
            2_000 + next
        });
        DirIdentity {
            device: 1,
            inode,
            btime: Some((1, 0)),
        }
    }

    /// 替换目录身份（模拟目录被换掉：unmount 后露出另一个目录）。
    pub(crate) fn replace_dir(&mut self, path: &str) {
        self.next_dir_inode += 1;
        let inode = 2_000 + self.next_dir_inode;
        self.dir_inodes.insert(path.to_owned(), inode);
    }

    fn stamp_of(&self, path: &str) -> FileStamp {
        let file = &self.files[path];
        FileStamp {
            key: FileKey {
                device: 1,
                inode: file.inode,
                links: 1,
                bytes: file.data.len() as u64,
            },
            btime: Some((1, 0)),
            mtime: (file.mtime, 0),
            ctime: (file.ctime, 0),
        }
    }
}

pub(crate) type SharedFs = Rc<RefCell<FsState>>;

pub(crate) fn shared_fs() -> SharedFs {
    Rc::new(RefCell::new(FsState::default()))
}

pub(crate) struct FakeWorkTree {
    pub(crate) fs: SharedFs,
}

impl FakeWorkTree {
    pub(crate) fn new(fs: &SharedFs) -> Self {
        FakeWorkTree { fs: fs.clone() }
    }
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

impl WorkTreePort for FakeWorkTree {
    fn list_dir(&mut self, dir: &str) -> Result<WorkDirListing, WorkTreeError> {
        let fs = self.fs.borrow();
        if fs.vault_unavailable {
            return Err(WorkTreeError::VaultUnavailable {
                detail: "挂载身份漂移".to_owned(),
            });
        }
        if fs.gone_dirs.contains(dir) {
            return Err(WorkTreeError::Gone {
                path: dir.to_owned(),
            });
        }
        if fs.unreadable_dirs.contains(dir) {
            return Err(WorkTreeError::PermissionDenied {
                path: dir.to_owned(),
                detail: "Permission denied".to_owned(),
            });
        }
        if fs.io_error_dirs.contains(dir) {
            return Err(WorkTreeError::Unreadable {
                path: dir.to_owned(),
                detail: "Input/output error".to_owned(),
            });
        }
        let mut entries = Vec::new();
        let name_of = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
        for path in fs.files.keys().filter(|path| parent_of(path) == dir) {
            entries.push(WorkDirEntry {
                name: name_of(path),
                kind: WorkEntryKind::File,
            });
        }
        for path in fs
            .dirs
            .iter()
            .filter(|path| !path.is_empty() && parent_of(path) == dir)
        {
            entries.push(WorkDirEntry {
                name: name_of(path),
                kind: WorkEntryKind::Directory,
            });
        }
        for path in fs.symlinks.iter().filter(|path| parent_of(path) == dir) {
            entries.push(WorkDirEntry {
                name: name_of(path),
                kind: WorkEntryKind::Symlink,
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(WorkDirListing {
            entries,
            unrepresentable_names: fs.unrepresentable_in.get(dir).copied().unwrap_or(0),
        })
    }

    fn observe_dir(&mut self, dir: &str) -> Result<Option<DirIdentity>, WorkTreeError> {
        let mut fs = self.fs.borrow_mut();
        if fs.vault_unavailable {
            return Err(WorkTreeError::VaultUnavailable {
                detail: "挂载身份漂移".to_owned(),
            });
        }
        if fs.unreadable_dirs.contains(dir) {
            return Err(WorkTreeError::PermissionDenied {
                path: dir.to_owned(),
                detail: "Permission denied".to_owned(),
            });
        }
        if fs.io_error_dirs.contains(dir) {
            return Err(WorkTreeError::Unreadable {
                path: dir.to_owned(),
                detail: "Input/output error".to_owned(),
            });
        }
        if !dir.is_empty() && !fs.dirs.contains(dir) {
            return Ok(None);
        }
        Ok(Some(fs.dir_identity(dir)))
    }

    fn observe(&mut self, path: &str) -> Result<WorkObservation, WorkTreeError> {
        let fs = self.fs.borrow();
        if fs.vault_unavailable {
            return Err(WorkTreeError::VaultUnavailable {
                detail: "挂载身份漂移".to_owned(),
            });
        }
        if fs.unreadable_files.contains(path) {
            return Err(WorkTreeError::PermissionDenied {
                path: path.to_owned(),
                detail: "Permission denied".to_owned(),
            });
        }
        if fs.files.contains_key(path) {
            return Ok(WorkObservation::File(fs.stamp_of(path)));
        }
        if fs.symlinks.contains(path) {
            return Ok(WorkObservation::NotRegular(WorkEntryKind::Symlink));
        }
        if fs.dirs.contains(path) {
            return Ok(WorkObservation::NotRegular(WorkEntryKind::Directory));
        }
        Ok(WorkObservation::Absent)
    }
}

pub(crate) type StageHook = Box<dyn FnOnce(&mut FsState)>;

/// 内存对象端口：候选内容取自内存文件系统。
pub(crate) struct FakeBlobs {
    pub(crate) fs: SharedFs,
    pub(crate) staged: BTreeMap<String, Vec<u8>>,
    pub(crate) objects: BTreeMap<String, Vec<u8>>,
    next_inode: u64,
    /// 在读取源内容之后、返回候选之前执行（模拟复制期间的并发写）。
    pub(crate) during_stage: Option<StageHook>,
    /// 候选比源看到的长度多这么多字节（模拟复制期间源在增长）。
    pub(crate) grow_by: usize,
    pub(crate) fallback: Option<CopyFallback>,
    pub(crate) stage_calls: usize,
    pub(crate) stage_error: Option<BlobError>,
}

impl FakeBlobs {
    pub(crate) fn new(fs: &SharedFs) -> Self {
        FakeBlobs {
            fs: fs.clone(),
            staged: BTreeMap::new(),
            objects: BTreeMap::new(),
            next_inode: 900,
            during_stage: None,
            grow_by: 0,
            fallback: None,
            stage_calls: 0,
            stage_error: None,
        }
    }
}

impl ObjectBlobPort for FakeBlobs {
    fn stage_working_copy(
        &mut self,
        source: &Path,
        staging: &Path,
    ) -> Result<StagedBlob, BlobError> {
        self.stage_calls += 1;
        if let Some(error) = self.stage_error.take() {
            return Err(error);
        }
        let path = source.to_str().expect("测试路径是 UTF-8").to_owned();
        let (data, source_key) = {
            let fs = self.fs.borrow();
            if fs.unreadable_files.contains(&path) {
                return Err(BlobError::SourceUnreadable {
                    detail: "Permission denied".to_owned(),
                });
            }
            let Some(file) = fs.files.get(&path) else {
                return Err(BlobError::SourceUnreadable {
                    detail: "No such file".to_owned(),
                });
            };
            (
                file.data.clone(),
                FileKey {
                    device: 1,
                    inode: file.inode,
                    links: 1,
                    bytes: file.data.len() as u64,
                },
            )
        };
        if let Some(hook) = self.during_stage.take() {
            hook(&mut self.fs.borrow_mut());
        }
        let mut copied = data;
        copied.extend(std::iter::repeat_n(b'+', self.grow_by));
        self.next_inode += 1;
        let staged_key = FileKey {
            device: 2,
            inode: self.next_inode,
            links: 1,
            bytes: copied.len() as u64,
        };
        let digest = digest_of(&copied);
        let name = staging.to_str().expect("暂存路径是 UTF-8").to_owned();
        self.staged.insert(name, copied);
        Ok(StagedBlob {
            staging: staging.to_path_buf(),
            source: source_key,
            staged: staged_key,
            digest,
            method: if self.fallback.is_some() {
                CopyMethod::VerifiedCopy
            } else {
                CopyMethod::Reflink
            },
            fallback: self.fallback,
        })
    }

    fn publish_staged(
        &mut self,
        staged: &StagedBlob,
        object: &Path,
    ) -> Result<PublishedBlob, BlobError> {
        let name = staged.staging.to_str().expect("UTF-8").to_owned();
        let data = self
            .staged
            .remove(&name)
            .ok_or_else(|| BlobError::Storage {
                stage: "publish",
                failure: StorageFailure::Other,
                detail: "暂存副本不存在".to_owned(),
            })?;
        let key = object.to_str().expect("UTF-8").to_owned();
        if self.objects.contains_key(&key) {
            return Ok(PublishedBlob {
                object: object.to_path_buf(),
                already_present: true,
            });
        }
        self.objects.insert(key, data);
        Ok(PublishedBlob {
            object: object.to_path_buf(),
            already_present: false,
        })
    }

    fn discard_staged(&mut self, staging: &Path) -> Result<(), BlobError> {
        self.staged.remove(staging.to_str().expect("UTF-8"));
        Ok(())
    }

    fn verify_object(&mut self, object: &Path) -> Result<StoredFacts, BlobError> {
        let data = self
            .objects
            .get(object.to_str().expect("UTF-8"))
            .ok_or_else(|| BlobError::ObjectMissing {
                detail: object.display().to_string(),
            })?;
        Ok(StoredFacts {
            bytes: data.len() as u64,
            digest: digest_of(data),
        })
    }

    fn read_object(&mut self, object: &Path) -> Result<StoredBytes, BlobError> {
        let data = self
            .objects
            .get(object.to_str().expect("UTF-8"))
            .ok_or_else(|| BlobError::ObjectMissing {
                detail: object.display().to_string(),
            })?
            .clone();
        Ok(StoredBytes {
            bytes: data.len() as u64,
            digest: digest_of(&data),
            data,
        })
    }

    fn read_object_range(
        &mut self,
        object: &Path,
        offset: u64,
        length: u64,
    ) -> Result<StoredSlice, BlobError> {
        let data = self
            .objects
            .get(object.to_str().expect("UTF-8"))
            .ok_or_else(|| BlobError::ObjectMissing {
                detail: object.display().to_string(),
            })?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = start.saturating_add(usize::try_from(length).unwrap_or(usize::MAX));
        if end > data.len() {
            return Err(BlobError::InvalidRequest {
                detail: "范围越界".to_owned(),
            });
        }
        Ok(StoredSlice {
            bytes: length,
            data: data[start..end].to_vec(),
        })
    }
}

fn id_bytes(marker: u64) -> [u8; ID_LEN] {
    let mut bytes = [0u8; ID_LEN];
    bytes[..8].copy_from_slice(&marker.to_be_bytes());
    bytes
}

/// 一个已提交版本（测试断言用）。
#[derive(Clone, Debug)]
pub(crate) struct StoredVersion {
    pub(crate) path: String,
    pub(crate) digest: ContentDigest,
    pub(crate) bytes: u64,
    pub(crate) revision: Revision,
    pub(crate) seq: VaultSeq,
    pub(crate) storage_path: String,
}

type StoredEntry = (EntryId, Revision, Option<(ContentDigest, u64)>);

/// 内存采纳存储：版本与事件（`events`）同事务追加。
#[derive(Default)]
pub(crate) struct FakeExternalStore {
    pub(crate) entries: BTreeMap<String, StoredEntry>,
    pub(crate) directories: BTreeSet<String>,
    pub(crate) versions: Vec<StoredVersion>,
    /// 事件批次：`(seq, 路径)`，与版本一一对应。
    pub(crate) events: Vec<(VaultSeq, String)>,
    pub(crate) writes: Vec<ExternalWrite>,
    pub(crate) moves: Vec<ExternalMove>,
    pub(crate) deletes: Vec<ExternalDelete>,
    /// 外部删除后留下的回收站记录（路径、条目、版本数）。
    pub(crate) trashed: Vec<(String, EntryId, usize)>,
    pub(crate) apply_error: Option<ExternalStoreError>,
    pub(crate) move_error: Option<ExternalStoreError>,
    pub(crate) delete_error: Option<ExternalStoreError>,
    pub(crate) lookup_error: Option<ExternalStoreError>,
    /// 把返回的证据摘要改坏（模拟违反契约的存储）。
    pub(crate) corrupt_record_digest: bool,
    counter: u64,
}

impl FakeExternalStore {
    pub(crate) fn versions_of(&self, path: &str) -> Vec<&StoredVersion> {
        self.versions
            .iter()
            .filter(|version| version.path == path)
            .collect()
    }

    /// 路径当前的条目身份（不存在的路径返回 `None`）。
    pub(crate) fn entry_of(&self, path: &str) -> Option<EntryId> {
        self.entries.get(path).map(|(entry, _, _)| *entry)
    }
}

impl ExternalAdoptStore for FakeExternalStore {
    async fn lookup_path(
        &mut self,
        _vault: VaultId,
        path: &str,
    ) -> Result<Option<KnownEntry>, ExternalStoreError> {
        if let Some(error) = self.lookup_error.take() {
            return Err(error);
        }
        if self.directories.contains(path) {
            return Ok(Some(KnownEntry {
                entry: EntryId::from_bytes(id_bytes(9_000)),
                revision: Revision::new(1).expect("合法"),
                kind: EntryKind::Directory,
                content: None,
            }));
        }
        Ok(self
            .entries
            .get(path)
            .map(|(entry, revision, content)| KnownEntry {
                entry: *entry,
                revision: *revision,
                kind: EntryKind::File,
                content: content
                    .map(|(digest, bytes)| crate::objects::ExpectedContent::new(digest, bytes)),
            }))
    }

    async fn apply_external(
        &mut self,
        write: &ExternalWrite,
    ) -> Result<ExternalRecord, ExternalStoreError> {
        self.writes.push(write.clone());
        if let Some(error) = self.apply_error.take() {
            return Err(error);
        }
        self.counter += 1;
        let (entry, revision) = match write.kind {
            AdoptKind::Create => (
                EntryId::from_bytes(id_bytes(1_000 + self.counter)),
                Revision::new(1).expect("合法"),
            ),
            AdoptKind::Update {
                entry,
                expected_revision,
            } => {
                let (_, current, _) = self.entries[&write.path];
                if current != expected_revision {
                    return Err(ExternalStoreError::RevisionChanged {
                        expected: expected_revision,
                        actual: current,
                    });
                }
                (entry, expected_revision.next().expect("可推进"))
            }
        };
        let seq = VaultSeq::new(self.counter).expect("合法");
        self.entries.insert(
            write.path.clone(),
            (
                entry,
                revision,
                Some((write.content.digest, write.content.bytes)),
            ),
        );
        self.versions.push(StoredVersion {
            path: write.path.clone(),
            digest: write.content.digest,
            bytes: write.content.bytes,
            revision,
            seq,
            storage_path: write.storage_path.clone(),
        });
        self.events.push((seq, write.path.clone()));
        let digest = if self.corrupt_record_digest {
            digest_of(b"not the content")
        } else {
            write.content.digest
        };
        Ok(ExternalRecord {
            entry,
            version: VersionId::from_bytes(id_bytes(5_000 + self.counter)),
            revision,
            seq,
            tx: TxId::from_bytes(id_bytes(7_000 + self.counter)),
            digest,
            size: write.content.bytes,
        })
    }

    async fn apply_external_move(
        &mut self,
        request: &ExternalMove,
    ) -> Result<ExternalMoveRecord, ExternalStoreError> {
        self.moves.push(request.clone());
        if let Some(error) = self.move_error.take() {
            return Err(error);
        }
        let Some((entry, revision, content)) = self.entries.get(&request.from).cloned() else {
            return Err(ExternalStoreError::Rejected {
                code: "unknown_path",
                detail: format!("{} 不在库里", request.from),
            });
        };
        if entry != request.entry {
            return Err(ExternalStoreError::Rejected {
                code: "unknown_entry",
                detail: "条目身份与请求不符".to_owned(),
            });
        }
        if revision != request.expected_revision {
            return Err(ExternalStoreError::RevisionChanged {
                expected: request.expected_revision,
                actual: revision,
            });
        }
        self.counter += 1;
        let next = revision.next().expect("可推进");
        self.entries.remove(&request.from);
        self.entries
            .insert(request.to.clone(), (entry, next, content));
        let seq = VaultSeq::new(self.counter).expect("合法");
        self.events.push((seq, request.to.clone()));
        Ok(ExternalMoveRecord {
            entry,
            revision: next,
            seq,
            tx: TxId::from_bytes(id_bytes(7_000 + self.counter)),
        })
    }

    async fn apply_external_delete(
        &mut self,
        request: &ExternalDelete,
    ) -> Result<ExternalDeleteRecord, ExternalStoreError> {
        self.deletes.push(request.clone());
        if let Some(error) = self.delete_error.take() {
            return Err(error);
        }
        let Some((entry, revision, _)) = self.entries.get(&request.path).cloned() else {
            return Err(ExternalStoreError::Rejected {
                code: "unknown_path",
                detail: format!("{} 不在库里", request.path),
            });
        };
        if entry != request.entry {
            return Err(ExternalStoreError::Rejected {
                code: "unknown_entry",
                detail: "条目身份与请求不符".to_owned(),
            });
        }
        if revision != request.expected_revision {
            return Err(ExternalStoreError::RevisionChanged {
                expected: request.expected_revision,
                actual: revision,
            });
        }
        self.counter += 1;
        let next = revision.next().expect("可推进");
        let versions = self.versions_of(&request.path).len();
        self.entries.remove(&request.path);
        self.trashed.push((request.path.clone(), entry, versions));
        let seq = VaultSeq::new(self.counter).expect("合法");
        self.events.push((seq, request.path.clone()));
        Ok(ExternalDeleteRecord {
            entry,
            revision: next,
            seq,
            tx: TxId::from_bytes(id_bytes(7_000 + self.counter)),
        })
    }
}
