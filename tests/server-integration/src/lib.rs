//! S037 崩溃注入装置：文件型假端口 + 在每一持久化边界强杀提交子进程（design §21.2、T23）。
//!
//! 本包是 `tests/server-integration/`（design §5.8）的第一个用例，只放**测试装置与验收**，不实现
//! 生产端口：`crates/server-core` 的真实 SQLite/文件适配器仍未落地（S035/S036 的限制），因此这里用
//! 一个把状态写进普通文件的假端口，好让“杀掉进程后重启”读到的是真正的磁盘证据，而不是内存残留。
//!
//! 装置由三部分组成：
//!
//! - [`CrashStore`]：`state/` 下的追加式日志（意图、版本、条目 revision、对象）与 `work/` 下的工作
//!   文件；每个持久化动作前后调用 [`CrashStore::boundary`]，到点后把边界名写进
//!   `state/boundary.txt` 并挂起，等父进程用 `SIGKILL` 强杀（超时自 abort，避免测试挂死）；
//! - [`StoreHandle`] + [`LogPort`]/[`HostTargetPort`]/[`JournalStorePort`]：把同一个文件型状态拆成
//!   `IntentLog`/`TargetPort`/`PutCommitStore` 三个端口，供 `commit_put` 与 `recover_root` 使用
//!   （提交路径需要三个可同时可变借用的端口）；
//! - [`run_commit`]/[`run_recovery`]：子进程与父进程共用的两条驱动路径。
//!
//! 摘要不是真实 SHA-256（本卡不改对象存储层）：[`digest_of`] 由内容确定性派生 32 字节，只用于在
//! 日志与断言里区分“前一个内容 / 后一个内容 / 第三种内容”。工作文件的 inode 用真实元数据，因此
//! “发布换了一个新 inode”与 `expected_before` 的 inode 比较都是真的。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use std::os::unix::fs::MetadataExt;

use vaultsyncd_domain::ids::{EntryId, IntentId, RootId, TxId, VersionId};
use vaultsyncd_domain::version::{Revision, RootSeq, ServerEpoch, UtcTimestamp};

use vaultsyncd_server_core::commit::intent::{
    NewPutIntent, PutIntent, PutPhase, TargetAfter, TargetBefore, TargetSnapshot,
};
use vaultsyncd_server_core::commit::put::{CommittedPut, PutRequest, RootCommitGate, commit_put};
use vaultsyncd_server_core::objects::{ContentDigest, ExpectedContent};
use vaultsyncd_server_core::ports::intent_log::{IntentLog, IntentLogError};
use vaultsyncd_server_core::ports::object_store::{FileKey, StorageFailure};
use vaultsyncd_server_core::ports::put_store::{
    EntryBaseline, EntryKind, ObjectState, PutCommitStore, PutRecord, PutStoreError, PutWrite,
    RootFacts,
};
use vaultsyncd_server_core::ports::target::{
    PublishRequest, PublishedTarget, TargetError, TargetObservation, TargetPort,
};
use vaultsyncd_server_core::recovery::startup::{
    RecoveryError, RecoveryReport, WriteAdmission, recover_root,
};

/// 固定 Root（测试装置只服务一个 Root）。
pub const ROOT: RootId = RootId::from_bytes([0x21; 16]);
/// 固定条目。
pub const ENTRY: EntryId = EntryId::from_bytes([0x51; 16]);
/// 固定父目录条目。
pub const PARENT: EntryId = EntryId::from_bytes([0x61; 16]);
/// 固定意图身份（一次只跑一条意图）。
pub const INTENT: IntentId = IntentId::from_bytes([0x41; 16]);
/// 目标在 Root 内的相对路径。
pub const TARGET_PATH: &str = "note.txt";
/// 暂存文件名。
pub const STAGING: &str = "put-0041";
/// 目标文件初始名称。
pub const NAME: &str = "note.txt";
/// 服务器 epoch。
pub const EPOCH: u64 = 3;
/// 条目初始 revision。
pub const SEED_REVISION: u64 = 7;
/// 工作文件原内容（“前”一个版本）。
pub const OLD_CONTENT: &[u8] = b"old-content";
/// 提交想发布的内容（“后”一个版本）。
pub const NEW_CONTENT: &[u8] = b"new-content-v2";
/// 崩溃后由外部程序写入的第三种内容。
pub const EXTERNAL_CONTENT: &[u8] = b"external!";

