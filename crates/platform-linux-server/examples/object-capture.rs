//! S033 真实样本诊断命令：用 Linux 适配层实现 `server-core` 的对象端口。
//!
//! 这里就是“入口用本地 wrapper 组装”的位置：核心只定义端口与规则，
//! `vaultsyncd-platform-linux-server` 的库不依赖 `server-core`，由本示例把
//! `files::clone`/`files::restricted` 的动作包装成
//! `vaultsyncd_server_core::ports::object_store::ObjectBlobPort`。
//!
//! 用法（输出为稳定 `key=value` 行，验收脚本按行断言）：
//!
//! ```text
//! object-capture capture --objects <对象区目录> --source <工作文件> --staging <名字> \
//!     --digest <64 位十六进制> --bytes <长度>
//! object-capture read --objects <对象区目录> --digest <64 位十六进制> --bytes <长度>
//! ```
//!
//! 退出码：0 成功；1 被拒绝（错误码写在 `code=` 行）；2 用法错误。

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use vaultsyncd_platform_linux_server::files::clone::{StageRequest, stage_copy};
use vaultsyncd_platform_linux_server::files::{
    self, CloneMethod as PlatformCloneMethod, CopyFallback as PlatformCopyFallback, FileIdentity,
    OBJECT_FILE_MODE, ResolveMode, RestrictError, RestrictedDir, StageError, StageFailure,
};
use vaultsyncd_server_core::objects::{
    CaptureRequest, ContentDigest, ExpectedContent, ObjectLayout, capture_object, read_object,
};
use vaultsyncd_server_core::ports::object_store::{
    BlobError, CopyFallback, CopyMethod, FileKey, ObjectBlobPort, PublishedBlob, StagedBlob,
    StorageFailure, StoredBytes, StoredFacts, StoredSlice,
};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let parsed = match parse(&command, args.collect()) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("用法错误：{message}");
            return ExitCode::from(2);
        }
    };
    let mut port = LinuxObjectPort::new(parsed.objects.clone());
    let result = match parsed.action {
        Action::Capture {
            source,
            staging,
            expected,
        } => capture(&mut port, &parsed.objects, source, staging, expected),
        Action::Read { digest, bytes } => read(&mut port, &parsed.objects, digest, bytes),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_code) => ExitCode::FAILURE,
    }
}

struct Parsed {
    action: Action,
    objects: PathBuf,
}

enum Action {
    Capture {
        source: PathBuf,
        staging: String,
        expected: ExpectedContent,
    },
    Read {
        digest: ContentDigest,
        bytes: u64,
    },
}

fn parse(command: &str, args: Vec<String>) -> Result<Parsed, String> {
    let mut objects: Option<PathBuf> = None;
    let mut source: Option<PathBuf> = None;
    let mut staging: Option<String> = None;
    let mut digest: Option<ContentDigest> = None;
    let mut bytes: Option<u64> = None;
    let mut iter = args.into_iter();
    while let Some(flag) = iter.next() {
        let value = iter.next().ok_or_else(|| format!("参数 {flag} 缺少取值"))?;
        match flag.as_str() {
            "--objects" => objects = Some(PathBuf::from(value)),
            "--source" => source = Some(PathBuf::from(value)),
            "--staging" => staging = Some(value),
            "--digest" => {
                digest = Some(ContentDigest::from_hex(&value).map_err(|error| error.to_string())?)
            }
            "--bytes" => bytes = Some(value.parse().map_err(|_| format!("长度不合法：{value}"))?),
            other => return Err(format!("未知参数 {other}")),
        }
    }
    let objects = objects.ok_or("缺少 --objects")?;
    let digest = digest.ok_or("缺少 --digest")?;
    let bytes = bytes.ok_or("缺少 --bytes")?;
    let action = match command {
        "capture" => Action::Capture {
            source: source.ok_or("capture 缺少 --source")?,
            staging: staging.ok_or("capture 缺少 --staging")?,
            expected: ExpectedContent::new(digest, bytes),
        },
        "read" => Action::Read { digest, bytes },
        other => return Err(format!("未知子命令 {other}（可用：capture、read）")),
    };
    Ok(Parsed { action, objects })
}

fn capture(
    port: &mut LinuxObjectPort,
    objects: &Path,
    source: PathBuf,
    staging: String,
    expected: ExpectedContent,
) -> Result<(), String> {
    let layout = ObjectLayout::new(objects);
    let request = CaptureRequest {
        source,
        staging_name: staging,
        expected,
    };
    match capture_object(port, &layout, &request) {
        Ok(captured) => {
            println!("decision=captured");
            println!("digest={}", captured.digest);
            println!("bytes={}", captured.bytes);
            println!("storage_path={}", captured.storage_path.display());
            println!("method={}", captured.method);
            println!("fallback={}", fallback_name(captured.fallback));
            println!("reuse={}", captured.reuse);
            println!("staging_cleanup={}", captured.staging);
            Ok(())
        }
        Err(failure) => {
            println!("decision=rejected");
            println!("code={}", failure.code());
            println!("detail={}", failure.reason);
            println!("staging_cleanup={}", failure.staging);
            Err(failure.code().to_owned())
        }
    }
}

