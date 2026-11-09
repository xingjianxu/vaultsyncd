//! 仅测试编译的真实磁盘适配：客户端库、远端账本、上传块、固定内容与本地意图均跨进程保留。
//! 不是 Linux 客户端产品，也不是 Windows CFAPI 验收。
#[path = "safety_tests.rs"]
mod safety_tests;

use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use vaultsyncd_client_store::bindings::{self, NewBinding};
use vaultsyncd_client_store::entries::{self, LocalEntry};
use vaultsyncd_client_store::events;
use vaultsyncd_domain::ids::{DeviceId, ServerId};
use vaultsyncd_domain::state::{Dirty, Residency, Usage};
use vaultsyncd_domain::version::ServerEpoch;
use vaultsyncd_protocol::generated::{ChangeList, OperationState};
use vaultsyncd_transfer::hash::digest_of;
use vaultsyncd_transfer::ports::*;

use crate::adapters::client_store::ClientStorePorts;
use crate::engine::events::{
    ApplyResult, ChangeSource, DecodeError, EventApplier, EventDecoder, EventEnvelope,
    EventReceiver,
};
use crate::ports::local::*;
use crate::ports::remote::RemotePort;
use crate::testing::FakeClock;

const BINDING: BindingId = BindingId::from_bytes([0x11; 16]);
const VAULT: VaultId = VaultId::from_bytes([0x22; 16]);
const VAULT_ENTRY: EntryId = EntryId::from_bytes([0x01; 16]);