/// 由内容确定性派生 32 字节摘要（假端口，不是 SHA-256；见模块文档）。
#[must_use]
pub fn digest_of(content: &[u8]) -> ContentDigest {
    let mut bytes = [0u8; 32];
    for (index, byte) in content.iter().enumerate() {
        bytes[index % 32] = bytes[index % 32]
            .wrapping_add(*byte)
            .wrapping_add(u8::try_from(index).unwrap_or(0));
    }
    bytes[0] = u8::try_from(content.len()).expect("测试内容短于 256 字节");
    ContentDigest::from_bytes(bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn hex_decode(text: &str) -> Vec<u8> {
    let digits = text.as_bytes();
    digits
        .chunks(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16).expect("十六进制高位");
            let low = char::from(pair[1]).to_digit(16).expect("十六进制低位");
            u8::try_from(high * 16 + low).expect("字节范围")
        })
        .collect()
}

/// 一次持久化边界：先落 `state/boundary.txt`（父进程据此确认到达），命中注入点时挂起等强杀。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    /// 写 `prepared` 意图之前。
    BeforeIntentPrepared,
    /// 写 `prepared` 意图之后。
    AfterIntentPrepared,
    /// 发布工作文件之前。
    BeforePublish,
    /// 发布工作文件之后。
    AfterPublish,
    /// 记 `published` 之前。
    BeforeAdvancePublished,
    /// 记 `published` 之后。
    AfterAdvancePublished,
    /// 元数据事务提交之前。
    BeforeMetadataCommit,
    /// 元数据事务提交之后（意图已由同一事务收尾）。
    AfterMetadataCommit,
    /// 元数据事务已提交、响应还没发出。
    BeforeAck,
}

impl Boundary {
    /// 稳定短名（同时是 `state/boundary.txt` 与注入参数的值）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Boundary::BeforeIntentPrepared => "before_intent_prepared",
            Boundary::AfterIntentPrepared => "after_intent_prepared",
            Boundary::BeforePublish => "before_publish",
            Boundary::AfterPublish => "after_publish",
            Boundary::BeforeAdvancePublished => "before_advance_published",
            Boundary::AfterAdvancePublished => "after_advance_published",
            Boundary::BeforeMetadataCommit => "before_metadata_commit",
            Boundary::AfterMetadataCommit => "after_metadata_commit",
            Boundary::BeforeAck => "before_ack",
        }
    }

    /// 全部提交持久化边界（验收要求“每一持久化边界”都有一次强杀）。
    #[must_use]
    pub fn all() -> [Boundary; 9] {
        [
            Boundary::BeforeIntentPrepared,
            Boundary::AfterIntentPrepared,
            Boundary::BeforePublish,
            Boundary::AfterPublish,
            Boundary::BeforeAdvancePublished,
            Boundary::AfterAdvancePublished,
            Boundary::BeforeMetadataCommit,
            Boundary::AfterMetadataCommit,
            Boundary::BeforeAck,
        ]
    }

    /// 按短名解析注入点。
    ///
    /// # Errors
    ///
    /// 名字不在 [`Boundary::all`] 里。
    pub fn parse(name: &str) -> Result<Boundary, String> {
        Boundary::all()
            .into_iter()
            .find(|boundary| boundary.as_str() == name)
            .ok_or_else(|| format!("未知边界 {name:?}"))
    }
}

