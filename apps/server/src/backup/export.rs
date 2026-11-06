//! 固定快照的有界字节流；清单授权先于文件打开，释放等待读者结束。
use super::jobs::{Context, View, failure};
use futures_util::{Stream, stream};
use std::{io, path::Path, sync::Arc};
use tokio::{io::AsyncReadExt, sync::OwnedRwLockReadGuard};
use vaultsyncd_domain::ids::{BackupLeaseId, UserId};
use vaultsyncd_server_core::backup::export::{self, CHUNK, Integrity};
/// HTTP与Unix导出共用的有界字节流。
pub type ByteStream = std::pin::Pin<Box<dyn Stream<Item = io::Result<Vec<u8>>> + Send>>;
struct Source {
    context: Arc<Context>,
    owner: UserId,
    id: BackupLeaseId,
    view: View,
    _guard: OwnedRwLockReadGuard<()>,
    archive: bool,
    phase: u8,
    manifest_at: usize,
    next: usize,
    file: Option<tokio::fs::File>,
    check: Option<Integrity>,
}
impl Source {
    async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            match self.phase {
                0 => {
                    self.phase = 1;
                    return Ok(Some(
                        export::prefix(self.view.manifest.len()).map_err(failure)?,
                    ));
                }
                1 => {
                    if self.manifest_at < self.view.manifest.len() {
                        let end = (self.manifest_at + CHUNK).min(self.view.manifest.len());
                        let bytes = self.view.manifest[self.manifest_at..end].to_vec();
                        self.manifest_at = end;
                        return Ok(Some(bytes));
                    }
                    self.phase = 2;
                }
                2 => {
                    let Some(item) = self.view.index.items.get(self.next) else {
                        self.phase = 4;
                        if self.archive {
                            return Ok(Some(vec![0; 4]));
                        }
                        return Ok(None);
                    };
                    let opened = self
                        .view
                        .directory
                        .open_read(Path::new(&item.path))
                        .map_err(failure)?;
                    if opened.identity.size != item.size {
                        return Err(failure("snapshot file size mismatch"));
                    }
                    self.file = Some(tokio::fs::File::from_std(opened.file));
                    self.check = Some(Integrity::new(item.clone()));
                    self.phase = 3;
                    if self.archive {
                        return Ok(Some(export::item_header(item)));
                    }
                }
                3 => {
                    let mut bytes = vec![0; CHUNK];
                    let n = self
                        .file
                        .as_mut()
                        .ok_or_else(|| failure("missing stream file"))?
                        .read(&mut bytes)
                        .await?;
                    if n == 0 {
                        self.check
                            .take()
                            .ok_or_else(|| failure("missing stream check"))?
                            .finish()
                            .map_err(failure)?;
                        self.file = None;
                        self.next += 1;
                        self.phase = 2;
                        continue;
                    }
                    bytes.truncate(n);
                    self.check
                        .as_mut()
                        .ok_or_else(|| failure("missing stream check"))?
                        .push(&bytes)
                        .map_err(failure)?;
                    return Ok(Some(bytes));
                }
                4 => return Ok(None),
                _ => return Err(failure("invalid export state")),
            }
        }
    }
}
impl Context {
    async fn source(
        self: &Arc<Self>,
        owner: UserId,
        id: BackupLeaseId,
        path: Option<&str>,
    ) -> io::Result<ByteStream> {
        let guard = self.lock(id).await.read_owned().await;
        let mut view = match self.view(owner, id).await {
            Ok(view) => view,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound
                    && error.kind() != io::ErrorKind::PermissionDenied
                {
                    self.incomplete(owner, id, &error.to_string()).await?;
                }
                return Err(error);
            }
        };
        let archive = path.is_none();
        if let Some(path) = path {
            let item = view.index.item(path).cloned().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "file is not in this backup manifest",
                )
            })?;
            view.index.items = vec![item];
        }
        let source = Source {
            context: Arc::clone(self),
            owner,
            id,
            view,
            _guard: guard,
            archive,
            phase: if archive { 0 } else { 2 },
            manifest_at: 0,
            next: 0,
            file: None,
            check: None,
        };
        Ok(Box::pin(stream::try_unfold(
            source,
            |mut state| async move {
                match state.next().await {
                    Ok(Some(bytes)) => Ok(Some((bytes, state))),
                    Ok(None) => Ok(None),
                    Err(error) => {
                        state
                            .context
                            .incomplete(state.owner, state.id, &error.to_string())
                            .await?;
                        Err(error)
                    }
                }
            },
        )))
    }
    /// `.vsbk`归档：所有项验证成功后才发最终零标记；客户端取消不会变成verified。
    pub async fn archive(
        self: &Arc<Self>,
        owner: UserId,
        id: BackupLeaseId,
    ) -> io::Result<ByteStream> {
        self.source(owner, id, None).await
    }
    /// 精确清单项下载；调用者不能指定任意服务器文件。
    pub async fn file(
        self: &Arc<Self>,
        owner: UserId,
        id: BackupLeaseId,
        path: &str,
    ) -> io::Result<ByteStream> {
        self.source(owner, id, Some(path)).await
    }
    /// 清单摘要和备份身份核对成功后返回原始字节，不能重新序列化改变固定摘要。
    pub async fn manifest(
        &self,
        owner: UserId,
        id: BackupLeaseId,
    ) -> io::Result<(Vec<u8>, [u8; 32])> {
        let _guard = self.lock(id).await.read_owned().await;
        let view = match self.view(owner, id).await {
            Ok(view) => view,
            Err(error) => {
                if !matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) {
                    self.incomplete(owner, id, &error.to_string()).await?;
                }
                return Err(error);
            }
        };
        Ok((view.manifest, view.index.manifest_hash))
    }
}