struct Ids(u64);
impl IdSource for Ids {
    fn next_id_bytes(&mut self) -> [u8; 16] {
        self.0 += 1;
        let mut bytes = [0xa5; 16];
        bytes[8..].copy_from_slice(&self.0.to_be_bytes());
        bytes
    }
}
struct NoNewIds;
impl IdSource for NoNewIds {
    fn next_id_bytes(&mut self) -> [u8; 16] {
        panic!("恢复不应分配新身份")
    }
}

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vaultsyncd-s058-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("测试目录");
        Self(dir)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("清理测试目录");
    }
}
fn io(error: impl std::fmt::Display) -> PortError {
    PortError::local(LocalErrorKind::Write, error.to_string())
}
fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PortError> {
    let bytes = serde_json::to_vec(value).map_err(io)?;
    let temp = path.with_extension("tmp");
    let file = fs::File::create(&temp).map_err(io)?;
    use std::io::Write;
    (&file).write_all(&bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    fs::rename(temp, path).map_err(io)
}
fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, PortError> {
    serde_json::from_slice(&fs::read(path).map_err(io)?).map_err(io)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn write_file(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().expect("父目录")).expect("父目录");
    fs::write(path, bytes).expect("写样本");
}

#[derive(Clone)]
struct DiskLocal {
    base: PathBuf,
    fault: String,
    admission: bool,
    on_demand: bool,
}
impl DiskLocal {
    fn new(base: &Path, fault: &str) -> Self {
        Self {
            base: base.to_owned(),
            fault: fault.to_owned(),
            admission: true,
            on_demand: true,
        }
    }
    fn vault(&self) -> PathBuf {
        self.base.join("local")
    }
    fn walk(&self) -> Result<Vec<LocalImportEntry>, PortError> {
        fn visit(
            vault: &Path,
            path: &Path,
            entries: &mut Vec<LocalImportEntry>,
        ) -> Result<(), PortError> {
            for child in fs::read_dir(path).map_err(io)? {
                let child = child.map_err(io)?.path();
                let meta = fs::symlink_metadata(&child).map_err(io)?;
                if !meta.is_dir() && !meta.is_file() {
                    return Err(invalid("测试扫描不支持此类型"));
                }
                let bytes = if meta.is_file() {
                    Some(fs::read(&child).map_err(io)?)
                } else {
                    None
                };
                entries.push(LocalImportEntry {
                    path: child
                        .strip_prefix(vault)
                        .map_err(io)?
                        .to_str()
                        .expect("测试 UTF-8")
                        .to_owned(),
                    kind: if meta.is_dir() {
                        WireKind::Directory
                    } else {
                        WireKind::File
                    },
                    generation: 1,
                    size: bytes.as_ref().map(|b| b.len() as u64),
                    hash: bytes.as_ref().map(|b| digest_of(b)),
                });
                if meta.is_dir() {
                    visit(vault, &child, entries)?;
                }
            }
            Ok(())
        }
        let mut entries = Vec::new();
        visit(&self.vault(), &self.vault(), &mut entries)?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
    fn signature(path: &Path) -> Result<Vec<(String, Vec<u8>)>, PortError> {
        if path.is_file() {
            return Ok(vec![(String::new(), fs::read(path).map_err(io)?)]);
        }
        fn collect(
            vault: &Path,
            current: &Path,
            out: &mut Vec<(String, Vec<u8>)>,
        ) -> Result<(), PortError> {
            for entry in fs::read_dir(current).map_err(io)? {
                let path = entry.map_err(io)?.path();
                let relative = path
                    .strip_prefix(vault)
                    .map_err(io)?
                    .to_str()
                    .expect("测试路径")
                    .to_owned();
                if path.is_file() {
                    out.push((relative, fs::read(&path).map_err(io)?));
                } else {
                    out.push((format!("{relative}/"), Vec::new()));
                    collect(vault, &path, out)?;
                }
            }
            Ok(())
        }
        let mut out = vec![("/".to_owned(), Vec::new())];
        collect(path, path, &mut out)?;
        out.sort();
        Ok(out)
    }
}
impl NameEquivalence for DiskLocal {
    fn equivalent(&self, left: &str, right: &str) -> bool {
        vaultsyncd_fs_common::name::PortableNameEquivalence.equivalent(left, right)
    }
}
impl ImportLocal for DiskLocal {
    fn validate<'a>(
        &'a self,
        _binding: &'a BindingRecord,
        _other: &'a [String],
    ) -> PortFuture<'a, Result<VaultAdmission, PortError>> {
        Box::pin(async move {
            Ok(VaultAdmission {
                identity: "test-volume:vault-inode".to_owned(),
                healthy: self.admission,
                directory: self.vault().is_dir(),
                disjoint: self.admission,
                representable: true,
            })
        })
    }
    fn scan(
        &self,
        _binding: BindingId,
    ) -> PortFuture<'_, Result<Vec<LocalImportEntry>, PortError>> {
        Box::pin(async move {
            if self.fault == "no-rescan" {
                panic!("恢复不得重新扫描本地目录");
            }
            if self.fault == "scan-error" {
                return Err(io("注入目录权限/扫描失败"));
            }
            let entries = self.walk()?;
            fs::write(self.base.join("scanned"), b"complete").map_err(io)?;
            Ok(entries)
        })
    }
    fn preserve<'a>(
        &'a self,
        _binding: BindingId,
        op: OpId,
        from: &'a RelativePath,
        to: &'a RelativePath,
        expected: Generation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            assert_eq!(expected.get(), 1);
            let from = self.vault().join(from.to_string());
            let to = self.vault().join(to.to_string());
            let intent = self.base.join("intents").join(op.to_string());
            if !intent.exists() {
                if to.exists() {
                    return Err(invalid("保留目标已存在"));
                }
                save_json(&intent, &Self::signature(&from)?)?;
            }
            let signature: Vec<(String, Vec<u8>)> = read_json(&intent)?;
            if !from.exists() && to.exists() {
                if Self::signature(&to)? != signature {
                    return Err(invalid("保留目标不是原内容"));
                }
                return Ok(());
            }
            if to.exists() || Self::signature(&from)? != signature {
                return Err(invalid("保留现场变化"));
            }
            fs::rename(from, to).map_err(io)?;
            if self.fault == "crash-preserve" {
                std::process::exit(72);
            }
            Ok(())
        })
    }
    fn install(&self, request: &ImportInstall) -> PortFuture<'_, Result<Generation, PortError>> {
        // 复制请求，避免借用临时 request 跨 future。
        let request = request.clone();
        Box::pin(async move {
            let target = self.vault().join(request.path.to_string());
            let intent = self.base.join("intents").join(request.op.to_string());
            if !intent.exists() {
                if target.exists() {
                    return Err(invalid("安装不能覆盖扫描后新建内容"));
                }
                save_json(&intent, &request.remote)?;
            }
            let pinned: SnapshotEntry = read_json(&intent)?;
            if pinned != request.remote {
                return Err(invalid("安装意图版本不符"));
            }
            if request.remote.kind == WireKind::Directory {
                if !target.exists() {
                    fs::create_dir(&target).map_err(io)?;
                }
                if !target.is_dir() {
                    return Err(invalid("安装目录目标变成文件"));
                }
            } else {
                assert_eq!(request.mode, SyncMode::Full, "磁盘验收仅做完整模式");
                let bytes = fs::read(
                    self.base
                        .join("content")
                        .join(request.remote.version_id.as_ref().expect("版本")),
                )
                .map_err(io)?;
                if request.remote.hash.as_deref() != Some(hex(&digest_of(&bytes)).as_str())
                    || request.remote.size.as_deref() != Some(bytes.len().to_string().as_str())
                {
                    return Err(invalid("固定版本摘要错误"));
                }
                if target.exists() {
                    if fs::read(&target).map_err(io)? != bytes {
                        return Err(invalid("安装重放目标变化"));
                    }
                } else {
                    let temp = intent.with_extension("content");
                    fs::write(&temp, bytes).map_err(io)?;
                    fs::rename(temp, target).map_err(io)?;
                }
            }
            if self.fault == "crash-install" {
                std::process::exit(74);
            }
            generation(1)
        })
    }
}
struct Bytes(Vec<u8>);
impl SnapshotSource for Bytes {
    fn identity(&self) -> &str {
        "disk-test-fixed-generation-1"
    }
    fn total_size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&self, offset: u64, length: u64) -> PortFuture<'_, Result<Vec<u8>, PortError>> {
        Box::pin(async move { Ok(self.0[offset as usize..(offset + length) as usize].to_vec()) })
    }
}
impl LocalPort for DiskLocal {
    fn capabilities(&self) -> LocalCapabilities {
        let mut caps = LocalCapabilities::full();
        caps.on_demand_read = self.on_demand;
        caps
    }
    fn observe(&self, path: &RelativePath) -> PortFuture<'_, Result<LocalFacts, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            let meta = fs::metadata(self.vault().join(path.to_string())).map_err(io)?;
            Ok(LocalFacts {
                path,
                kind: if meta.is_file() {
                    LocalEntryKind::File
                } else {
                    LocalEntryKind::Directory
                },
                generation: generation(1)?,
                size: if meta.is_file() { meta.len() } else { 0 },
                file_identity: None,
                usage: Usage::Idle,
            })
        })
    }
    fn snapshot(
        &self,
        path: &RelativePath,
        expected: Generation,
    ) -> PortFuture<'_, Result<Arc<dyn SnapshotSource>, PortError>> {
        let path = path.clone();
        Box::pin(async move {
            assert_eq!(expected.get(), 1);
            Ok(Arc::new(Bytes(
                fs::read(self.vault().join(path.to_string())).map_err(io)?,
            )) as Arc<dyn SnapshotSource>)
        })
    }
    fn apply(&self, _: &ApplyRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { Err(invalid("不应调用通用 apply")) })
    }
    fn move_entry(&self, _: &MoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        Box::pin(async { Err(invalid("不应调用通用 move")) })
    }
    fn remove(&self, _: &RemoveRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        panic!("首次合并绝不删除")
    }
    fn release(&self, _: &ReleaseRequest) -> PortFuture<'_, Result<ApplyReceipt, PortError>> {
        panic!("首次合并绝不释放")
    }
}