/// 文件型假端口的状态：`state/` 下的追加日志与 `work/` 下的工作文件。
pub struct CrashStore {
    dir: PathBuf,
    crash_at: Option<Boundary>,
}

/// 一条已提交版本的日志行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRow {
    /// 条目身份。
    pub entry: EntryId,
    /// 事务后的 revision。
    pub revision: Revision,
    /// 版本身份。
    pub version: VersionId,
    /// 内容摘要。
    pub digest: ContentDigest,
    /// 内容长度。
    pub size: u64,
}

/// 从磁盘重新读出的装置状态（断言用；不含任何内存残留）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JournalState {
    /// 意图日志还原出的意图（按写入顺序）。
    pub intents: Vec<PutIntent>,
    /// 已提交版本。
    pub versions: Vec<VersionRow>,
    /// 条目当前 revision。
    pub entries: BTreeMap<EntryId, Revision>,
    /// 已登记对象：摘要 → 字节。
    pub objects: BTreeMap<ContentDigest, Vec<u8>>,
    /// 收到过的元数据事务次数。
    pub applies: usize,
}

impl CrashStore {
    /// 打开（或初始化）一个装置目录。
    ///
    /// # Errors
    ///
    /// 目录无法创建。
    pub fn open(dir: impl Into<PathBuf>, crash_at: Option<Boundary>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("state"))?;
        fs::create_dir_all(dir.join("work"))?;
        Ok(CrashStore { dir, crash_at })
    }

    /// 装置根目录。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 工作文件路径。
    #[must_use]
    pub fn work_file(&self) -> PathBuf {
        self.dir.join("work").join(NAME)
    }

    /// 工作文件当前字节（不存在时为 `None`）。
    ///
    /// # Errors
    ///
    /// 读失败（不是“不存在”）。
    pub fn work_bytes(&self) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.work_file()) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn state_path(&self, name: &str) -> PathBuf {
        self.dir.join("state").join(name)
    }

    fn append(&self, name: &str, line: &str) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state_path(name))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()
    }

    fn read_lines(&self, name: &str) -> io::Result<Vec<String>> {
        match fs::read_to_string(self.state_path(name)) {
            Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// 到达一个持久化边界：落 `state/boundary.txt`；命中注入点时挂起，等父进程强杀。
    ///
    /// # Errors
    ///
    /// 边界标记写不进去。
    pub fn boundary(&self, boundary: Boundary) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.state_path("boundary.txt"))?;
        file.write_all(boundary.as_str().as_bytes())?;
        file.sync_all()?;
        if self.crash_at == Some(boundary) {
            // 真实地“卡在这里”：父进程看到 boundary.txt 后 SIGKILL。没有父进程时自 abort。
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            std::process::abort();
        }
        Ok(())
    }

    /// 父进程等待子进程到达注入点（读到 `crash_at` 的边界名）。
    ///
    /// # Errors
    ///
    /// 超时（子进程没到点就退出）。
    pub fn wait_for_boundary(&self, expected: Boundary, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(text) = fs::read_to_string(self.state_path("boundary.txt"))
                && text == expected.as_str()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(format!("子进程没有到达边界 {}", expected.as_str()))
    }

    /// 记录一条已登记对象（父进程准备阶段用）。
    ///
    /// # Errors
    ///
    /// 日志写失败。
    pub fn register_object(&self, content: &[u8]) -> io::Result<ContentDigest> {
        let digest = digest_of(content);
        self.append(
            "objects.log",
            &format!("{} {} {}", digest, content.len(), hex_encode(content)),
        )?;
        Ok(digest)
    }

    /// 写入工作文件初始内容并返回它的现场快照（父进程准备阶段用）。
    ///
    /// # Errors
    ///
    /// 文件写入或元数据读取失败。
    pub fn seed_target(&self, content: &[u8], revision: u64) -> io::Result<TargetSnapshot> {
        fs::write(self.work_file(), content)?;
        self.append("entries.log", &format!("{ENTRY} {revision}"))?;
        let meta = fs::metadata(self.work_file())?;
        Ok(TargetSnapshot {
            entry: ENTRY,
            revision,
            digest: digest_of(content),
            identity: FileKey {
                device: meta.dev(),
                inode: meta.ino(),
                links: u32::try_from(meta.nlink()).unwrap_or(u32::MAX),
                bytes: meta.len(),
            },
        })
    }

    /// 由外部程序改写工作文件（不经过服务端，模拟“发布后又被外部改写”）。
    ///
    /// # Errors
    ///
    /// 写失败。
    pub fn external_write(&self, content: &[u8]) -> io::Result<()> {
        fs::write(self.work_file(), content)
    }

    /// 让后续每一次 `observe` 都返回“不可读”（模拟权限/挂载故障，不能读成删除）。
    ///
    /// # Errors
    ///
    /// 标记写失败。
    pub fn mark_unreadable(&self) -> io::Result<()> {
        self.append("unreadable.log", "observe=unreadable")
    }

    /// 从磁盘重新读出全部状态。
    ///
    /// # Errors
    ///
    /// 日志行无法解析（装置自身损坏）。
    pub fn journal(&self) -> io::Result<JournalState> {
        let mut state = JournalState::default();
        let mut by_id: BTreeMap<IntentId, usize> = BTreeMap::new();
        for line in self.read_lines("intents.log")? {
            let fields: Vec<&str> = line.split(' ').collect();
            match fields.first().copied() {
                Some("begin") => {
                    let intent = parse_begin(&fields)?;
                    by_id.insert(intent.intent_id, state.intents.len());
                    state.intents.push(intent);
                }
                Some("phase") => {
                    let id = parse_id::<IntentId>(&fields, 1)?;
                    let index = *by_id
                        .get(&id)
                        .ok_or_else(|| invalid("phase 行指向未知意图"))?;
                    let phase =
                        PutPhase::from_text(fields[2]).ok_or_else(|| invalid("未知意图阶段"))?;
                    state.intents[index].phase = phase;
                }
                _ => return Err(invalid("未知意图日志行")),
            }
        }
        for line in self.read_lines("entries.log")? {
            let fields: Vec<&str> = line.split(' ').collect();
            let entry = parse_id::<EntryId>(&fields, 0)?;
            let revision =
                Revision::new(parse_u64(&fields, 1)?).map_err(|_| invalid("revision"))?;
            state.entries.insert(entry, revision);
        }
        for line in self.read_lines("versions.log")? {
            let fields: Vec<&str> = line.split(' ').collect();
            state.versions.push(VersionRow {
                entry: parse_id::<EntryId>(&fields, 0)?,
                revision: Revision::new(parse_u64(&fields, 1)?).map_err(|_| invalid("revision"))?,
                version: parse_id::<VersionId>(&fields, 2)?,
                digest: ContentDigest::from_hex(fields[3]).map_err(|_| invalid("digest"))?,
                size: parse_u64(&fields, 4)?,
            });
            state.applies += 1;
        }
        for line in self.read_lines("objects.log")? {
            let fields: Vec<&str> = line.split(' ').collect();
            let digest = ContentDigest::from_hex(fields[0]).map_err(|_| invalid("digest"))?;
            state.objects.insert(digest, hex_decode(fields[2]));
        }
        Ok(state)
    }

    fn observe_work_file(&self) -> Result<TargetObservation, TargetError> {
        if self.state_path("unreadable.log").exists() {
            return Err(TargetError::Unreadable {
                detail: "EACCES（注入的不可读故障）".to_owned(),
            });
        }
        match fs::metadata(self.work_file()) {
            Ok(meta) => {
                let bytes =
                    fs::read(self.work_file()).map_err(|error| TargetError::Unreadable {
                        detail: error.to_string(),
                    })?;
                Ok(TargetObservation::Present {
                    identity: FileKey {
                        device: meta.dev(),
                        inode: meta.ino(),
                        links: u32::try_from(meta.nlink()).unwrap_or(u32::MAX),
                        bytes: meta.len(),
                    },
                    digest: digest_of(&bytes),
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TargetObservation::Absent),
            Err(error) => Err(TargetError::Unreadable {
                detail: error.to_string(),
            }),
        }
    }

    fn publish_object(&self, request: &PublishRequest) -> Result<PublishedTarget, TargetError> {
        let state = self.journal().map_err(|error| TargetError::Storage {
            stage: "publish",
            failure: StorageFailure::Io,
            detail: error.to_string(),
        })?;
        let content = state
            .objects
            .get(&request.digest)
            .ok_or_else(|| TargetError::Storage {
                stage: "publish",
                failure: StorageFailure::Io,
                detail: format!("内容对象 {} 未登记", request.digest),
            })?;
        // 与真实发布同样的替换语义：先写目标子卷内的暂存文件并刷盘，再改名替换（换新 inode）。
        let staging = self.dir.join("work").join(".note.txt.staging");
        fs::write(&staging, content).map_err(storage_io("stage_publish"))?;
        OpenOptions::new()
            .read(true)
            .open(&staging)
            .and_then(|file| file.sync_all())
            .map_err(storage_io("stage_fsync"))?;
        fs::rename(&staging, self.work_file()).map_err(storage_io("rename"))?;
        let meta = fs::metadata(self.work_file()).map_err(storage_io("restat"))?;
        Ok(PublishedTarget {
            identity: FileKey {
                device: meta.dev(),
                inode: meta.ino(),
                links: u32::try_from(meta.nlink()).unwrap_or(u32::MAX),
                bytes: meta.len(),
            },
            digest: request.digest,
        })
    }

    fn begin_put(&self, new: &NewPutIntent) -> io::Result<PutIntent> {
        self.boundary(Boundary::BeforeIntentPrepared)?;
        let intent = new.prepared();
        self.append(
            "intents.log",
            &format!(
                "begin {} {} {} {} {} {}",
                intent.intent_id,
                intent.root,
                intent.target_path,
                intent.staging_path,
                intent.before.to_json(),
                intent.after.to_json()
            ),
        )?;
        self.boundary(Boundary::AfterIntentPrepared)?;
        Ok(intent)
    }

    fn advance_put(&self, id: IntentId, next: PutPhase) -> Result<PutIntent, IntentLogError> {
        let state = self.journal().map_err(log_io)?;
        let current = state
            .intents
            .into_iter()
            .find(|intent| intent.intent_id == id)
            .ok_or(IntentLogError::UnknownIntent { id })?;
        if !current.phase.allows_transition_to(next) {
            return Err(IntentLogError::IllegalTransition {
                from: current.phase,
                to: next,
            });
        }
        // 只有 `published` 这个阶段是提交路径的持久化边界（`needs_recovery` 是恢复自己的标记）。
        if next == PutPhase::Published {
            self.boundary(Boundary::BeforeAdvancePublished)
                .map_err(log_io)?;
        }
        self.append("intents.log", &format!("phase {id} {}", next.as_text()))
            .map_err(log_io)?;
        if next == PutPhase::Published {
            self.boundary(Boundary::AfterAdvancePublished)
                .map_err(log_io)?;
        }
        let mut updated = current;
        updated.phase = next;
        Ok(updated)
    }
}

fn storage_io(stage: &'static str) -> impl Fn(io::Error) -> TargetError {
    move |error| TargetError::Storage {
        stage,
        failure: StorageFailure::Io,
        detail: error.to_string(),
    }
}

fn log_io(error: io::Error) -> IntentLogError {
    IntentLogError::Storage {
        detail: error.to_string(),
    }
}

fn store_io(error: io::Error) -> PutStoreError {
    PutStoreError::Storage {
        detail: error.to_string(),
    }
}

fn invalid(detail: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.to_owned())
}

