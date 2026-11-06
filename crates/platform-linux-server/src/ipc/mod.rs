//! Linux 管理 socket：私有路径、内核 UID 身份与有界帧；不承载业务或凭据。

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// 单帧上限（请求与响应相同）。
pub const MAX_FRAME: usize = 1024 * 1024;

unsafe extern "C" {
    fn geteuid() -> u32;
}

/// 本进程的有效 UID，直接取内核身份而非环境变量。
#[must_use]
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid 无参数、无指针、不会失败。
    unsafe { geteuid() }
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "管理 socket 的路径权限或调用者身份不匹配",
    )
}

fn private_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != effective_uid() || meta.mode() & 0o777 != 0o700 {
        return Err(denied());
    }
    Ok(())
}

/// 持有已绑定 socket；停止时只删除本实例创建的 inode，不删除替换后的路径。
pub struct AdminSocket {
    /// 异步监听器。
    pub listener: UnixListener,
    path: PathBuf,
    inode: u64,
    // 绑定、chmod 与清理都锚定已打开的私有目录，防止父路径被替换后操作另一位置。
    _directory: fs::File,
}

impl AdminSocket {
    /// 在状态目录下创建 `admin/socket`；既有路径一律拒绝覆盖。
    pub fn bind(state_dir: &Path) -> io::Result<Self> {
        bind_impl(state_dir, false)
    }

    /// 在已持有状态目录独占锁时创建 `admin/socket`，并回收崩溃遗留的 socket。
    ///
    /// 安全前提由调用方保证：同一状态目录的独占锁（`server.lock`）在创建 socket 之前已经取得。
    /// 因此此刻已存在的 `admin/socket` 不可能是另一个活着的服务端或离线 init 创建的，只能是
    /// 上次进程被强杀后留下的 inode。回收仍保守：只删除锚定目录内、`lstat` 确认是 socket、
    /// 且属主为本进程有效 UID 的条目；普通文件、符号链接或他人属主一律明确报错而不删除。
    pub fn bind_after_state_lock(state_dir: &Path) -> io::Result<Self> {
        bind_impl(state_dir, true)
    }
}

