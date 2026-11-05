//! 外部目录观察的 Linux 适配器（design §12.1、§11.5、§16.1）：把 `RootHandle` 与 `stage_copy` 接到
//! `server-core` 的 [`WorkTreePort`] 与 [`ObjectBlobPort`]。
//!
//! 核心只定义端口与规则，平台库不依赖核心，装配发生在进程入口（本模块）。所有对 Root 内容的访问都
//! 走 Root 句柄：不跟随符号链接、不穿过挂载边界；读不到一律上报错误，**不会**报告成“不存在”或
//! “目录为空”（design §11.4）。候选建在对象区 staging 目录：同一文件系统优先 `FICLONE`，跨卷或
//! 不支持时校验复制并暴露原因（design §11.5）。

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::rc::Rc;

use vaultsyncd_platform_linux_server::files::clone::{StageRequest, stage_copy};
use vaultsyncd_platform_linux_server::files::{
    self, CloneMethod as PlatformCloneMethod, CopyFallback as PlatformCopyFallback, EntryKind,
    FileIdentity, OBJECT_FILE_MODE, ResolveMode, RestrictError, RestrictedDir, RootHandle,
    StageError, StageFailure,
};
use vaultsyncd_server_core::objects::ContentDigest;
use vaultsyncd_server_core::ports::object_store::{
    BlobError, CopyFallback, CopyMethod, FileKey, ObjectBlobPort, PublishedBlob, StagedBlob,
    StorageFailure, StoredBytes, StoredFacts, StoredSlice,
};
use vaultsyncd_server_core::ports::work_tree::{
    FileStamp, WorkDirEntry, WorkDirListing, WorkEntryKind, WorkObservation, WorkTreeError,
    WorkTreePort,
};

/// Root 工作目录的只读观察端口（Linux）。
pub struct LinuxWorkTree {
    root: Rc<RootHandle>,
}

impl LinuxWorkTree {
    /// 以已打开的 Root 句柄建立端口。
    #[must_use]
    pub fn new(root: Rc<RootHandle>) -> Self {
        LinuxWorkTree { root }
    }
}

fn stamp_of(identity: &FileIdentity) -> FileStamp {
    FileStamp {
        key: file_key(identity),
        btime: identity.btime,
        mtime: identity.mtime,
        ctime: identity.ctime,
    }
}

fn file_key(identity: &FileIdentity) -> FileKey {
    FileKey {
        device: (u64::from(identity.dev_major) << 32) | u64::from(identity.dev_minor),
        inode: identity.inode,
        links: identity.nlink,
        bytes: identity.size,
    }
}

fn entry_kind(kind: EntryKind) -> WorkEntryKind {
    match kind {
        EntryKind::File => WorkEntryKind::File,
        EntryKind::Directory => WorkEntryKind::Directory,
        EntryKind::Symlink => WorkEntryKind::Symlink,
        EntryKind::Other => WorkEntryKind::Other,
        EntryKind::Unknown => WorkEntryKind::Unknown,
    }
}

fn map_restrict(path: &str, error: &RestrictError) -> WorkTreeError {
    match error {
        RestrictError::NotFound { .. } => WorkTreeError::Gone {
            path: path.to_owned(),
        },
        RestrictError::InvalidRelativePath(_) | RestrictError::EscapesRoot { .. } => {
            WorkTreeError::InvalidPath {
                detail: error.to_string(),
            }
        }
        _ => WorkTreeError::Unreadable {
            path: path.to_owned(),
            detail: error.to_string(),
        },
    }
}