fn parse_id<T: std::str::FromStr>(fields: &[&str], index: usize) -> io::Result<T> {
    fields
        .get(index)
        .ok_or_else(|| invalid("日志行缺少字段"))?
        .parse::<T>()
        .map_err(|_| invalid("身份无法解析"))
}

fn parse_u64(fields: &[&str], index: usize) -> io::Result<u64> {
    fields
        .get(index)
        .ok_or_else(|| invalid("日志行缺少字段"))?
        .parse::<u64>()
        .map_err(|_| invalid("计数无法解析"))
}

fn parse_begin(fields: &[&str]) -> io::Result<PutIntent> {
    if fields.len() != 7 {
        return Err(invalid("begin 行字段数不对"));
    }
    Ok(PutIntent {
        intent_id: parse_id::<IntentId>(fields, 1)?,
        root: parse_id::<RootId>(fields, 2)?,
        target_path: fields[3].to_owned(),
        staging_path: fields[4].to_owned(),
        before: TargetBefore::from_json(fields[5]).map_err(|_| invalid("expected_before"))?,
        after: TargetAfter::from_json(fields[6]).map_err(|_| invalid("desired_after"))?,
        phase: PutPhase::Prepared,
    })
}

/// 共享同一个文件型状态的句柄（端口适配器在它上面拆分）。
#[derive(Clone)]
pub struct StoreHandle(Rc<CrashStore>);