#[derive(Serialize, Deserialize)]
struct RemoteDisk {
    entries: Vec<SnapshotEntry>,
    results: BTreeMap<String, OperationResult>,
    requests: Vec<OperationRequest>,
    seq: u64,
}
#[derive(Serialize, Deserialize)]
struct SessionDisk {
    size: u64,
    digest: [u8; 32],
    chunk_size: u64,
    finalized: bool,
}
#[derive(Clone)]
struct DiskRemote {
    base: PathBuf,
    fault: String,
}
impl DiskRemote {
    fn state(&self) -> Result<RemoteDisk, PortError> {
        read_json(&self.base.join("server.json"))
    }
    fn save(&self, state: &RemoteDisk) -> Result<(), PortError> {
        save_json(&self.base.join("server.json"), state)
    }
    fn session(&self, id: &str) -> Result<SessionDisk, PortError> {
        read_json(&self.base.join("uploads").join(format!("{id}.json")))
    }
    fn info(&self, id: &str) -> Result<UploadSessionInfo, PortError> {
        let s = self.session(id)?;
        Ok(UploadSessionInfo {
            id: id.to_owned(),
            chunk_size: s.chunk_size,
            part_count: 1,
            total_size: s.size,
            digest: s.digest,
            state: if s.finalized {
                UploadSessionState::Finalized
            } else {
                UploadSessionState::Open
            },
        })
    }
}
impl ImportSnapshot for DiskRemote {
    fn begin(&self, vault: VaultId) -> PortFuture<'_, Result<SnapshotSeed, PortError>> {
        Box::pin(async move {
            assert_eq!(vault, VAULT);
            let pinned = self.base.join("pinned.json");
            if pinned.exists() {
                panic!("恢复不应重新获取快照");
            }
            let state = self.state()?;
            save_json(&pinned, &state.entries)?;
            Ok(SnapshotSeed {
                metadata: Snapshot {
                    entry_count: state.entries.len().to_string(),
                    expires_at: "1900000000".to_owned(),
                    high_water_seq: state.seq.to_string(),
                    vault_id: vault.to_string(),
                    server_epoch: "3".to_owned(),
                    snapshot_id: EntryId::from_bytes([0x77; 16]).to_string(),
                },
                server_time: UtcTimestamp::from_unix_seconds(1899990000).expect("服务器时间"),
            })
        })
    }
    fn page<'a>(
        &'a self,
        _: VaultId,
        snapshot: &'a str,
        cursor: Option<&'a str>,
    ) -> PortFuture<'a, Result<SnapshotPage, PortError>> {
        Box::pin(async move {
            if self.fault == "crash-page" && cursor.is_some() {
                std::process::exit(71);
            }
            let entries: Vec<SnapshotEntry> = read_json(&self.base.join("pinned.json"))?;
            let start = cursor.map(number).transpose()?.unwrap_or(0) as usize;
            let end = (start + 2).min(entries.len());
            Ok(SnapshotPage {
                entries: entries[start..end].to_vec(),
                high_water_seq: "7".to_owned(),
                next_cursor: if end < entries.len() {
                    Some(end.to_string())
                } else {
                    None
                },
                snapshot_id: snapshot.to_owned(),
            })
        })
    }
}
impl RemotePort for DiskRemote {
    fn submit(
        &self,
        request: &OperationRequest,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        let request = request.clone();
        Box::pin(async move {
            assert_eq!(
                request.kind,
                OperationKind::Create,
                "首次合并不能发 put/delete/move"
            );
            assert_eq!(request.expected_absent, Some(true), "上传必须保留 CAS");
            assert_eq!(request.server_epoch, "3");
            assert!(request.base_revision.is_none());
            let mut state = self.state()?;
            if let Some(result) = state.results.get(&request.op_id) {
                return Ok(result.clone());
            }
            let parent = request.parent_id.as_ref().expect("父身份");
            if state.entries.iter().any(|entry| {
                entry.parent_id.as_ref() == Some(parent)
                    && Some(&entry.name) == request.name.as_ref()
            }) {
                return Err(PortError::rejected(
                    "BASE_MISMATCH",
                    "expected_absent CAS 拒绝并发创建",
                ));
            }
            let next = (0x90 + state.results.len()) as u8;
            let id = EntryId::from_bytes([next; 16]).to_string();
            let version = if request.upload_id.is_some() {
                Some(EntryId::from_bytes([next + 20; 16]).to_string())
            } else {
                None
            };
            let bytes = request
                .upload_id
                .as_ref()
                .map(|id| {
                    fs::read(self.base.join("uploads").join(format!("{id}.part"))).map_err(io)
                })
                .transpose()?;
            if let (Some(version), Some(bytes)) = (&version, &bytes) {
                fs::write(self.base.join("content").join(version), bytes).map_err(io)?;
            }
            state.seq += 1;
            let result = OperationResult {
                conflict: None,
                echo_local_generation: request.local_generation.clone(),
                entry_id: Some(id.clone()),
                error_code: None,
                error_message: None,
                op_id: request.op_id.clone(),
                replayed: Some(false),
                revision: Some("1".to_owned()),
                vault_seq: Some(state.seq.to_string()),
                state: OperationState::Committed,
                version_id: version.clone(),
            };
            state.entries.push(SnapshotEntry {
                modified_at: None,
                entry_id: id,
                hash: bytes.as_ref().map(|b| hex(&digest_of(b))),
                kind: if bytes.is_some() {
                    WireKind::File
                } else {
                    WireKind::Directory
                },
                name: request.name.clone().expect("名称"),
                parent_id: Some(parent.clone()),
                revision: "1".to_owned(),
                size: bytes.as_ref().map(|b| b.len().to_string()),
                version_id: version,
            });
            state.results.insert(request.op_id.clone(), result.clone());
            state.requests.push(request);
            self.save(&state)?;
            if self.fault == "crash-ack" {
                std::process::exit(73);
            }
            Ok(result)
        })
    }
    fn operation(
        &self,
        _: BindingId,
        op: OpId,
    ) -> PortFuture<'_, Result<OperationResult, PortError>> {
        Box::pin(async move {
            self.state()?
                .results
                .get(&op.to_string())
                .cloned()
                .ok_or_else(|| invalid("操作未知"))
        })
    }
    fn upload(&self, _: BindingId) -> PortFuture<'_, Result<Arc<dyn UploadTarget>, PortError>> {
        Box::pin(async move { Ok(Arc::new(self.clone()) as Arc<dyn UploadTarget>) })
    }
    fn content(
        &self,
        _: BindingId,
    ) -> PortFuture<'_, Result<Arc<dyn VersionedContent>, PortError>> {
        Box::pin(async { Err(invalid("安装按固定版本文件读取")) })
    }
}
impl UploadTarget for DiskRemote {
    fn begin<'a>(
        &'a self,
        spec: &'a SessionSpec,
    ) -> PortFuture<'a, Result<UploadSessionInfo, PortError>> {
        Box::pin(async move {
            let id = hex(&spec.digest[..16]);
            save_json(
                &self.base.join("uploads").join(format!("{id}.json")),
                &SessionDisk {
                    size: spec.total_size,
                    digest: spec.digest,
                    chunk_size: spec.chunk_size,
                    finalized: false,
                },
            )?;
            self.info(&id)
        })
    }
    fn status<'a>(&'a self, id: &'a str) -> PortFuture<'a, Result<UploadProgressInfo, PortError>> {
        Box::pin(async move {
            let session = self.info(id)?;
            let p = self.base.join("uploads").join(format!("{id}.part"));
            let received = if p.exists() {
                let b = fs::read(p).map_err(io)?;
                vec![ReceivedPart {
                    index: 0,
                    length: b.len() as u64,
                    digest: digest_of(&b),
                }]
            } else {
                vec![]
            };
            let missing = if received.is_empty() {
                vec![vaultsyncd_transfer::bitmap::PartRange::new(0, 1).expect("合法测试块区间")]
            } else {
                vec![]
            };
            Ok(UploadProgressInfo {
                session,
                received,
                missing,
            })
        })
    }
    fn put_part<'a>(
        &'a self,
        id: &'a str,
        index: u64,
        digest: &'a [u8; 32],
        bytes: &'a [u8],
    ) -> PortFuture<'a, Result<PartAck, PortError>> {
        Box::pin(async move {
            assert_eq!(index, 0);
            assert_eq!(&digest_of(bytes), digest);
            fs::write(self.base.join("uploads").join(format!("{id}.part")), bytes).map_err(io)?;
            Ok(PartAck {
                index,
                length: bytes.len() as u64,
                digest: *digest,
                already_present: false,
            })
        })
    }
    fn finalize<'a>(&'a self, id: &'a str) -> PortFuture<'a, Result<FinalizedContent, PortError>> {
        Box::pin(async move {
            let mut s = self.session(id)?;
            let bytes =
                fs::read(self.base.join("uploads").join(format!("{id}.part"))).map_err(io)?;
            assert_eq!(digest_of(&bytes), s.digest);
            assert_eq!(bytes.len() as u64, s.size);
            let replayed = s.finalized;
            s.finalized = true;
            save_json(&self.base.join("uploads").join(format!("{id}.json")), &s)?;
            Ok(FinalizedContent {
                digest: s.digest,
                size: s.size,
                part_count: 1,
                replayed,
                storage_path: format!("objects/{id}"),
            })
        })
    }
}