impl WorkTreePort for LinuxWorkTree {
    fn list_dir(&mut self, dir: &str) -> Result<WorkDirListing, WorkTreeError> {
        // 每列一个目录核对一次 Root 身份（挂载漂移时整 Root 暂停，不推导删除）。
        self.root
            .verify()
            .map_err(|error| WorkTreeError::RootUnavailable {
                detail: error.to_string(),
            })?;
        let raw = self
            .root
            .list_dir(Path::new(dir))
            .map_err(|error| map_restrict(dir, &error))?;
        let mut entries = Vec::with_capacity(raw.len());
        let mut unrepresentable = 0usize;
        for entry in raw {
            match entry.name.to_str() {
                Some(name) => entries.push(WorkDirEntry {
                    name: name.to_owned(),
                    kind: entry_kind(entry.kind),
                }),
                None => unrepresentable += 1,
            }
        }
        Ok(WorkDirListing {
            entries,
            unrepresentable_names: unrepresentable,
        })
    }

    fn observe(&mut self, path: &str) -> Result<WorkObservation, WorkTreeError> {
        match self.root.identity_at(Path::new(path)) {
            Ok(None) => Ok(WorkObservation::Absent),
            Ok(Some(identity)) if identity.is_regular_file() => {
                Ok(WorkObservation::File(stamp_of(&identity)))
            }
            Ok(Some(identity)) if identity.is_directory() => {
                Ok(WorkObservation::NotRegular(WorkEntryKind::Directory))
            }
            Ok(Some(_)) => Ok(WorkObservation::NotRegular(WorkEntryKind::Other)),
            Err(RestrictError::SymlinkRejected { .. }) => {
                Ok(WorkObservation::NotRegular(WorkEntryKind::Symlink))
            }
            Err(error) => Err(map_restrict(path, &error)),
        }
    }
}

/// 对象区字节层端口（Linux）：源文件经 Root 句柄读取，候选建在对象区 staging 目录。
pub struct LinuxObjectPort {
    root: Rc<RootHandle>,
    objects: std::path::PathBuf,
}

impl LinuxObjectPort {
    /// 以 Root 句柄与对象区根目录建立端口。
    #[must_use]
    pub fn new(root: Rc<RootHandle>, objects: impl Into<std::path::PathBuf>) -> Self {
        LinuxObjectPort {
            root,
            objects: objects.into(),
        }
    }

    fn staging_dir(&self) -> std::path::PathBuf {
        self.objects.join("staging")
    }

    fn prepare_dirs(&self, object: &Path) -> Result<(), BlobError> {
        fs::create_dir_all(self.staging_dir())
            .map_err(|error| map_io("create_staging_dir", &error))?;
        if let Some(parent) = object.parent() {
            fs::create_dir_all(parent).map_err(|error| map_io("create_object_dir", &error))?;
        }
        Ok(())
    }

    fn open_dir(&self, path: &Path, stage: &'static str) -> Result<RestrictedDir, BlobError> {
        RestrictedDir::open(path, ResolveMode::Auto).map_err(|error| BlobError::Storage {
            stage,
            failure: storage_failure_of_restrict(&error),
            detail: error.to_string(),
        })
    }
}