impl StoreHandle {
    /// 打开装置目录。
    ///
    /// # Errors
    ///
    /// 目录无法创建。
    pub fn open(dir: impl Into<PathBuf>, crash_at: Option<Boundary>) -> io::Result<Self> {
        Ok(StoreHandle(Rc::new(CrashStore::open(dir, crash_at)?)))
    }

    /// 底层装置（断言与父进程准备用）。
    #[must_use]
    pub fn store(&self) -> &CrashStore {
        &self.0
    }
}

/// `IntentLog` 端口适配器。
#[derive(Clone)]
pub struct LogPort(StoreHandle);

/// `TargetPort` 端口适配器。
#[derive(Clone)]
pub struct HostTargetPort(StoreHandle);

/// `PutCommitStore` 端口适配器。
#[derive(Clone)]
pub struct JournalStorePort(StoreHandle);

impl StoreHandle {
    /// 拆出三个端口（提交路径需要三个同时可变借用的端口）。
    #[must_use]
    pub fn ports(&self) -> (LogPort, HostTargetPort, JournalStorePort) {
        (
            LogPort(self.clone()),
            HostTargetPort(self.clone()),
            JournalStorePort(self.clone()),
        )
    }
}

impl IntentLog for LogPort {
    async fn begin_put(
        &mut self,
        new: &NewPutIntent,
        _at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        self.0.store().begin_put(new).map_err(log_io)
    }