async fn setup(base: &Path) -> Arc<Database> {
    for dir in ["local", "uploads", "content", "intents"] {
        fs::create_dir_all(base.join(dir)).expect("目录");
    }
    write_file(&base.join("local/a-local.txt"), b"local only");
    write_file(&base.join("local/shared/same.txt"), b"identical");
    write_file(&base.join("local/shared/different.txt"), b"local branch");
    write_file(&base.join("local/kind/child.txt"), b"local directory child");
    write_file(
        &base.join("local/file-kind"),
        b"local file at remote directory",
    );
    let vault = SnapshotEntry {
        modified_at: None,
        entry_id: VAULT_ENTRY.to_string(),
        hash: None,
        kind: WireKind::Directory,
        name: String::new(),
        parent_id: None,
        revision: "1".to_owned(),
        size: None,
        version_id: None,
    };
    let mut entries = vec![vault];
    let mut add = |marker: u8, parent: EntryId, name: &str, bytes: Option<&[u8]>| {
        let version = bytes.map(|_| EntryId::from_bytes([marker + 0x40; 16]).to_string());
        if let (Some(version), Some(bytes)) = (&version, bytes) {
            write_file(&base.join("content").join(version), bytes);
        }
        let id = EntryId::from_bytes([marker; 16]);
        entries.push(SnapshotEntry {
            modified_at: None,
            entry_id: id.to_string(),
            hash: bytes.map(|b| hex(&digest_of(b))),
            kind: if bytes.is_some() {
                WireKind::File
            } else {
                WireKind::Directory
            },
            name: name.to_owned(),
            parent_id: Some(parent.to_string()),
            revision: "1".to_owned(),
            size: bytes.map(|b| b.len().to_string()),
            version_id: version,
        });
        id
    };
    let shared = add(2, VAULT_ENTRY, "shared", None);
    add(3, shared, "same.txt", Some(b"identical"));
    add(4, shared, "different.txt", Some(b"server branch"));
    add(5, VAULT_ENTRY, "z-remote.txt", Some(b"remote only"));
    add(
        6,
        VAULT_ENTRY,
        "kind",
        Some(b"remote file at local directory"),
    );
    let directory = add(7, VAULT_ENTRY, "file-kind", None);
    add(8, directory, "child.txt", Some(b"remote directory child"));
    save_json(
        &base.join("server.json"),
        &RemoteDisk {
            entries,
            results: BTreeMap::new(),
            requests: Vec::new(),
            seq: 7,
        },
    )
    .expect("远端账本");
    let database = Arc::new(
        Database::open(base.join("client.sqlite"))
            .await
            .expect("客户端库"),
    );
    let mut tx = database.begin_write().await.expect("事务");
    bindings::insert_binding(
        &mut tx,
        &NewBinding {
            id: BINDING,
            server: ServerId::from_bytes([0x33; 16]),
            device: DeviceId::from_bytes([0x44; 16]),
            vault: VAULT,
            local_path: base.join("local").to_str().expect("路径").to_owned(),
            cache_path: base.join("cache").to_str().expect("路径").to_owned(),
            mode: SyncMode::Full,
            rules_version: "1".to_owned(),
            server_epoch: ServerEpoch::new(3).expect("epoch"),
        },
    )
    .await
    .expect("绑定");
    tx.commit().await.expect("提交");
    database
}
fn runner(database: Arc<Database>, local: Arc<DiskLocal>, remote: Arc<DiskRemote>) -> FirstImport {
    let store = Arc::new(ClientStorePorts::new(database.clone()));
    let ports = Ports {
        local: local.clone(),
        clock: Arc::new(FakeClock::at(1)),
        remote: remote.clone(),
        operations: store.clone(),
        intents: store.clone(),
        upload_resumes: store,
    };
    FirstImport::new(database, ports, local, remote)
}
fn start_child(base: &Path, stage: &str, expected: i32) {
    let status = Command::new(std::env::current_exe().expect("测试程序"))
        .args([
            "--exact",
            "bindings::import::tests::durable_child",
            "--nocapture",
        ])
        .env("VAULTSYNCD_S058_TEST_DIR", base)
        .env("VAULTSYNCD_S058_TEST_STAGE", stage)
        .status()
        .expect("启动独立子进程");
    assert_eq!(status.code(), Some(expected), "阶段 {stage}");
}
#[tokio::test]
async fn durable_child() {
    let Ok(base) = std::env::var("VAULTSYNCD_S058_TEST_DIR") else {
        return;
    };
    let base = PathBuf::from(base);
    let stage = std::env::var("VAULTSYNCD_S058_TEST_STAGE").expect("阶段");
    let database = Arc::new(
        Database::open(base.join("client.sqlite"))
            .await
            .expect("重开库"),
    );
    let local = Arc::new(DiskLocal::new(
        &base,
        if stage == "manifest" {
            ""
        } else if stage == "resume" {
            "no-rescan"
        } else {
            &stage
        },
    ));
    let remote = Arc::new(DiskRemote {
        base: base.clone(),
        fault: stage.clone(),
    });
    let runner = runner(database.clone(), local, remote);
    let result = if stage == "manifest" || stage == "crash-page" {
        runner.run(BINDING, &mut Ids(0), 0).await
    } else {
        runner.run(BINDING, &mut NoNewIds, usize::MAX).await
    };
    let report = result.expect("阶段推进");
    assert_eq!(report.high_water.get(), 7);
    assert_eq!(report.complete, stage == "resume");
    drop(runner);
    Arc::try_unwrap(database).expect("独占数据库").close().await;
}

