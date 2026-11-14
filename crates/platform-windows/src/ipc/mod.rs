//! User/session scoped local IPC. This module does not initialize CFAPI.
//!
//! Each connection executes requests sequentially; connections execute concurrently. A whole
//! request (including partial header/body) and write have a five-second deadline. Ordinary handlers
//! also have five seconds; orderly stop has sixty seconds and explicit recovery/download/release/detach operations
//! are bounded by thirty minutes.
//! Malformed frames/JSON, peer mismatches and disconnects close only that connection.
mod security;
pub use security::ServerProcess;
use std::{future::Future, io, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    task::{JoinHandle, JoinSet},
};
use vaultsyncd_local_ipc::{
    framing,
    generated::{MAX_FRAME_BYTES, Request, Response},
};

/// Maximum active connections, excluding the single waiting listener.
pub const MAX_CONNECTIONS: usize = 8;
/// Deadline for one complete incoming frame, handler or response write.
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Endpoint derived from the real process token, not a caller-supplied SID.
#[derive(Debug, Clone)]
pub struct Endpoint {
    name: String,
    sid: String,
}
impl Endpoint {
    /// Application scope must be a short ASCII application identity (not a path).
    /// Production callers use `vaultsyncd-agent`; tests use a unique run scope.
    pub fn current(scope: &str) -> io::Result<Self> {
        if scope.is_empty()
            || scope.len() > 100
            || !scope
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid IPC scope",
            ));
        }
        let (sid, session) = security::identity()?;
        Ok(Self {
            name: format!(r"\\.\pipe\{scope}-{sid}-{session}"),
            sid,
        })
    }
    /// Local pipe name for native consumers. Always reject remote endpoints.
    pub fn name(&self) -> &str {
        &self.name
    }
    fn listener(&self, first: bool) -> io::Result<NamedPipeServer> {
        let security = security::Security::new(&self.sid)?;
        let mut attributes = security.attributes();
        // Descriptor and SECURITY_ATTRIBUTES remain live through the synchronous create call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .max_instances(MAX_CONNECTIONS + 2)
                .in_buffer_size(8192)
                .out_buffer_size(8192)
                .create_with_security_attributes_raw(
                    &self.name,
                    (&mut attributes as *mut security::Attributes).cast(),
                )
        }
    }
    /// Connect with identification-only impersonation and verify the server's token user.
    pub async fn connect(&self) -> io::Result<NamedPipeClient> {
        deadline(async {
            loop {
                match ClientOptions::new().open(&self.name) {
                    Ok(pipe) => {
                        security::check_server(&pipe, &self.sid)?;
                        return Ok(pipe);
                    }
                    Err(e) if e.raw_os_error() == Some(231) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
    }
    /// Pin the current authenticated server and verify its installed executable before maintenance.
    pub async fn maintenance_process(
        &self,
        expected: &std::path::Path,
    ) -> io::Result<ServerProcess> {
        let pipe = self.connect().await?;
        security::ServerProcess::capture(&pipe, &self.sid, expected)
    }
    /// Issue a single request and validate its response. GUI/CLI can use this without CFAPI.
    pub async fn request(&self, request: &Request) -> io::Result<Response> {
        let mut pipe = self.connect().await?;
        request_on(&mut pipe, request).await
    }
    /// Authenticate the token user on this exact connection and retain its original process.
    /// Ordinary session start/stop/health can reuse an agent from another program directory.
    /// File-replacing maintenance must still use `request_verified` or `maintenance_process`.
    pub async fn request_authenticated(
        &self,
        request: &Request,
    ) -> io::Result<(Response, ServerProcess)> {
        let mut pipe = self.connect().await?;
        let process = ServerProcess::capture_authenticated(&pipe, &self.sid)?;
        let response = request_on(&mut pipe, request).await?;
        Ok((response, process))
    }
    /// Verify the executable and token user on the request's connection before sending any bytes.
    /// Returns the original peer's pinned process handle only after validating its response.
    pub async fn request_verified(
        &self,
        expected: &Path,
        request: &Request,
    ) -> io::Result<(Response, ServerProcess)> {
        let mut pipe = self.connect().await?;
        let process = ServerProcess::capture(&pipe, &self.sid, expected)?;
        let response = request_on(&mut pipe, request).await?;
        Ok((response, process))
    }
}
async fn request_on(pipe: &mut NamedPipeClient, request: &Request) -> io::Result<Response> {
    deadline(write_json(pipe, request)).await?;
    let bytes = operation_deadline(request, read_frame(pipe)).await?;
    let response: Response = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    response.validate().map_err(io::Error::other)?;
    if response.request_id != request.request_id {
        return Err(io::Error::other("IPC response identity mismatch"));
    }
    Ok(response)
}
/// Owns the listener and bounded connection tasks. Drop cancels all pending I/O.
pub struct Server {
    task: JoinHandle<io::Result<()>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}
impl Server {
    /// Bind exclusively before returning. A preexisting pipe (including a squatter) is an error.
    /// Handler errors close the connection; the handler must not include secrets in responses.
    pub fn bind<F, Fut>(endpoint: Endpoint, handler: F) -> io::Result<Self>
    where
        F: Fn(Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = io::Result<Response>> + Send + 'static,
    {
        let mut listener = endpoint.listener(true)?;
        let handler = Arc::new(handler);
        let (stop, mut stopping) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                tokio::select! {
                    result = listener.connect() => {
                        result?;
                        // Keep at least one owned instance alive across replacement, so another
                        // process can never steal the name during normal connection churn.
                        let next = endpoint.listener(false)?;
                        let connected = std::mem::replace(&mut listener, next);
                        if clients.len() < MAX_CONNECTIONS {
                            let handler = handler.clone();
                            let endpoint = endpoint.clone();
                            clients.spawn(async move {
                                let _ = connection(connected, endpoint, handler).await;
                            });
                        } // Excess connections are closed without allocation or handler work.
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                    _ = &mut stopping => {
                        clients.shutdown().await;
                        return Ok(());
                    }
                }
            }
        });
        Ok(Self {
            task,
            stop: Some(stop),
        })
    }
    /// A stopped listener is a process-level fault; consumers must not claim IPC is available.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
    /// Stop accepting, cancel all partial requests and wait for listener/connection teardown.
    pub async fn shutdown(mut self) -> io::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match (&mut self.task).await {
            Ok(result) => result,
            Err(e) if e.is_cancelled() => Ok(()),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn connection<F, Fut>(
    mut pipe: NamedPipeServer,
    endpoint: Endpoint,
    handler: Arc<F>,
) -> io::Result<()>
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = io::Result<Response>> + Send,
{
    loop {
        let bytes = deadline(read_frame(&mut pipe)).await?;
        // Impersonation never spans an await, and is repeated for every request.
        security::check_client(&pipe, &endpoint.sid)?;
        let request: Request = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        let duration = operation_duration(&request);
        let response = tokio::time::timeout(duration, handler(request))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "IPC operation deadline exceeded")
            })??;
        response.validate().map_err(io::Error::other)?;
        deadline(write_json(&mut pipe, &response)).await?;
    }
}
fn operation_duration(request: &Request) -> Duration {
    if matches!(
        request.command.name,
        vaultsyncd_local_ipc::generated::CommandName::PrepareMaintenance
            | vaultsyncd_local_ipc::generated::CommandName::ExportRecovery
            | vaultsyncd_local_ipc::generated::CommandName::DownloadEntry
            | vaultsyncd_local_ipc::generated::CommandName::ReleaseEntry
            | vaultsyncd_local_ipc::generated::CommandName::DetachBinding
    ) {
        Duration::from_secs(1800)
    } else if request.command.name == vaultsyncd_local_ipc::generated::CommandName::StopAgent {
        Duration::from_secs(60)
    } else {
        IO_TIMEOUT
    }
}
async fn operation_deadline<T>(
    request: &Request,
    future: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(operation_duration(request), future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC operation deadline exceeded"))?
}
async fn deadline<T>(future: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(IO_TIMEOUT, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC deadline exceeded"))?
}
async fn read_frame(pipe: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    pipe.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC frame length",
        ));
    }
    let mut body = vec![0; length];
    pipe.read_exact(&mut body).await?;
    Ok(body)
}
async fn write_json(
    pipe: &mut (impl AsyncWrite + Unpin),
    value: &impl serde::Serialize,
) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    let frame = framing::encode(&body).map_err(io::Error::other)?;
    pipe.write_all(&frame).await
}
#[cfg(test)]
mod tests;