    async fn advance_put(
        &mut self,
        id: IntentId,
        next: PutPhase,
        _at: UtcTimestamp,
    ) -> Result<PutIntent, IntentLogError> {
        self.0.store().advance_put(id, next)
    }

    async fn load_put(&mut self, id: IntentId) -> Result<Option<PutIntent>, IntentLogError> {
        let state = self.0.store().journal().map_err(log_io)?;
        Ok(state
            .intents
            .into_iter()
            .find(|intent| intent.intent_id == id))
    }

    async fn unfinished_puts(&mut self, root: RootId) -> Result<Vec<PutIntent>, IntentLogError> {
        let state = self.0.store().journal().map_err(log_io)?;
        Ok(state
            .intents
            .into_iter()
            .filter(|intent| intent.root == root && intent.phase.is_open())
            .collect())
    }
}

impl TargetPort for HostTargetPort {
    fn observe(&mut self, _target_path: &str) -> Result<TargetObservation, TargetError> {
        self.0.store().observe_work_file()
    }

    fn publish(&mut self, request: &PublishRequest) -> Result<PublishedTarget, TargetError> {
        self.0
            .store()
            .boundary(Boundary::BeforePublish)
            .map_err(|error| TargetError::Storage {
                stage: "publish",
                failure: StorageFailure::Io,
                detail: error.to_string(),
            })?;
        let published = self.0.store().publish_object(request)?;
        self.0
            .store()
            .boundary(Boundary::AfterPublish)
            .map_err(|error| TargetError::Storage {
                stage: "publish",
                failure: StorageFailure::Io,
                detail: error.to_string(),
            })?;
        Ok(published)
    }
}