#[tokio::test]
async fn nonempty_directories_resume_across_real_process_deaths_without_deletion() {
    let dir = TestDir::new();
    let database = setup(&dir.0).await;
    Arc::try_unwrap(database).expect("独占库").close().await;
    start_child(&dir.0, "crash-page", 71);
    start_child(&dir.0, "manifest", 0);
    let before: Vec<SnapshotEntry> = read_json(&dir.0.join("pinned.json")).expect("快照");
    start_child(&dir.0, "crash-ack", 73);
    start_child(&dir.0, "crash-preserve", 72);
    start_child(&dir.0, "crash-install", 74);
    start_child(&dir.0, "resume", 0);
    let database = Arc::new(
        Database::open(dir.0.join("client.sqlite"))
            .await
            .expect("检查库"),
    );
    let store = ImportStore::new(database.clone());
    let journal = store.load(BINDING).await.expect("读日志").expect("日志");
    assert!(journal.complete);
    assert_eq!(journal.entries, before, "恢复仍使用同一快照");
    let state: RemoteDisk = read_json(&dir.0.join("server.json")).expect("读远端");
    assert!(
        state.seq > 7,
        "上传提交了后续事件，不能用最新 vault_seq 跳过高水位"
    );
    assert_eq!(
        state.requests.len(),
        5,
        "同内容不上传；两种类型冲突与内容冲突各只创建一次"
    );
    assert_eq!(
        state
            .requests
            .iter()
            .map(|r| r.op_id.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        5,
        "重启不生成重复副本"
    );
    for entry in &before {
        assert!(state.entries.contains(entry), "没有删除或覆盖原服务器条目");
    }
    let paths = remote_paths(&state.entries).expect("远端路径");
    for item in journal.items.as_ref().expect("清单") {
        if let Some(local) = &item.local {
            let target = dir.0.join("local").join(&item.target);
            assert!(target.exists(), "本地内容保留：{}", item.target);
            if let Some(hash) = local.hash {
                assert_eq!(digest_of(&fs::read(target).expect("本地字节")), hash);
            }
            assert!(paths.contains_key(&item.target), "远端有导入条目");
        }
        if let Some(remote) = &item.remote {
            let target = dir.0.join("local").join(&item.path);
            assert!(target.exists());
            if let Some(hash) = &remote.hash {
                assert_eq!(
                    &hex(&digest_of(&fs::read(target).expect("远端安装字节"))),
                    hash
                );
            }
        }
    }
    let map_count: i64 = sqlx::query_scalar("SELECT count(*) FROM import_id_map")
        .fetch_one(database.pool())
        .await
        .expect("身份映射");
    assert_eq!(map_count, 7, "所有本地临时身份都有稳定映射");
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_ops")
        .fetch_one(database.pool())
        .await
        .expect("队列");
    assert_eq!(pending, 0);
    let mut conn = database.pool().acquire().await.expect("连接");
    assert_eq!(
        events::cursors(&mut conn, BINDING)
            .await
            .expect("游标")
            .received
            .get(),
        7
    );
    drop(conn);
    let catchup = Arc::new(safety_tests::DiskCatchUp(dir.0.clone()));
    let receiver = EventReceiver::new(
        database.clone(),
        VAULT,
        BINDING,
        catchup.clone(),
        catchup.clone(),
        catchup,
        20,
    )
    .expect("接收器");
    let caught = receiver
        .run_cycle(None)
        .await
        .expect("从原高水位拉取并接收真实后续批次");
    assert_eq!((caught.received, caught.applied), (5, 5));
    let mut conn = database.pool().acquire().await.expect("连接");
    let cursors = events::cursors(&mut conn, BINDING)
        .await
        .expect("追赶后游标");
    assert_eq!(cursors.received.get(), state.seq);
    assert_eq!(cursors.applied, cursors.received);
    drop(conn);
    drop(receiver);
    drop(store);
    Arc::try_unwrap(database).expect("独占库").close().await;
}
struct AfterSeven;
impl ChangeSource for AfterSeven {
    fn pull(
        &self,
        vault: VaultId,
        after: VaultSeq,
        _: u32,
    ) -> PortFuture<'_, Result<ChangeList, PortError>> {
        Box::pin(async move {
            assert_eq!(after.get(), 7, "不能丢掉快照期间的新事件");
            Ok(ChangeList {
                batches: vec![],
                has_more: false,
                head_seq: "7".to_owned(),
                next_after: after.to_decimal(),
                vault_id: vault.to_string(),
                server_epoch: "3".to_owned(),
            })
        })
    }
}
struct NeverDecode;
impl EventDecoder for NeverDecode {
    fn decode(
        &self,
        _: BindingId,
        _: &EventEnvelope,
    ) -> Result<vaultsyncd_client_store::events::IncomingBatch, DecodeError> {
        panic!("无事件无需解码")
    }
}
struct NeverApply;
impl EventApplier for NeverApply {
    fn apply(
        &self,
        _: BindingId,
        _: &[vaultsyncd_client_store::entries::RemoteEntry],
        _: &[vaultsyncd_client_store::queue::NewPendingOp],
    ) -> PortFuture<'_, Result<ApplyResult, PortError>> {
        panic!("无事件无需应用")
    }
}