impl ObjectBlobPort for LinuxObjectPort {
    fn stage_working_copy(
        &mut self,
        source: &Path,
        staging: &Path,
    ) -> Result<StagedBlob, BlobError> {
        self.prepare_dirs(staging)?;
        let staging_name = staging
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| BlobError::InvalidRequest {
                detail: format!("暂存路径没有可用文件名：{}", staging.display()),
            })?
            .to_owned();
        let opened = self
            .root
            .open_read(source)
            .map_err(|error| BlobError::SourceUnreadable {
                detail: error.to_string(),
            })?;
        let staging_dir = self.open_dir(&self.staging_dir(), "open_staging_dir")?;
        let staged = stage_copy(StageRequest {
            source: &opened.file,
            source_identity: &opened.identity,
            target_dir: &staging_dir,
            staging_name: &staging_name,
        })
        .map_err(|error| map_stage(&error))?;
        let staged_identity = FileIdentity::of_fd(staged.file.as_raw_fd()).map_err(|error| {
            BlobError::Verification {
                detail: format!("读取暂存副本身份失败：{error}"),
            }
        })?;
        Ok(StagedBlob {
            staging: staging.to_path_buf(),
            source: file_key(&opened.identity),
            staged: file_key(&staged_identity),
            digest: ContentDigest::from_bytes(staged.sha256),
            method: match staged.method {
                PlatformCloneMethod::Ficlone => CopyMethod::Reflink,
                PlatformCloneMethod::VerifiedCopy => CopyMethod::VerifiedCopy,
            },
            fallback: staged.fallback.map(|fallback| match fallback {
                PlatformCopyFallback::CrossDevice => CopyFallback::CrossDevice,
                PlatformCopyFallback::ReflinkUnsupported => CopyFallback::ReflinkUnsupported,
            }),
        })
    }

    fn publish_staged(
        &mut self,
        staged: &StagedBlob,
        object: &Path,
    ) -> Result<PublishedBlob, BlobError> {
        self.prepare_dirs(object)?;
        // 对象只由服务管理，不向外部写入者开放写权限（design §11.1）。
        fs::set_permissions(
            &staged.staging,
            fs::Permissions::from_mode(OBJECT_FILE_MODE),
        )
        .map_err(|error| map_io("chmod_staging", &error))?;
        let staging_dir = self.open_dir(&self.staging_dir(), "open_staging_dir")?;
        let object_dir =
            self.open_dir(object.parent().unwrap_or(&self.objects), "open_object_dir")?;
        let staging_name = staged
            .staging
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| BlobError::InvalidRequest {
                detail: "暂存路径没有可用文件名".to_owned(),
            })?;
        let object_name = object
            .file_name()
            .ok_or_else(|| BlobError::InvalidRequest {
                detail: "对象路径没有可用文件名".to_owned(),
            })?;
        match staging_dir.rename(
            Path::new(staging_name),
            &object_dir,
            Path::new(object_name),
            true,
        ) {
            Ok(()) => {}
            Err(RestrictError::AlreadyExists { .. }) => {
                return Ok(PublishedBlob {
                    object: object.to_path_buf(),
                    already_present: true,
                });
            }
            Err(error) => {
                return Err(BlobError::Storage {
                    stage: "publish_rename",
                    failure: storage_failure_of_restrict(&error),
                    detail: error.to_string(),
                });
            }
        }
        object_dir.fsync_dir().map_err(|error| BlobError::Storage {
            stage: "publish_dir_fsync",
            failure: storage_failure_of_restrict(&error),
            detail: error.to_string(),
        })?;
        Ok(PublishedBlob {
            object: object.to_path_buf(),
            already_present: false,
        })
    }

    fn discard_staged(&mut self, staging: &Path) -> Result<(), BlobError> {
        match fs::remove_file(staging) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(map_io("discard_staging", &error)),
        }
    }

    fn verify_object(&mut self, object: &Path) -> Result<StoredFacts, BlobError> {
        let file = fs::File::open(object).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => BlobError::ObjectMissing {
                detail: object.display().to_string(),
            },
            _ => map_io("open_object", &error),
        })?;
        let (digest, bytes) =
            files::hash_file(&file).map_err(|error| map_io("hash_object", &error))?;
        Ok(StoredFacts {
            bytes,
            digest: ContentDigest::from_bytes(digest),
        })
    }

    fn read_object(&mut self, object: &Path) -> Result<StoredBytes, BlobError> {
        let data = match fs::read(object) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(BlobError::ObjectMissing {
                    detail: object.display().to_string(),
                });
            }
            Err(error) => return Err(map_io("read_object", &error)),
        };
        Ok(StoredBytes {
            bytes: data.len() as u64,
            digest: ContentDigest::from_bytes(files::hash::digest(&data)),
            data,
        })
    }

    fn read_object_range(
        &mut self,
        object: &Path,
        offset: u64,
        length: u64,
    ) -> Result<StoredSlice, BlobError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = fs::File::open(object).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => BlobError::ObjectMissing {
                detail: object.display().to_string(),
            },
            _ => map_io("open_object", &error),
        })?;
        let total = file
            .metadata()
            .map_err(|error| map_io("stat_object", &error))?
            .len();
        if offset.checked_add(length).is_none_or(|end| end > total) {
            return Err(BlobError::InvalidRequest {
                detail: format!("范围 {offset}+{length} 超出对象长度 {total}"),
            });
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| map_io("seek_object", &error))?;
        let mut data = Vec::new();
        let read = file
            .take(length)
            .read_to_end(&mut data)
            .map_err(|error| map_io("read_object_range", &error))?;
        if read as u64 != length {
            return Err(BlobError::Storage {
                stage: "read_object_range",
                failure: StorageFailure::Io,
                detail: format!("只读到 {read} 字节，期望 {length}"),
            });
        }
        Ok(StoredSlice {
            bytes: length,
            data,
        })
    }
}