impl PutCommitStore for JournalStorePort {
    async fn root_facts(&mut self, _root: RootId) -> Result<RootFacts, PutStoreError> {
        Ok(RootFacts {
            epoch: ServerEpoch::new(EPOCH).map_err(|_| PutStoreError::MalformedIntent {
                detail: "epoch 越界".to_owned(),
            })?,
        })
    }

    async fn entry_baseline(
        &mut self,
        root: RootId,
        entry: EntryId,
    ) -> Result<EntryBaseline, PutStoreError> {
        let state = self.0.store().journal().map_err(store_io)?;
        let revision = state
            .entries
            .get(&entry)
            .copied()
            .ok_or(PutStoreError::EntryMissing { entry })?;
        Ok(EntryBaseline {
            entry,
            root,
            parent: PARENT,
            name: NAME.to_owned(),
            revision,
            kind: EntryKind::File,
        })
    }

    async fn object_state(
        &mut self,
        _root: RootId,
        digest: ContentDigest,
    ) -> Result<ObjectState, PutStoreError> {
        let state = self.0.store().journal().map_err(store_io)?;
        Ok(match state.objects.get(&digest) {
            Some(bytes) => ObjectState::Persisted {
                bytes: bytes.len() as u64,
            },
            None => ObjectState::Missing,
        })
    }

    async fn apply_put(&mut self, write: &PutWrite) -> Result<PutRecord, PutStoreError> {
        // 契约：版本/条目 revision/意图收尾在同一个事务里；这里用一次边界 + 三条追加行表达。
        let store = self.0.store();
        let state = store.journal().map_err(store_io)?;
        let intent = state
            .intents
            .iter()
            .find(|intent| intent.intent_id == write.intent_id)
            .ok_or(PutStoreError::IllegalIntentTransition {
                from: PutPhase::Prepared,
                to: PutPhase::Finalized,
            })?;
        if intent.phase != PutPhase::Published {
            return Err(PutStoreError::IllegalIntentTransition {
                from: intent.phase,
                to: PutPhase::Finalized,
            });
        }
        let actual = state
            .entries
            .get(&write.entry)
            .copied()
            .ok_or(PutStoreError::EntryMissing { entry: write.entry })?;
        if actual != write.expected_revision {
            return Err(PutStoreError::RevisionChanged {
                expected: write.expected_revision,
                actual,
            });
        }
        let next = actual.next().map_err(|_| PutStoreError::MalformedIntent {
            detail: "revision 越界".to_owned(),
        })?;
        let seq = RootSeq::new(state.versions.len() as u64 + 1).map_err(|_| {
            PutStoreError::MalformedIntent {
                detail: "RootSeq 越界".to_owned(),
            }
        })?;
        let version = VersionId::from_bytes([0x80u8.wrapping_add(seq.get() as u8); 16]);
        store
            .boundary(Boundary::BeforeMetadataCommit)
            .map_err(store_io)?;
        store
            .append("entries.log", &format!("{} {}", write.entry, next.get()))
            .map_err(store_io)?;
        store
            .append(
                "versions.log",
                &format!(
                    "{} {} {} {} {}",
                    write.entry,
                    next.get(),
                    version,
                    write.content.digest,
                    write.content.bytes
                ),
            )
            .map_err(store_io)?;
        store
            .append(
                "intents.log",
                &format!(
                    "phase {} {}",
                    write.intent_id,
                    PutPhase::Finalized.as_text()
                ),
            )
            .map_err(store_io)?;
        store
            .boundary(Boundary::AfterMetadataCommit)
            .map_err(store_io)?;
        Ok(PutRecord {
            intent_id: write.intent_id,
            phase: PutPhase::Finalized,
            version,
            revision: next,
            seq,
            tx: TxId::from_bytes([0x71; 16]),
            digest: write.content.digest,
            size: write.content.bytes,
        })
    }
}