#[tokio::test]
async fn admission_mode_scan_and_existing_baseline_fail_closed() {
    let dir = TestDir::new();
    let database = setup(&dir.0).await;
    let remote = Arc::new(DiskRemote {
        base: dir.0.clone(),
        fault: String::new(),
    });
    let mut local = DiskLocal::new(&dir.0, "");
    local.admission = false;
    assert!(
        runner(database.clone(), Arc::new(local), remote.clone())
            .run(BINDING, &mut Ids(0), 0)
            .await
            .is_err()
    );
    assert!(!dir.0.join("pinned.json").exists(), "准入失败不取快照");
    sqlx::query("UPDATE bindings SET mode='on_demand'")
        .execute(database.pool())
        .await
        .expect("模式");
    let mut local = DiskLocal::new(&dir.0, "");
    local.on_demand = false;
    assert!(
        runner(database.clone(), Arc::new(local), remote.clone())
            .run(BINDING, &mut Ids(0), 0)
            .await
            .is_err()
    );
    assert!(!dir.0.join("pinned.json").exists());
    sqlx::query("UPDATE bindings SET mode='full'")
        .execute(database.pool())
        .await
        .expect("模式");
    let import = runner(
        database.clone(),
        Arc::new(DiskLocal::new(&dir.0, "scan-error")),
        remote.clone(),
    );
    assert!(import.run(BINDING, &mut Ids(0), 0).await.is_err());
    assert!(
        import
            .store
            .load(BINDING)
            .await
            .expect("日志")
            .expect("固定快照已保存")
            .items
            .is_none()
    );
    let receiver = EventReceiver::new(
        database.clone(),
        VAULT,
        BINDING,
        Arc::new(AfterSeven),
        Arc::new(NeverDecode),
        Arc::new(NeverApply),
        20,
    )
    .expect("接收器");
    assert!(receiver.run_cycle(None).await.is_err(), "未导入完不接事件");
    drop(receiver);
    drop(import);
    let state = remote.state().expect("远端");
    assert!(state.requests.is_empty(), "不完整扫描不会推导操作或删除");
    Arc::try_unwrap(database).expect("独占库").close().await;
    let second = TestDir::new();
    let database = setup(&second.0).await;
    let mut tx = database.begin_write().await.expect("事务");
    entries::put_baseline(
        &mut tx,
        BINDING,
        &baseline(&state.entries[0]).expect("根基线"),
    )
    .await
    .expect("旧基线");
    tx.commit().await.expect("提交");
    let import = runner(
        database.clone(),
        Arc::new(DiskLocal::new(&second.0, "")),
        Arc::new(DiskRemote {
            base: second.0.clone(),
            fault: String::new(),
        }),
    );
    assert!(
        import.run(BINDING, &mut Ids(0), 0).await.is_err(),
        "旧基线必须重建不能初次合并"
    );
    drop(import);
    assert!(!second.0.join("pinned.json").exists());
    Arc::try_unwrap(database).expect("独占库").close().await;
}