fn map_stage(error: &StageError) -> BlobError {
    match &error.reason {
        // 源在副本建立期间变化：不是删除，也不是完整内容；核心按源不可用处理并重新排队/重试。
        StageFailure::SourceChanged { changed, .. } => BlobError::SourceUnreadable {
            detail: format!("源文件在副本建立期间变化（{}）", changed.join(",")),
        },
        StageFailure::Verify { detail } => BlobError::Verification {
            detail: detail.clone(),
        },
        StageFailure::Io { stage, source } => map_io(stage, source),
        StageFailure::ReflinkFailed(source) => map_io("reflink", source),
        StageFailure::Create(error) => BlobError::Storage {
            stage: "stage_create",
            failure: storage_failure_of_restrict(error),
            detail: error.to_string(),
        },
        StageFailure::WrongFilesystem {
            staging_mount,
            target_mount,
        } => BlobError::Storage {
            stage: "stage_mount_check",
            failure: StorageFailure::Other,
            detail: format!("暂存挂载 {staging_mount:?} 与目标挂载 {target_mount:?} 不同"),
        },
    }
}

fn storage_failure_of_restrict(error: &RestrictError) -> StorageFailure {
    let source = match error {
        RestrictError::PermissionDenied { .. } => return StorageFailure::PermissionDenied,
        RestrictError::OpenRoot { source, .. }
        | RestrictError::Resolve { source, .. }
        | RestrictError::EscapesRoot { source, .. }
        | RestrictError::SymlinkRejected { source, .. }
        | RestrictError::NotFound { source, .. }
        | RestrictError::AlreadyExists { source, .. }
        | RestrictError::CrossDeviceRename { source, .. }
        | RestrictError::Rename { source, .. }
        | RestrictError::Unlink { source, .. }
        | RestrictError::Fsync { source, .. } => source,
        _ => return StorageFailure::Other,
    };
    storage_failure_of_io(source)
}

fn map_io(stage: &'static str, error: &io::Error) -> BlobError {
    BlobError::Storage {
        stage,
        failure: storage_failure_of_io(error),
        detail: error.to_string(),
    }
}

fn storage_failure_of_io(error: &io::Error) -> StorageFailure {
    const ENOSPC: i32 = 28;
    const EDQUOT: i32 = 122;
    const EIO: i32 = 5;
    const EACCES: i32 = 13;
    const EPERM: i32 = 1;
    const EROFS: i32 = 30;
    match error.raw_os_error() {
        Some(ENOSPC) => StorageFailure::NoSpace,
        Some(EDQUOT) => StorageFailure::QuotaExceeded,
        Some(EIO) => StorageFailure::Io,
        Some(EACCES | EPERM) => StorageFailure::PermissionDenied,
        Some(EROFS) => StorageFailure::ReadOnly,
        _ => StorageFailure::Other,
    }
}

#[cfg(test)]
mod tests;