fn read(
    port: &mut LinuxObjectPort,
    objects: &Path,
    digest: ContentDigest,
    bytes: u64,
) -> Result<(), String> {
    let layout = ObjectLayout::new(objects);
    match read_object(port, &layout, digest, bytes) {
        Ok(content) => {
            println!("decision=read");
            println!("digest={}", content.digest);
            println!("bytes={}", content.len());
            println!(
                "observed={}",
                files::hash::hex(&files::hash::digest(&content.data))
            );
            Ok(())
        }
        Err(failure) => {
            println!("decision=rejected");
            println!("code={}", failure.code());
            println!("detail={}", failure.reason);
            Err(failure.code().to_owned())
        }
    }
}

fn fallback_name(fallback: Option<CopyFallback>) -> String {
    match fallback {
        Some(CopyFallback::CrossDevice) => "exdev".to_owned(),
        Some(CopyFallback::ReflinkUnsupported) => "unsupported".to_owned(),
        None => "none".to_owned(),
    }
}

/// Linux 侧对象端口：暂存 `<objects>/staging/`，已发布对象 `<objects>/objects/<aa>/<hex>`。
struct LinuxObjectPort {
    objects: PathBuf,
}

impl LinuxObjectPort {
    fn new(objects: PathBuf) -> Self {
        LinuxObjectPort { objects }
    }

    fn staging_dir(&self) -> PathBuf {
        self.objects.join("staging")
    }

    fn prepare_dirs(&self, object: &Path) -> Result<(), BlobError> {
        let staging = self.staging_dir();
        fs::create_dir_all(&staging).map_err(|error| map_io("create_staging_dir", &error))?;
        if let Some(parent) = object.parent() {
            fs::create_dir_all(parent).map_err(|error| map_io("create_object_dir", &error))?;
        }
        Ok(())
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
        let file = fs::File::open(source).map_err(|error| map_io("open_source", &error))?;
        let source_identity =
            FileIdentity::of_fd(std::os::fd::AsRawFd::as_raw_fd(&file)).map_err(|error| {
                BlobError::SourceUnreadable {
                    detail: format!("读取源文件身份失败：{error}"),
                }
            })?;
        let staging_dir =
            RestrictedDir::open(&self.staging_dir(), ResolveMode::Auto).map_err(|error| {
                BlobError::Storage {
                    stage: "open_staging_dir",
                    failure: storage_failure_of_restrict(&error),
                    detail: error.to_string(),
                }
            })?;
        let staged = stage_copy(StageRequest {
            source: &file,
            source_identity: &source_identity,
            target_dir: &staging_dir,
            staging_name: &staging_name,
        })
        .map_err(|error| map_stage(&error))?;
        let staged_identity = FileIdentity::of_fd(std::os::fd::AsRawFd::as_raw_fd(&staged.file))
            .map_err(|error| BlobError::Verification {
                detail: format!("读取暂存副本身份失败：{error}"),
            })?;
        Ok(StagedBlob {
            staging: staging.to_path_buf(),
            source: key_of(&source_identity),
            staged: key_of(&staged_identity),
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
        let staging_dir =
            RestrictedDir::open(&self.staging_dir(), ResolveMode::Auto).map_err(|error| {
                BlobError::Storage {
                    stage: "open_staging_dir",
                    failure: storage_failure_of_restrict(&error),
                    detail: error.to_string(),
                }
            })?;
        let object_dir =
            RestrictedDir::open(object.parent().unwrap_or(&self.objects), ResolveMode::Auto)
                .map_err(|error| BlobError::Storage {
                    stage: "open_object_dir",
                    failure: storage_failure_of_restrict(&error),
                    detail: error.to_string(),
                })?;
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
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| map_io("seek_object", &error))?;
        let mut data = Vec::new();
        let read = file
            .take(length)
            .read_to_end(&mut data)
            .map_err(|error| map_io("read_object_range", &error))?;
        Ok(StoredSlice {
            bytes: read as u64,
            data,
        })
    }
}

fn key_of(identity: &FileIdentity) -> FileKey {
    FileKey {
        device: (u64::from(identity.dev_major) << 32) | u64::from(identity.dev_minor),
        inode: identity.inode,
        links: identity.nlink,
        bytes: identity.size,
    }
}

fn map_stage(error: &StageError) -> BlobError {
    match &error.reason {
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
    match restrict_io_error(error) {
        Some(source) => storage_failure_of_io(source),
        None => StorageFailure::Other,
    }
}

fn restrict_io_error(error: &RestrictError) -> Option<&io::Error> {
    match error {
        RestrictError::OpenRoot { source, .. }
        | RestrictError::Resolve { source, .. }
        | RestrictError::EscapesRoot { source, .. }
        | RestrictError::SymlinkRejected { source, .. }
        | RestrictError::NotFound { source, .. }
        | RestrictError::AlreadyExists { source, .. }
        | RestrictError::CrossDeviceRename { source, .. }
        | RestrictError::Rename { source, .. }
        | RestrictError::Unlink { source, .. }
        | RestrictError::Fsync { source, .. } => Some(source),
        _ => None,
    }
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