/// 构造一次“把 [`NEW_CONTENT`] 写到 `note.txt`”的提交请求：现场由装置当前状态观察得到。
///
/// # Errors
///
/// 观察目标或读状态失败。
pub fn commit_request(store: &CrashStore) -> Result<PutRequest, String> {
    let observed = store
        .observe_work_file()
        .map_err(|error| error.to_string())?;
    let state = store.journal().map_err(|error| error.to_string())?;
    let revision = state
        .entries
        .get(&ENTRY)
        .copied()
        .ok_or_else(|| "条目基线缺失".to_owned())?;
    let before = match observed {
        TargetObservation::Absent => TargetBefore::Absent,
        TargetObservation::Present { identity, digest } => TargetBefore::Present(TargetSnapshot {
            entry: ENTRY,
            revision: revision.get(),
            digest,
            identity,
        }),
    };
    Ok(PutRequest {
        intent_id: INTENT,
        root: ROOT,
        entry: ENTRY,
        expected_epoch: ServerEpoch::new(EPOCH).expect("装置 epoch 合法"),
        expected_revision: revision,
        expected_parent: PARENT,
        expected_name: NAME.to_owned(),
        target_path: TARGET_PATH.to_owned(),
        staging_path: STAGING.to_owned(),
        content: ExpectedContent::new(digest_of(NEW_CONTENT), NEW_CONTENT.len() as u64),
        before,
    })
}

/// 子进程路径：跑一次完整的 `commit_put`，成功时在 ACK 之前再过一个边界。
///
/// # Errors
///
/// 提交失败时返回错误码文本（装置仍留在磁盘上供重启检查）。
pub async fn run_commit(handle: &StoreHandle) -> Result<CommittedPut, String> {
    let request = commit_request(handle.store())?;
    let (mut log, mut target, mut journal) = handle.ports();
    let mut gate = RootCommitGate::new();
    let committed = commit_put(
        &mut gate,
        &mut journal,
        &mut log,
        &mut target,
        now(),
        &request,
    )
    .await
    .map_err(|failure| failure.code().to_owned())?;
    // 元数据事务已提交、响应还没发出：这里是最容易被外部误认为“已成功”的位置。
    handle
        .store()
        .boundary(Boundary::BeforeAck)
        .map_err(|error| error.to_string())?;
    Ok(committed)
}

/// 父进程路径：重启后用同一份磁盘证据执行启动恢复。
///
/// # Errors
///
/// 见 [`RecoveryError`]（读不出未完成意图）。
pub async fn run_recovery(handle: &StoreHandle) -> Result<RecoveryReport, RecoveryError> {
    let (mut log, mut target, mut journal) = handle.ports();
    recover_root(&mut log, &mut target, &mut journal, now(), ROOT).await
}

/// 启动顺序：先恢复，再把报告交给写入门，最后返回可用的门与报告。
///
/// # Errors
///
/// 见 [`RecoveryError`]：恢复没完成时返回错误，调用方必须保持写入关闭。
pub async fn start_with_recovery(
    handle: &StoreHandle,
) -> Result<(WriteAdmission, RecoveryReport), RecoveryError> {
    let report = run_recovery(handle).await?;
    let mut admission = WriteAdmission::new();
    admission.record(report.clone());
    Ok((admission, report))
}

/// 装置里用的固定时间。
#[must_use]
pub fn now() -> UtcTimestamp {
    UtcTimestamp::from_unix_seconds(1_700_000_000).expect("固定时间合法")
}