#[tokio::test]
async fn committed_ack_maps_id_without_clearing_new_generation_and_invalid_ack_rolls_back() {
    let dir = TestDir::new();
    let database = setup(&dir.0).await;
    let import = runner(
        database.clone(),
        Arc::new(DiskLocal::new(&dir.0, "")),
        Arc::new(DiskRemote {
            base: dir.0.clone(),
            fault: String::new(),
        }),
    );
    import.run(BINDING, &mut Ids(0), 0).await.expect("保存清单");
    let journal = import
        .store
        .load(BINDING)
        .await
        .expect("日志")
        .expect("日志");
    let item = journal
        .items
        .as_ref()
        .expect("清单")
        .iter()
        .find(|i| i.path == "a-local.txt")
        .expect("上传项");
    let temporary = entry_id(item.temporary.as_ref().expect("临时身份")).expect("身份");
    let stable = EntryId::from_bytes([0xee; 16]);
    let mut result = OperationResult {
        conflict: None,
        echo_local_generation: Some("2".to_owned()),
        entry_id: Some(stable.to_string()),
        error_code: None,
        error_message: None,
        op_id: item.op.clone(),
        replayed: None,
        revision: Some("1".to_owned()),
        vault_seq: Some("8".to_owned()),
        state: OperationState::Committed,
        version_id: Some(EntryId::from_bytes([0xef; 16]).to_string()),
    };
    assert!(
        import
            .store
            .ack(BINDING, op_id(&item.op).expect("操作"), &result)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM import_id_map")
        .fetch_one(database.pool())
        .await
        .expect("映射");
    assert_eq!(count, 0, "坏 ACK 整笔回滚");
    let mut tx = database.begin_write().await.expect("事务");
    entries::put_local(
        &mut tx,
        BINDING,
        &LocalEntry {
            entry: temporary,
            generation: generation(2).expect("代际"),
            dirty: Dirty::Dirty,
            residency: Residency::Materialized,
            fingerprint: None,
            file_identity: None,
        },
    )
    .await
    .expect("后续修改");
    tx.commit().await.expect("提交");
    result.echo_local_generation = Some("1".to_owned());
    assert_eq!(
        import
            .store
            .ack(BINDING, op_id(&item.op).expect("操作"), &result)
            .await
            .expect("旧 ACK"),
        crate::executor::upload::UploadCommitDisposition::StaleGeneration
    );
    assert_eq!(
        import
            .store
            .ack(BINDING, op_id(&item.op).expect("操作"), &result)
            .await
            .expect("重放旧 ACK"),
        crate::executor::upload::UploadCommitDisposition::StaleGeneration
    );
    let mut conn = database.pool().acquire().await.expect("连接");
    let local = entries::local_entry(&mut conn, BINDING, stable)
        .await
        .expect("稳定身份")
        .expect("本地事实");
    assert_eq!(local.generation.get(), 2);
    assert_eq!(local.dirty, Dirty::Dirty);
    assert!(
        entries::baseline_entry(&mut conn, BINDING, stable)
            .await
            .expect("旧基线确认")
            .is_some()
    );
    drop(conn);
    drop(import);
    Arc::try_unwrap(database).expect("独占库").close().await;
}

#[test]
fn malformed_duplicate_and_native_colliding_inventory_is_rejected() {
    let binding = BindingRecord {
        id: BINDING,
        server: ServerId::from_bytes([3; 16]),
        device: DeviceId::from_bytes([4; 16]),
        vault: VAULT,
        local_path: "ignored".to_owned(),
        cache_path: "other".to_owned(),
        mode: SyncMode::Full,
        rules_version: "1".to_owned(),
        server_epoch: ServerEpoch::new(3).expect("epoch"),
    };
    let bad = LocalImportEntry {
        path: "../escape".to_owned(),
        kind: WireKind::File,
        generation: 1,
        size: Some(1),
        hash: Some([0; 32]),
    };
    let vault = SnapshotEntry {
        modified_at: None,
        entry_id: VAULT_ENTRY.to_string(),
        hash: None,
        kind: WireKind::Directory,
        name: String::new(),
        parent_id: None,
        revision: "1".to_owned(),
        size: None,
        version_id: None,
    };
    let time = UtcTimestamp::from_unix_seconds(1).expect("时间");
    let names = vaultsyncd_fs_common::name::PortableNameEquivalence;
    assert!(
        plan_manifest(
            &binding,
            std::slice::from_ref(&vault),
            vec![bad],
            time,
            &names,
            &mut Ids(0)
        )
        .is_err()
    );
    let entry = LocalImportEntry {
        path: "report.txt".to_owned(),
        kind: WireKind::File,
        generation: 1,
        size: Some(1),
        hash: Some([0; 32]),
    };
    assert!(
        plan_manifest(
            &binding,
            std::slice::from_ref(&vault),
            vec![entry.clone(), entry.clone()],
            time,
            &names,
            &mut Ids(0)
        )
        .is_err()
    );
    let mut capitalized = entry.clone();
    capitalized.path = "REPORT.txt".to_owned();
    assert!(
        plan_manifest(
            &binding,
            std::slice::from_ref(&vault),
            vec![entry, capitalized],
            time,
            &names,
            &mut Ids(0)
        )
        .is_err()
    );
    let mut cycle = vault.clone();
    cycle.parent_id = Some(cycle.entry_id.clone());
    cycle.name = "cycle".to_owned();
    assert!(remote_paths(&[vault.clone(), cycle]).is_err());
    let mut malformed_vault = vault;
    malformed_vault.version_id = Some(EntryId::from_bytes([0x99; 16]).to_string());
    assert!(remote_paths(&[malformed_vault]).is_err());
}

#[tokio::test]
async fn install_checkpoint_preserves_controlled_apply_proof_and_newer_dirty_generation() {
    for dirty in [Dirty::Clean, Dirty::Dirty, Dirty::AwaitingCommit] {
        let dir = TestDir::new();
        let database = setup(&dir.0).await;
        let import = runner(
            database.clone(),
            Arc::new(DiskLocal::new(&dir.0, "")),
            Arc::new(DiskRemote {
                base: dir.0.clone(),
                fault: String::new(),
            }),
        );
        import.run(BINDING, &mut Ids(0), 0).await.expect("持久清单");
        let mut journal = import.store.load(BINDING).await.unwrap().unwrap();
        let index = journal
            .items
            .as_ref()
            .unwrap()
            .iter()
            .position(|item| item.path == "z-remote.txt")
            .unwrap();
        let remote = journal.items.as_ref().unwrap()[index]
            .remote
            .clone()
            .unwrap();
        let base = baseline(&remote).unwrap();
        let row = LocalEntry {
            entry: base.entry,
            generation: generation(if dirty == Dirty::Clean { 1 } else { 2 }).unwrap(),
            dirty,
            residency: Residency::Materialized,
            fingerprint: Some(vec![0x59; 32]),
            file_identity: Some(vec![0x23; 16]),
        };
        let mut tx = database.begin_write().await.unwrap();
        entries::put_baseline(&mut tx, BINDING, &base)
            .await
            .unwrap();
        entries::put_local(&mut tx, BINDING, &row).await.unwrap();
        tx.commit().await.unwrap();
        let binding = import.store.binding(BINDING).await.unwrap();
        import
            .store
            .finish_item(&binding, &mut journal, index, Some(generation(1).unwrap()))
            .await
            .unwrap();
        let mut conn = database.pool().acquire().await.unwrap();
        assert_eq!(
            entries::local_entry(&mut conn, BINDING, base.entry)
                .await
                .unwrap(),
            Some(row)
        );
        drop(conn);
        drop(import);
        Arc::try_unwrap(database).unwrap().close().await;
    }
}