fn bind_impl(state_dir: &Path, recover_stale: bool) -> io::Result<AdminSocket> {
    let open_dir = |path: &Path| {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(0o200_000 | 0o400_000)
            .open(path)
    }; // O_DIRECTORY | O_NOFOLLOW
    let state = open_dir(state_dir)?;
    let meta = state.metadata()?;
    if meta.uid() != effective_uid() || meta.mode() & 0o022 != 0 {
        return Err(denied());
    }
    let parent = PathBuf::from(format!("/proc/self/fd/{}/admin", state.as_raw_fd()));
    match fs::DirBuilder::new().mode(0o700).create(&parent) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error),
    }
    let directory = open_dir(&parent)?;
    let meta = directory.metadata()?;
    if meta.uid() != effective_uid() || meta.mode() & 0o777 != 0o700 {
        return Err(denied());
    }
    let path = PathBuf::from(format!("/proc/self/fd/{}/socket", directory.as_raw_fd()));
    if recover_stale {
        remove_stale_socket(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let inode = fs::symlink_metadata(&path)?.ino();
    Ok(AdminSocket {
        listener,
        path,
        inode,
        _directory: directory,
    })
}

/// 回收崩溃遗留的 socket；只处理锚定目录 fd 下的 socket 条目，不跟随符号链接。
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == effective_uid() => {
            fs::remove_file(path)
        }
        Ok(meta) if meta.file_type().is_socket() => Err(denied()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "管理 socket 路径已被非 socket 文件占用，拒绝删除",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

impl Drop for AdminSocket {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.ino() == self.inode
            && meta.file_type().is_socket()
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 接收请求前核验 SO_PEERCRED，仅服务进程相同有效 UID 可访问（不信任 JSON 身份）。
pub fn authorize_peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != effective_uid() {
        return Err(denied());
    }
    Ok(())
}

/// 客户端也校验路径权限、inode 与服务 UID，拒绝符号链接和假冒服务。
pub async fn connect(path: &Path) -> io::Result<UnixStream> {
    private_dir(path.parent().ok_or_else(denied)?)?;
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_socket()
        || before.uid() != effective_uid()
        || before.mode() & 0o777 != 0o600
    {
        return Err(denied());
    }
    let stream = UnixStream::connect(path).await?;
    authorize_peer(&stream)?;
    let after = fs::symlink_metadata(path)?;
    if before.ino() != after.ino() || before.dev() != after.dev() {
        return Err(denied());
    }
    Ok(stream)
}

/// 读取 u32 大端字节长度 + UTF-8 JSON 载荷；分配前拒绝零长或过大帧，截断必定报错。
pub async fn read_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "管理帧长度越界"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

/// 写出一个有界帧；超过上限不写任何字节。
pub async fn write_frame(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "管理帧长度越界"));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socket_path_is_private_and_existing_paths_are_never_overwritten() {
        let path =
            std::env::temp_dir().join(format!("vaultsyncd-s075-socket-{}", std::process::id()));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("admin")).unwrap();
        fs::set_permissions(path.join("admin"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(AdminSocket::bind(&path).is_err());
        fs::set_permissions(path.join("admin"), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = AdminSocket::bind(&path).unwrap();
        assert_eq!(
            fs::metadata(path.join("admin/socket")).unwrap().mode() & 0o777,
            0o600
        );
        let stream = connect(&path.join("admin/socket")).await.unwrap();
        authorize_peer(&stream).unwrap();
        assert!(AdminSocket::bind(&path).is_err());
        fs::set_permissions(path.join("admin/socket"), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(connect(&path.join("admin/socket")).await.is_err());
        fs::remove_file(path.join("admin/socket")).unwrap();
        fs::write(path.join("admin/socket"), b"replacement must survive").unwrap();
        drop(socket);
        assert_eq!(
            fs::read(path.join("admin/socket")).unwrap(),
            b"replacement must survive"
        );
        fs::remove_file(path.join("admin/socket")).unwrap();
        let socket = AdminSocket::bind(&path).unwrap();
        fs::rename(path.join("admin"), path.join("moved")).unwrap();
        fs::create_dir(path.join("admin")).unwrap();
        fs::write(path.join("admin/socket"), b"other directory").unwrap();
        drop(socket);
        assert!(!path.join("moved/socket").exists());
        assert_eq!(
            fs::read(path.join("admin/socket")).unwrap(),
            b"other directory"
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn stale_socket_is_reclaimed_but_non_socket_placeholders_are_kept() {
        let path = std::env::temp_dir().join(format!(
            "vaultsyncd-s075-stale-socket-{}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();

        // `forget` 等价于进程被强杀：Drop 不执行，socket 文件留在目录里。
        let first = AdminSocket::bind(&path).unwrap();
        std::mem::forget(first);
        assert!(
            fs::symlink_metadata(path.join("admin/socket"))
                .unwrap()
                .file_type()
                .is_socket()
        );

        let recovered = AdminSocket::bind_after_state_lock(&path).unwrap();
        let stream = connect(&path.join("admin/socket")).await.unwrap();
        authorize_peer(&stream).unwrap();
        drop(recovered);

        // 非 socket 文件占位：明确失败，且不删除、不覆盖。
        fs::write(path.join("admin/socket"), b"evidence must survive").unwrap();
        match AdminSocket::bind_after_state_lock(&path) {
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::AlreadyExists),
            Ok(_) => panic!("非 socket 占位必须拒绝启动"),
        }
        assert_eq!(
            fs::read(path.join("admin/socket")).unwrap(),
            b"evidence must survive"
        );
        fs::remove_file(path.join("admin/socket")).unwrap();

        // 符号链接同样不被跟随或删除。
        let outside = path.join("outside.sock");
        let _target = AdminSocket::bind(&path).unwrap();
        fs::rename(path.join("admin/socket"), &outside).unwrap();
        std::os::unix::fs::symlink(&outside, path.join("admin/socket")).unwrap();
        assert!(AdminSocket::bind_after_state_lock(&path).is_err());
        assert!(
            fs::symlink_metadata(path.join("admin/socket"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn bounded_frames_reject_oversize_zero_and_truncation() {
        for size in [0, MAX_FRAME as u32 + 1] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            a.write_u32(size).await.unwrap();
            assert_eq!(
                read_frame(&mut b).await.unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_u32(10).await.unwrap();
        a.write_all(b"xx").await.unwrap();
        drop(a);
        assert_eq!(
            read_frame(&mut b).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_frame(&mut a, b"[\"status\"]").await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), b"[\"status\"]");
        authorize_peer(&a).unwrap();
    }
}
