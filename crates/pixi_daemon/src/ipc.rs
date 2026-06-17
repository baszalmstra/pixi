use std::{
    collections::HashMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use pixi_compute_fs::{FsError, GlobMTime, InputGlobMTimeProvider, InputGlobSpec};
use rattler_conda_types::GenericVirtualPackage;
use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{
        AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf,
        split,
    },
    process::{Child, Command},
    sync::{Mutex, oneshot, watch},
    time::sleep,
};

use crate::{DaemonError, WorkspaceFsDaemon};

const PROTOCOL_VERSION: u32 = 1;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const REQUEST_PING: &str = "ping";
const REQUEST_SHUTDOWN: &str = "shutdown";
const REQUEST_INPUT_GLOB_MTIME: &str = "input-glob-mtime";
const REQUEST_INPUT_GLOB_FILES: &str = "input-glob-files";
const REQUEST_SYSTEM_VIRTUAL_PACKAGES: &str = "system-virtual-packages";
const REQUEST_ENVELOPE: &str = "request";
const RESPONSE_ENVELOPE: &str = "response";
const RESPONSE_INPUT_GLOB_MTIME: &str = "input-glob-mtime";
const RESPONSE_SYSTEM_VIRTUAL_PACKAGES: &str = "system-virtual-packages";
const RESPONSE_STOPPING: &str = "stopping";

/// Deterministic local IPC address for a workspace daemon.
#[derive(Clone, Debug)]
pub struct DaemonEndpoint {
    workspace_root: PathBuf,
    workspace_id: String,
    address: EndpointAddress,
}

#[derive(Clone, Debug)]
enum EndpointAddress {
    #[cfg(unix)]
    UnixSocket(PathBuf),
    #[cfg(windows)]
    NamedPipe(String),
}

impl DaemonEndpoint {
    /// Create the endpoint used by the daemon serving `workspace_root`.
    pub fn for_workspace_root(workspace_root: impl AsRef<Path>) -> Result<Self, DaemonIpcError> {
        let requested_root = workspace_root.as_ref();
        let workspace_root = requested_root.canonicalize().map_err(|source| {
            DaemonIpcError::CanonicalizeWorkspaceRoot {
                path: requested_root.to_path_buf(),
                source,
            }
        })?;
        let workspace_id = workspace_id(&workspace_root);
        let address = platform_address(&workspace_id);

        Ok(Self {
            workspace_root,
            workspace_id,
            address,
        })
    }

    /// Canonical workspace root associated with this endpoint.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Stable workspace id used in the local IPC address.
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// Human-readable local IPC address.
    pub fn display_address(&self) -> String {
        match &self.address {
            #[cfg(unix)]
            EndpointAddress::UnixSocket(path) => path.display().to_string(),
            #[cfg(windows)]
            EndpointAddress::NamedPipe(name) => name.clone(),
        }
    }

    #[cfg(unix)]
    fn socket_path(&self) -> &Path {
        match &self.address {
            EndpointAddress::UnixSocket(path) => path,
        }
    }

    #[cfg(windows)]
    fn pipe_name(&self) -> &str {
        match &self.address {
            EndpointAddress::NamedPipe(name) => name,
        }
    }
}

/// A successful response to a daemon ping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonPong {
    /// Protocol version spoken by the daemon.
    pub protocol_version: u32,
    /// Canonical workspace root served by the daemon.
    pub workspace_root: PathBuf,
}

/// Current daemon status for a workspace endpoint.
#[derive(Clone, Debug)]
pub struct DaemonStatus {
    /// Endpoint checked by the status request.
    pub endpoint: DaemonEndpoint,
    /// Ping response if a daemon is accepting requests.
    pub pong: Option<DaemonPong>,
}

impl DaemonStatus {
    /// Whether a daemon accepted a ping at this endpoint.
    pub fn is_running(&self) -> bool {
        self.pong.is_some()
    }
}

/// Result of asking a daemon to stop.
#[derive(Clone, Debug)]
pub struct DaemonStopResult {
    /// Endpoint targeted by the stop request.
    pub endpoint: DaemonEndpoint,
    /// Whether a daemon accepted the stop request.
    pub stopped: bool,
}

/// Reusable client for a workspace daemon.
///
/// The client owns daemon startup policy and one persistent IPC connection.
/// Requests are sent with ids and responses are matched by a background reader,
/// so cloned clients can have multiple in-flight requests over the same
/// connection. The daemon runs each request concurrently and serializes only
/// response writes.
#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<DaemonClientInner>,
}

impl std::fmt::Debug for DaemonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonClient")
            .field("workspace_root", &self.inner.workspace_root)
            .field("can_spawn", &self.inner.spawn_options.is_some())
            .finish_non_exhaustive()
    }
}

struct DaemonClientInner {
    workspace_root: PathBuf,
    spawn_options: Option<SpawnDaemonOptions>,
    in_process: Option<Arc<WorkspaceFsDaemon>>,
    state: Mutex<DaemonClientState>,
    start_lock: Mutex<()>,
    next_request_id: AtomicU64,
}

struct DaemonClientState {
    endpoint: Option<DaemonEndpoint>,
    connection: Option<Arc<DaemonConnection>>,
}

struct DaemonConnection {
    writer: Mutex<WriteHalf<platform::Connection>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<WireResponseBody, DaemonIpcError>>>>>,
}

impl DaemonClient {
    /// Create a client that connects to an already-running daemon process.
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self::from_parts(workspace_root.as_ref().to_path_buf(), None, None)
    }

    /// Create a client that can start the daemon process while establishing its
    /// persistent connection. Request methods reuse this policy instead of each
    /// exposing its own auto-start variant.
    pub fn with_spawn_options(options: SpawnDaemonOptions) -> Self {
        Self::from_parts(options.workspace_root.clone(), Some(options), None)
    }

    /// Create a client backed by an in-process workspace daemon.
    pub fn in_process(workspace_root: impl AsRef<Path>) -> Result<Self, DaemonIpcError> {
        let daemon = Arc::new(WorkspaceFsDaemon::start(workspace_root)?);
        Ok(Self::from_parts(
            daemon.root().to_path_buf(),
            None,
            Some(daemon),
        ))
    }

    fn from_parts(
        workspace_root: PathBuf,
        spawn_options: Option<SpawnDaemonOptions>,
        in_process: Option<Arc<WorkspaceFsDaemon>>,
    ) -> Self {
        Self {
            inner: Arc::new(DaemonClientInner {
                workspace_root,
                spawn_options,
                in_process,
                state: Mutex::new(DaemonClientState {
                    endpoint: None,
                    connection: None,
                }),
                start_lock: Mutex::new(()),
                next_request_id: AtomicU64::new(1),
            }),
        }
    }

    /// Establish the persistent IPC connection if it is not already open.
    pub async fn connect(&self) -> Result<(), DaemonIpcError> {
        if self.inner.in_process.is_some() {
            return Ok(());
        }
        self.inner.connection().await.map(|_| ())
    }

    /// Ping the daemon over this client.
    pub async fn ping(&self) -> Result<DaemonPong, DaemonIpcError> {
        if let Some(daemon) = &self.inner.in_process {
            return Ok(DaemonPong {
                protocol_version: PROTOCOL_VERSION,
                workspace_root: daemon.root().to_path_buf(),
            });
        }
        match self.inner.request(WireRequestKind::Ping).await? {
            WireResponseBody::Pong {
                protocol_version,
                workspace_root,
            } => Ok(DaemonPong {
                protocol_version,
                workspace_root,
            }),
            WireResponseBody::Error { message } => Err(DaemonIpcError::Remote { message }),
            other => Err(DaemonIpcError::UnexpectedResponse {
                expected: "multiplexed pong",
                response: format!("{other:?}"),
            }),
        }
    }

    /// Request input-glob mtime over this client's persistent connection.
    pub async fn input_glob_mtime(
        &self,
        root: impl AsRef<Path>,
        spec: InputGlobSpec,
    ) -> Result<GlobMTime, DaemonIpcError> {
        if let Some(daemon) = &self.inner.in_process {
            return Ok(daemon.input_glob_mtime(root, spec).await?.value);
        }
        match self
            .inner
            .request(WireRequestKind::InputGlobMTime {
                root: root.as_ref().to_path_buf(),
                spec,
            })
            .await?
        {
            WireResponseBody::InputGlobMTime {
                response: WireInputGlobMTimeResponse::Ok { value },
            } => Ok(value),
            WireResponseBody::InputGlobMTime {
                response: WireInputGlobMTimeResponse::Error { message },
            }
            | WireResponseBody::Error { message } => Err(DaemonIpcError::Remote { message }),
            other => Err(DaemonIpcError::UnexpectedResponse {
                expected: "multiplexed input-glob-mtime",
                response: format!("{other:?}"),
            }),
        }
    }

    /// Request exact input-glob file matches over this client.
    pub async fn input_glob_files(
        &self,
        root: impl AsRef<Path>,
        spec: InputGlobSpec,
    ) -> Result<Vec<PathBuf>, DaemonIpcError> {
        if let Some(daemon) = &self.inner.in_process {
            return Ok(daemon.input_glob_files(root, spec).await?);
        }
        match self
            .inner
            .request(WireRequestKind::InputGlobFiles {
                root: root.as_ref().to_path_buf(),
                spec,
            })
            .await?
        {
            WireResponseBody::InputGlobFiles {
                response: WireInputGlobFilesResponse::Ok { paths },
            } => Ok(paths),
            WireResponseBody::InputGlobFiles {
                response: WireInputGlobFilesResponse::Error { message },
            }
            | WireResponseBody::Error { message } => Err(DaemonIpcError::Remote { message }),
            other => Err(DaemonIpcError::UnexpectedResponse {
                expected: "multiplexed input-glob-files",
                response: format!("{other:?}"),
            }),
        }
    }

    /// Detect raw system virtual packages in the daemon process.
    pub async fn system_virtual_packages(
        &self,
    ) -> Result<Vec<GenericVirtualPackage>, DaemonIpcError> {
        if self.inner.in_process.is_some() {
            return detect_system_virtual_packages().map_err(|error| DaemonIpcError::Remote {
                message: error.to_string(),
            });
        }
        match self
            .inner
            .request(WireRequestKind::SystemVirtualPackages)
            .await?
        {
            WireResponseBody::SystemVirtualPackages {
                response: WireSystemVirtualPackagesResponse::Ok { virtual_packages },
            } => Ok(virtual_packages),
            WireResponseBody::SystemVirtualPackages {
                response: WireSystemVirtualPackagesResponse::Error { message },
            }
            | WireResponseBody::Error { message } => Err(DaemonIpcError::Remote { message }),
            other => Err(DaemonIpcError::UnexpectedResponse {
                expected: "multiplexed system-virtual-packages",
                response: format!("{other:?}"),
            }),
        }
    }
}

impl DaemonClientInner {
    async fn endpoint(&self) -> Result<DaemonEndpoint, DaemonIpcError> {
        let mut state = self.state.lock().await;
        if let Some(endpoint) = &state.endpoint {
            return Ok(endpoint.clone());
        }
        let endpoint = DaemonEndpoint::for_workspace_root(&self.workspace_root)?;
        state.endpoint = Some(endpoint.clone());
        Ok(endpoint)
    }

    async fn connection(&self) -> Result<Arc<DaemonConnection>, DaemonIpcError> {
        if let Some(connection) = self.state.lock().await.connection.clone() {
            return Ok(connection);
        }
        self.connect_new_connection().await
    }

    async fn connect_new_connection(&self) -> Result<Arc<DaemonConnection>, DaemonIpcError> {
        let endpoint = self.endpoint().await?;
        match self.connect_endpoint(&endpoint).await {
            Ok(connection) => Ok(connection),
            Err(DaemonIpcError::Io { .. }) if self.spawn_options.is_some() => {
                let _guard = self.start_lock.lock().await;
                if let Some(connection) = self.state.lock().await.connection.clone() {
                    return Ok(connection);
                }
                if let Ok(connection) = self.connect_endpoint(&endpoint).await {
                    return Ok(connection);
                }
                if let Some(options) = self.spawn_options.clone() {
                    ensure_daemon_running(options).await?;
                }
                self.connect_endpoint(&endpoint).await
            }
            Err(error) => Err(error),
        }
    }

    async fn connect_endpoint(
        &self,
        endpoint: &DaemonEndpoint,
    ) -> Result<Arc<DaemonConnection>, DaemonIpcError> {
        let connect_started = Instant::now();
        let stream = platform::connect_endpoint(endpoint)
            .await
            .map_err(|source| DaemonIpcError::Io {
                context: format!(
                    "failed to connect to daemon at {}",
                    endpoint.display_address()
                ),
                source,
            })?;
        let connect_elapsed = connect_started.elapsed();
        tracing::info!(
            endpoint = %endpoint.display_address(),
            ?connect_elapsed,
            "established multiplexed daemon IPC connection",
        );

        let (reader, writer) = split(stream);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(read_multiplexed_responses(
            reader,
            pending.clone(),
            endpoint.clone(),
        ));

        let connection = Arc::new(DaemonConnection {
            writer: Mutex::new(writer),
            pending,
        });
        self.state.lock().await.connection = Some(connection.clone());
        Ok(connection)
    }

    async fn request(&self, kind: WireRequestKind) -> Result<WireResponseBody, DaemonIpcError> {
        match self.request_once(kind.clone()).await {
            Ok(response) => Ok(response),
            Err(error) if error.is_retryable_io() => {
                tracing::warn!(
                    %error,
                    request = kind.name(),
                    "daemon IPC connection was dropped while in use; reconnecting and retrying once",
                );
                self.request_once(kind).await
            }
            Err(error) => Err(error),
        }
    }

    async fn request_once(
        &self,
        kind: WireRequestKind,
    ) -> Result<WireResponseBody, DaemonIpcError> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let request_name = kind.name();
        let envelope = WireRequestEnvelope {
            id: request_id,
            kind,
        };
        let payload = serde_json::to_string(&envelope).map_err(|source| DaemonIpcError::Io {
            context: "failed to serialize daemon request envelope".to_owned(),
            source: io::Error::other(source),
        })?;
        let line = format!("{REQUEST_ENVELOPE} {payload}\n");

        let connection = self.connection().await?;
        let (tx, rx) = oneshot::channel();
        connection.pending.lock().await.insert(request_id, tx);

        let write_result = async {
            let mut writer = connection.writer.lock().await;
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await
        }
        .await;

        if let Err(source) = write_result {
            connection.pending.lock().await.remove(&request_id);
            self.drop_connection(&connection).await;
            return Err(DaemonIpcError::Io {
                context: format!(
                    "failed to write {request_name} request to daemon at {}",
                    self.endpoint().await?.display_address()
                ),
                source,
            });
        }

        match rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => {
                self.drop_connection(&connection).await;
                Err(error)
            }
            Err(_) => {
                self.drop_connection(&connection).await;
                Err(DaemonIpcError::Io {
                    context: format!(
                        "daemon connection closed before {request_name} response from {}",
                        self.endpoint().await?.display_address()
                    ),
                    source: io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "daemon closed connection",
                    ),
                })
            }
        }
    }

    async fn drop_connection(&self, connection: &Arc<DaemonConnection>) {
        let mut state = self.state.lock().await;
        if state
            .connection
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, connection))
        {
            state.connection = None;
        }
    }
}

async fn read_multiplexed_responses(
    reader: ReadHalf<platform::Connection>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<WireResponseBody, DaemonIpcError>>>>>,
    endpoint: DaemonEndpoint,
) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => match parse_response_envelope(&line) {
                Ok(response) => {
                    if let Some(tx) = pending.lock().await.remove(&response.id) {
                        let _ = tx.send(Ok(response.body));
                    } else {
                        tracing::debug!(
                            response_id = response.id,
                            endpoint = %endpoint.display_address(),
                            "received daemon response for unknown request id",
                        );
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        %error,
                        endpoint = %endpoint.display_address(),
                        response = %line.trim_end_matches(['\r', '\n']),
                        "failed to parse multiplexed daemon response",
                    );
                }
            },
            Err(source) => {
                let mut pending = pending.lock().await;
                let pending_count = pending.len();
                if pending_count > 0 {
                    tracing::warn!(
                        endpoint = %endpoint.display_address(),
                        pending_requests = pending_count,
                        %source,
                        "daemon IPC connection was dropped while requests were in flight",
                    );
                }
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(DaemonIpcError::Io {
                        context: format!(
                            "failed to read response from daemon at {}",
                            endpoint.display_address()
                        ),
                        source: io::Error::new(source.kind(), source.to_string()),
                    }));
                }
                return;
            }
        }
    }

    let mut pending = pending.lock().await;
    let pending_count = pending.len();
    if pending_count > 0 {
        tracing::warn!(
            endpoint = %endpoint.display_address(),
            pending_requests = pending_count,
            "daemon IPC connection closed while requests were in flight",
        );
    }
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(DaemonIpcError::Io {
            context: format!("daemon at {} closed connection", endpoint.display_address()),
            source: io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed connection"),
        }));
    }
}

/// Compute-fs provider that answers input-glob mtime requests through the
/// workspace daemon process.
#[derive(Clone, Debug)]
pub struct IpcInputGlobMTimeProvider {
    client: DaemonClient,
}

impl IpcInputGlobMTimeProvider {
    /// Create a provider that uses a multiplexed daemon client connection,
    /// starting the daemon while connecting if no daemon is already accepting
    /// connections.
    pub fn new(workspace_root: impl AsRef<Path>, executable: impl AsRef<Path>) -> Self {
        Self {
            client: DaemonClient::with_spawn_options(SpawnDaemonOptions::new(
                workspace_root,
                executable,
            )),
        }
    }

    async fn request(&self, root: PathBuf, spec: InputGlobSpec) -> Result<GlobMTime, FsError> {
        self.client
            .input_glob_mtime(root, spec)
            .await
            .map_err(fs_error_from_ipc)
    }
}

impl InputGlobMTimeProvider for IpcInputGlobMTimeProvider {
    fn input_glob_mtime(
        &self,
        root: PathBuf,
        spec: InputGlobSpec,
    ) -> futures::future::BoxFuture<'static, Result<GlobMTime, FsError>> {
        let provider = self.clone();
        Box::pin(async move { provider.request(root, spec).await })
    }
}

impl InputGlobMTimeProvider for DaemonClient {
    fn input_glob_mtime(
        &self,
        root: PathBuf,
        spec: InputGlobSpec,
    ) -> futures::future::BoxFuture<'static, Result<GlobMTime, FsError>> {
        let client = self.clone();
        Box::pin(async move {
            client
                .input_glob_mtime(root, spec)
                .await
                .map_err(fs_error_from_ipc)
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct WireRequestEnvelope {
    id: u64,
    kind: WireRequestKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum WireRequestKind {
    Ping,
    Shutdown,
    InputGlobMTime { root: PathBuf, spec: InputGlobSpec },
    InputGlobFiles { root: PathBuf, spec: InputGlobSpec },
    SystemVirtualPackages,
}

impl WireRequestKind {
    fn name(&self) -> &'static str {
        match self {
            Self::Ping => REQUEST_PING,
            Self::Shutdown => REQUEST_SHUTDOWN,
            Self::InputGlobMTime { .. } => REQUEST_INPUT_GLOB_MTIME,
            Self::InputGlobFiles { .. } => REQUEST_INPUT_GLOB_FILES,
            Self::SystemVirtualPackages => REQUEST_SYSTEM_VIRTUAL_PACKAGES,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct WireResponseEnvelope {
    id: u64,
    body: WireResponseBody,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum WireResponseBody {
    Pong {
        protocol_version: u32,
        workspace_root: PathBuf,
    },
    Stopping,
    InputGlobMTime {
        response: WireInputGlobMTimeResponse,
    },
    InputGlobFiles {
        response: WireInputGlobFilesResponse,
    },
    SystemVirtualPackages {
        response: WireSystemVirtualPackagesResponse,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct WireInputGlobMTimeRequest {
    root: PathBuf,
    spec: InputGlobSpec,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum WireInputGlobMTimeResponse {
    Ok { value: GlobMTime },
    Error { message: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum WireInputGlobFilesResponse {
    Ok { paths: Vec<PathBuf> },
    Error { message: String },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
enum WireSystemVirtualPackagesResponse {
    Ok {
        virtual_packages: Vec<GenericVirtualPackage>,
    },
    Error {
        message: String,
    },
}

fn fs_error_from_ipc(error: DaemonIpcError) -> FsError {
    FsError::Indexed {
        message: format!("daemon input-glob mtime request failed: {error}"),
    }
}

/// Options for spawning a daemon process and waiting for it to accept pings.
#[derive(Clone, Debug)]
pub struct SpawnDaemonOptions {
    /// Canonical or canonicalizable workspace root.
    pub workspace_root: PathBuf,
    /// Pixi executable to spawn in daemon mode.
    pub executable: PathBuf,
    /// How long to wait for the spawned daemon to accept a ping.
    pub connect_timeout: Duration,
}

impl SpawnDaemonOptions {
    /// Construct spawn options using the default connect timeout.
    pub fn new(workspace_root: impl AsRef<Path>, executable: impl AsRef<Path>) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            executable: executable.as_ref().to_path_buf(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// Override the connect timeout.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }
}

/// A daemon connection established by [`spawn_daemon_and_connect`].
///
/// If this helper started a child daemon process, the child is killed when this
/// handle is dropped. Production auto-start can later choose a detached variant;
/// this first slice intentionally keeps spawned daemons scoped to the caller.
pub struct SpawnedDaemon {
    endpoint: DaemonEndpoint,
    child: Option<Child>,
}

impl SpawnedDaemon {
    /// Endpoint that accepted the ping.
    pub fn endpoint(&self) -> &DaemonEndpoint {
        &self.endpoint
    }

    /// Whether this handle spawned a new process instead of reusing an existing daemon.
    pub fn spawned(&self) -> bool {
        self.child.is_some()
    }

    /// Kill the spawned daemon process, if this handle owns one.
    pub async fn kill_spawned(mut self) -> Result<(), DaemonIpcError> {
        if let Some(mut child) = self.child.take() {
            child.kill().await.map_err(|source| DaemonIpcError::Io {
                context: "failed to kill spawned daemon".to_owned(),
                source,
            })?;
        }
        Ok(())
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

/// Errors returned by the daemon process/IPC layer.
#[derive(Debug, Error)]
pub enum DaemonIpcError {
    #[error("failed to canonicalize daemon workspace root {path}: {source}")]
    CanonicalizeWorkspaceRoot { path: PathBuf, source: io::Error },
    #[error("daemon is already running for workspace {workspace_root} at {endpoint}")]
    AlreadyRunning {
        workspace_root: PathBuf,
        endpoint: String,
    },
    #[error("daemon IPC operation failed: {context}: {source}")]
    Io { context: String, source: io::Error },
    #[error("daemon at {endpoint} did not become ready within {timeout:?}")]
    ConnectTimeout { endpoint: String, timeout: Duration },
    #[error("spawned daemon exited before accepting connections: {status}")]
    SpawnedExited { status: ExitStatus },
    #[error("daemon responded with {response:?} instead of {expected}")]
    UnexpectedResponse {
        expected: &'static str,
        response: String,
    },
    #[error("daemon returned an error: {message}")]
    Remote { message: String },
    #[error(transparent)]
    Daemon(#[from] DaemonError),
}

impl DaemonIpcError {
    fn is_retryable_io(&self) -> bool {
        matches!(self, Self::Io { .. })
    }
}

/// Serve daemon IPC for `workspace_root` until the process exits.
pub async fn serve_workspace(workspace_root: impl AsRef<Path>) -> Result<(), DaemonIpcError> {
    serve_workspace_until_shutdown(workspace_root, std::future::pending::<()>()).await
}

/// Serve daemon IPC for `workspace_root` until `shutdown` resolves.
pub async fn serve_workspace_until_shutdown(
    workspace_root: impl AsRef<Path>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(workspace_root)?;
    if ping_endpoint(&endpoint).await.is_ok() {
        return Err(DaemonIpcError::AlreadyRunning {
            workspace_root: endpoint.workspace_root().to_path_buf(),
            endpoint: endpoint.display_address(),
        });
    }

    let daemon = Arc::new(WorkspaceFsDaemon::start(endpoint.workspace_root())?);
    tracing::info!(
        workspace_root = %endpoint.workspace_root().display(),
        endpoint = %endpoint.display_address(),
        "starting pixi workspace daemon"
    );
    let result = serve_endpoint_until_shutdown(endpoint.clone(), daemon.clone(), shutdown).await;
    drop(daemon);
    result
}

/// Ping a daemon for `workspace_root`.
pub async fn ping_workspace(
    workspace_root: impl AsRef<Path>,
) -> Result<DaemonPong, DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(workspace_root)?;
    ping_endpoint(&endpoint).await
}

/// Return daemon status for `workspace_root`.
pub async fn status_workspace(
    workspace_root: impl AsRef<Path>,
) -> Result<DaemonStatus, DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(workspace_root)?;
    let pong = match ping_endpoint(&endpoint).await {
        Ok(pong) => Some(pong),
        Err(DaemonIpcError::Io { .. }) => None,
        Err(error) => return Err(error),
    };
    Ok(DaemonStatus { endpoint, pong })
}

/// Ask a daemon for `workspace_root` to stop.
pub async fn stop_workspace(
    workspace_root: impl AsRef<Path>,
) -> Result<DaemonStopResult, DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(workspace_root)?;
    match request_endpoint(&endpoint, REQUEST_SHUTDOWN).await {
        Ok(response) if response.trim_end_matches(['\r', '\n']) == RESPONSE_STOPPING => {
            Ok(DaemonStopResult {
                endpoint,
                stopped: true,
            })
        }
        Ok(response) => Err(DaemonIpcError::UnexpectedResponse {
            expected: RESPONSE_STOPPING,
            response: response.trim_end_matches(['\r', '\n']).to_owned(),
        }),
        Err(DaemonIpcError::Io { .. }) => Ok(DaemonStopResult {
            endpoint,
            stopped: false,
        }),
        Err(error) => Err(error),
    }
}

/// Request input-glob mtime from the daemon serving `workspace_root`.
pub async fn input_glob_mtime_workspace(
    workspace_root: impl AsRef<Path>,
    root: impl AsRef<Path>,
    spec: InputGlobSpec,
) -> Result<GlobMTime, DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(workspace_root)?;
    input_glob_mtime_endpoint(&endpoint, root.as_ref().to_path_buf(), spec).await
}

/// Detect system virtual packages in the daemon process without applying
/// `CONDA_OVERRIDE_*` environment variable overrides. Callers that honor
/// overrides should apply their own process environment after this returns.
pub async fn system_virtual_packages_workspace(
    workspace_root: impl AsRef<Path>,
) -> Result<Vec<GenericVirtualPackage>, DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(workspace_root)?;
    system_virtual_packages_endpoint(&endpoint).await
}

/// Spawn the Pixi executable in daemon mode if necessary and wait until it accepts a ping.
pub async fn ensure_daemon_running(
    options: SpawnDaemonOptions,
) -> Result<DaemonEndpoint, DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(&options.workspace_root)?;

    if ping_endpoint(&endpoint).await.is_ok() {
        return Ok(endpoint);
    }

    let mut child = spawn_daemon_child(&options, &endpoint)?;
    let deadline = Instant::now() + options.connect_timeout;
    loop {
        if ping_endpoint(&endpoint).await.is_ok() {
            return Ok(endpoint);
        }

        if let Some(status) = child.try_wait().map_err(|source| DaemonIpcError::Io {
            context: "failed to poll spawned daemon".to_owned(),
            source,
        })? {
            if ping_endpoint(&endpoint).await.is_ok() {
                return Ok(endpoint);
            }
            return Err(DaemonIpcError::SpawnedExited { status });
        }

        if Instant::now() >= deadline {
            let _ = child.start_kill();
            return Err(DaemonIpcError::ConnectTimeout {
                endpoint: endpoint.display_address(),
                timeout: options.connect_timeout,
            });
        }

        sleep(CONNECT_RETRY_INTERVAL).await;
    }
}

/// Spawn the Pixi executable in daemon mode and wait until it accepts a ping.
pub async fn spawn_daemon_and_connect(
    options: SpawnDaemonOptions,
) -> Result<SpawnedDaemon, DaemonIpcError> {
    let endpoint = DaemonEndpoint::for_workspace_root(&options.workspace_root)?;

    if ping_endpoint(&endpoint).await.is_ok() {
        return Ok(SpawnedDaemon {
            endpoint,
            child: None,
        });
    }

    let mut child = spawn_daemon_child(&options, &endpoint)?;

    let deadline = Instant::now() + options.connect_timeout;
    loop {
        if let Ok(_pong) = ping_endpoint(&endpoint).await {
            return Ok(SpawnedDaemon {
                endpoint,
                child: Some(child),
            });
        }

        if let Some(status) = child.try_wait().map_err(|source| DaemonIpcError::Io {
            context: "failed to poll spawned daemon".to_owned(),
            source,
        })? {
            return Err(DaemonIpcError::SpawnedExited { status });
        }

        if Instant::now() >= deadline {
            let _ = child.start_kill();
            return Err(DaemonIpcError::ConnectTimeout {
                endpoint: endpoint.display_address(),
                timeout: options.connect_timeout,
            });
        }

        sleep(CONNECT_RETRY_INTERVAL).await;
    }
}

fn spawn_daemon_child(
    options: &SpawnDaemonOptions,
    endpoint: &DaemonEndpoint,
) -> Result<Child, DaemonIpcError> {
    Command::new(&options.executable)
        .arg("daemon")
        .arg("--root")
        .arg(endpoint.workspace_root())
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|source| DaemonIpcError::Io {
            context: format!(
                "failed to spawn daemon executable {}",
                options.executable.display()
            ),
            source,
        })
}

async fn serve_endpoint_until_shutdown(
    endpoint: DaemonEndpoint,
    daemon: Arc<WorkspaceFsDaemon>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), DaemonIpcError> {
    platform::serve_endpoint_until_shutdown(endpoint, daemon, shutdown).await
}

async fn handle_connection<S>(
    stream: S,
    workspace_root: PathBuf,
    daemon: Arc<WorkspaceFsDaemon>,
    shutdown_tx: watch::Sender<bool>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = split(stream);
    let mut reader = BufReader::new(reader);
    let writer = Arc::new(Mutex::new(writer));

    loop {
        let mut request = String::new();
        if reader.read_line(&mut request).await? == 0 {
            return Ok(());
        }
        let request = request.trim_end_matches(['\r', '\n']).to_owned();

        if let Some(payload) = request
            .strip_prefix(REQUEST_ENVELOPE)
            .and_then(|rest| rest.strip_prefix(' '))
        {
            match serde_json::from_str::<WireRequestEnvelope>(payload) {
                Ok(envelope) => {
                    let writer = writer.clone();
                    let workspace_root = workspace_root.clone();
                    let daemon = daemon.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    tokio::spawn(async move {
                        let response = handle_multiplexed_request(
                            envelope,
                            workspace_root,
                            daemon,
                            shutdown_tx,
                        )
                        .await;
                        if let Err(error) = write_response_envelope(writer, response).await {
                            tracing::debug!(%error, "failed to write multiplexed daemon response");
                        }
                    });
                }
                Err(error) => {
                    let mut writer = writer.lock().await;
                    writer.write_all(b"error invalid-request-envelope ").await?;
                    writer.write_all(error.to_string().as_bytes()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await?;
                }
            }
            continue;
        }

        let mut writer = writer.lock().await;
        if handle_legacy_request(
            &request,
            &mut *writer,
            workspace_root.clone(),
            daemon.clone(),
            shutdown_tx.clone(),
        )
        .await?
        {
            return Ok(());
        }
    }
}

async fn write_response_envelope<W>(
    writer: Arc<Mutex<W>>,
    response: WireResponseEnvelope,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_string(&response).map_err(io::Error::other)?;
    let mut writer = writer.lock().await;
    writer.write_all(RESPONSE_ENVELOPE.as_bytes()).await?;
    writer.write_all(b" ").await?;
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn handle_multiplexed_request(
    request: WireRequestEnvelope,
    workspace_root: PathBuf,
    daemon: Arc<WorkspaceFsDaemon>,
    shutdown_tx: watch::Sender<bool>,
) -> WireResponseEnvelope {
    let body = match request.kind {
        WireRequestKind::Ping => WireResponseBody::Pong {
            protocol_version: PROTOCOL_VERSION,
            workspace_root,
        },
        WireRequestKind::Shutdown => {
            let _ = shutdown_tx.send(true);
            WireResponseBody::Stopping
        }
        WireRequestKind::InputGlobMTime { root, spec } => {
            let response = match daemon.input_glob_mtime(root, spec).await {
                Ok(response) => WireInputGlobMTimeResponse::Ok {
                    value: response.value,
                },
                Err(error) => WireInputGlobMTimeResponse::Error {
                    message: error.to_string(),
                },
            };
            WireResponseBody::InputGlobMTime { response }
        }
        WireRequestKind::InputGlobFiles { root, spec } => {
            let response = match daemon.input_glob_files(root, spec).await {
                Ok(paths) => WireInputGlobFilesResponse::Ok { paths },
                Err(error) => WireInputGlobFilesResponse::Error {
                    message: error.to_string(),
                },
            };
            WireResponseBody::InputGlobFiles { response }
        }
        WireRequestKind::SystemVirtualPackages => {
            let response = match detect_system_virtual_packages() {
                Ok(virtual_packages) => WireSystemVirtualPackagesResponse::Ok { virtual_packages },
                Err(error) => WireSystemVirtualPackagesResponse::Error {
                    message: error.to_string(),
                },
            };
            WireResponseBody::SystemVirtualPackages { response }
        }
    };

    WireResponseEnvelope {
        id: request.id,
        body,
    }
}

async fn handle_legacy_request<W>(
    request: &str,
    stream: &mut W,
    workspace_root: PathBuf,
    daemon: Arc<WorkspaceFsDaemon>,
    shutdown_tx: watch::Sender<bool>,
) -> io::Result<bool>
where
    W: AsyncWrite + Unpin,
{
    match request {
        REQUEST_PING => {
            stream.write_all(b"pong ").await?;
            stream
                .write_all(PROTOCOL_VERSION.to_string().as_bytes())
                .await?;
            stream.write_all(b" ").await?;
            stream
                .write_all(workspace_root.to_string_lossy().as_bytes())
                .await?;
            stream.write_all(b"\n").await?;
        }
        REQUEST_SHUTDOWN => {
            stream.write_all(RESPONSE_STOPPING.as_bytes()).await?;
            stream.write_all(b"\n").await?;
            stream.flush().await?;
            let _ = shutdown_tx.send(true);
            return Ok(true);
        }
        request if request.starts_with(REQUEST_INPUT_GLOB_MTIME) => {
            let response = handle_input_glob_mtime_request(request, daemon).await;
            stream
                .write_all(RESPONSE_INPUT_GLOB_MTIME.as_bytes())
                .await?;
            stream.write_all(b" ").await?;
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(b"\n").await?;
        }
        request if request.starts_with(REQUEST_SYSTEM_VIRTUAL_PACKAGES) => {
            let response = handle_system_virtual_packages_request(request).await;
            stream
                .write_all(RESPONSE_SYSTEM_VIRTUAL_PACKAGES.as_bytes())
                .await?;
            stream.write_all(b" ").await?;
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(b"\n").await?;
        }
        _ => stream.write_all(b"error unknown-request\n").await?,
    }

    stream.flush().await?;
    Ok(false)
}

async fn handle_input_glob_mtime_request(request: &str, daemon: Arc<WorkspaceFsDaemon>) -> String {
    let response = async {
        let payload = request
            .strip_prefix(REQUEST_INPUT_GLOB_MTIME)
            .and_then(|rest| rest.strip_prefix(' '))
            .ok_or_else(|| "missing input-glob-mtime payload".to_owned())?;
        let request: WireInputGlobMTimeRequest =
            serde_json::from_str(payload).map_err(|error| error.to_string())?;
        let value = daemon
            .input_glob_mtime(request.root, request.spec)
            .await
            .map_err(|error| error.to_string())?
            .value;
        Ok::<_, String>(WireInputGlobMTimeResponse::Ok { value })
    }
    .await
    .unwrap_or_else(|message| WireInputGlobMTimeResponse::Error { message });

    serde_json::to_string(&response)
        .unwrap_or_else(|error| format!(r#"{{"status":"error","message":"{error}"}}"#))
}

async fn handle_system_virtual_packages_request(request: &str) -> String {
    let response = async {
        if request != REQUEST_SYSTEM_VIRTUAL_PACKAGES {
            return Err("unexpected system-virtual-packages payload".to_owned());
        }
        let virtual_packages =
            detect_system_virtual_packages().map_err(|error| error.to_string())?;
        Ok::<_, String>(WireSystemVirtualPackagesResponse::Ok { virtual_packages })
    }
    .await
    .unwrap_or_else(|message| WireSystemVirtualPackagesResponse::Error { message });

    serde_json::to_string(&response)
        .unwrap_or_else(|error| format!(r#"{{"status":"error","message":"{error}"}}"#))
}

fn detect_system_virtual_packages()
-> Result<Vec<GenericVirtualPackage>, rattler_virtual_packages::DetectVirtualPackageError> {
    Ok(
        VirtualPackages::detect(&VirtualPackageOverrides::default())?
            .into_generic_virtual_packages()
            .collect(),
    )
}

async fn ping_endpoint(endpoint: &DaemonEndpoint) -> Result<DaemonPong, DaemonIpcError> {
    let response = request_endpoint(endpoint, REQUEST_PING).await?;
    parse_pong(&response)
}

async fn input_glob_mtime_endpoint(
    endpoint: &DaemonEndpoint,
    root: PathBuf,
    spec: InputGlobSpec,
) -> Result<GlobMTime, DaemonIpcError> {
    let payload =
        serde_json::to_string(&WireInputGlobMTimeRequest { root, spec }).map_err(|source| {
            DaemonIpcError::Io {
                context: "failed to serialize input-glob-mtime request".to_owned(),
                source: io::Error::other(source),
            }
        })?;
    let request = format!("{REQUEST_INPUT_GLOB_MTIME} {payload}");
    let response = request_endpoint_owned(endpoint, request).await?;
    parse_input_glob_mtime_response(&response)
}

async fn system_virtual_packages_endpoint(
    endpoint: &DaemonEndpoint,
) -> Result<Vec<GenericVirtualPackage>, DaemonIpcError> {
    let response = request_endpoint(endpoint, REQUEST_SYSTEM_VIRTUAL_PACKAGES).await?;
    parse_system_virtual_packages_response(&response)
}

async fn request_endpoint(
    endpoint: &DaemonEndpoint,
    request: &'static str,
) -> Result<String, DaemonIpcError> {
    request_endpoint_owned(endpoint, request.to_owned()).await
}

async fn request_endpoint_owned(
    endpoint: &DaemonEndpoint,
    request: String,
) -> Result<String, DaemonIpcError> {
    let request_name = request
        .split_once(' ')
        .map_or(request.as_str(), |(name, _)| name)
        .to_owned();
    let connect_started = Instant::now();
    let mut stream = platform::connect_endpoint(endpoint)
        .await
        .map_err(|source| DaemonIpcError::Io {
            context: format!(
                "failed to connect to daemon at {}",
                endpoint.display_address()
            ),
            source,
        })?;
    let connect_elapsed = connect_started.elapsed();
    tracing::info!(
        endpoint = %endpoint.display_address(),
        request = %request_name,
        ?connect_elapsed,
        "established daemon IPC connection",
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|source| DaemonIpcError::Io {
            context: format!(
                "failed to write {request_name} request to daemon at {}",
                endpoint.display_address()
            ),
            source,
        })?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|source| DaemonIpcError::Io {
            context: format!(
                "failed to finish {request_name} request to daemon at {}",
                endpoint.display_address()
            ),
            source,
        })?;
    stream.flush().await.map_err(|source| DaemonIpcError::Io {
        context: format!(
            "failed to flush {request_name} request to daemon at {}",
            endpoint.display_address()
        ),
        source,
    })?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .await
        .map_err(|source| DaemonIpcError::Io {
            context: format!(
                "failed to read {request_name} response from daemon at {}",
                endpoint.display_address()
            ),
            source,
        })?;
    Ok(response)
}

fn parse_response_envelope(response: &str) -> Result<WireResponseEnvelope, DaemonIpcError> {
    let response = response.trim_end_matches(['\r', '\n']);
    let Some(payload) = response
        .strip_prefix(RESPONSE_ENVELOPE)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return Err(DaemonIpcError::UnexpectedResponse {
            expected: RESPONSE_ENVELOPE,
            response: response.to_owned(),
        });
    };
    serde_json::from_str::<WireResponseEnvelope>(payload).map_err(|error| {
        DaemonIpcError::UnexpectedResponse {
            expected: RESPONSE_ENVELOPE,
            response: format!("{response} ({error})"),
        }
    })
}

fn parse_input_glob_mtime_response(response: &str) -> Result<GlobMTime, DaemonIpcError> {
    let response = response.trim_end_matches(['\r', '\n']);
    let Some(payload) = response
        .strip_prefix(RESPONSE_INPUT_GLOB_MTIME)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return Err(DaemonIpcError::UnexpectedResponse {
            expected: RESPONSE_INPUT_GLOB_MTIME,
            response: response.to_owned(),
        });
    };
    match serde_json::from_str::<WireInputGlobMTimeResponse>(payload).map_err(|error| {
        DaemonIpcError::UnexpectedResponse {
            expected: RESPONSE_INPUT_GLOB_MTIME,
            response: format!("{response} ({error})"),
        }
    })? {
        WireInputGlobMTimeResponse::Ok { value } => Ok(value),
        WireInputGlobMTimeResponse::Error { message } => Err(DaemonIpcError::Remote { message }),
    }
}

fn parse_system_virtual_packages_response(
    response: &str,
) -> Result<Vec<GenericVirtualPackage>, DaemonIpcError> {
    let response = response.trim_end_matches(['\r', '\n']);
    let Some(payload) = response
        .strip_prefix(RESPONSE_SYSTEM_VIRTUAL_PACKAGES)
        .and_then(|rest| rest.strip_prefix(' '))
    else {
        return Err(DaemonIpcError::UnexpectedResponse {
            expected: RESPONSE_SYSTEM_VIRTUAL_PACKAGES,
            response: response.to_owned(),
        });
    };
    match serde_json::from_str::<WireSystemVirtualPackagesResponse>(payload).map_err(|error| {
        DaemonIpcError::UnexpectedResponse {
            expected: RESPONSE_SYSTEM_VIRTUAL_PACKAGES,
            response: format!("{response} ({error})"),
        }
    })? {
        WireSystemVirtualPackagesResponse::Ok { virtual_packages } => Ok(virtual_packages),
        WireSystemVirtualPackagesResponse::Error { message } => {
            Err(DaemonIpcError::Remote { message })
        }
    }
}

fn parse_pong(response: &str) -> Result<DaemonPong, DaemonIpcError> {
    let response = response.trim_end_matches(['\r', '\n']);
    let Some(rest) = response.strip_prefix("pong ") else {
        return Err(DaemonIpcError::UnexpectedResponse {
            expected: "pong",
            response: response.to_owned(),
        });
    };
    let Some((version, root)) = rest.split_once(' ') else {
        return Err(DaemonIpcError::UnexpectedResponse {
            expected: "pong",
            response: response.to_owned(),
        });
    };
    let protocol_version =
        version
            .parse::<u32>()
            .map_err(|_| DaemonIpcError::UnexpectedResponse {
                expected: "pong",
                response: response.to_owned(),
            })?;

    Ok(DaemonPong {
        protocol_version,
        workspace_root: PathBuf::from(root),
    })
}

fn workspace_id(root: &Path) -> String {
    let mut normalized = root.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        normalized = normalized.to_ascii_lowercase();
    }
    format!(
        "v{PROTOCOL_VERSION}-{:016x}",
        fnv1a64(normalized.as_bytes())
    )
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(unix)]
fn platform_address(workspace_id: &str) -> EndpointAddress {
    EndpointAddress::UnixSocket(
        std::env::temp_dir().join(format!("pixi-daemon-{workspace_id}.sock")),
    )
}

#[cfg(windows)]
fn platform_address(workspace_id: &str) -> EndpointAddress {
    EndpointAddress::NamedPipe(format!(r"\\.\pipe\pixi-daemon-{workspace_id}"))
}

#[cfg(unix)]
mod platform {
    use std::{future::Future, io, sync::Arc};

    use tokio::{net::UnixListener, net::UnixStream, sync::watch, task::JoinSet};

    use super::{DaemonEndpoint, DaemonIpcError, WorkspaceFsDaemon, handle_connection};

    pub(super) type Connection = UnixStream;

    pub(super) async fn serve_endpoint_until_shutdown(
        endpoint: DaemonEndpoint,
        daemon: Arc<WorkspaceFsDaemon>,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), DaemonIpcError> {
        let socket_path = endpoint.socket_path();
        if socket_path.exists() {
            std::fs::remove_file(socket_path).map_err(|source| DaemonIpcError::Io {
                context: format!(
                    "failed to remove stale daemon socket {}",
                    socket_path.display()
                ),
                source,
            })?;
        }
        let listener = UnixListener::bind(socket_path).map_err(|source| DaemonIpcError::Io {
            context: format!("failed to bind daemon socket {}", socket_path.display()),
            source,
        })?;
        let _cleanup = SocketCleanup(socket_path.to_path_buf());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let (stream, _) = accepted.map_err(|source| DaemonIpcError::Io {
                        context: format!("failed to accept daemon socket connection at {}", socket_path.display()),
                        source,
                    })?;
                    tracing::info!(
                        endpoint = %endpoint.display_address(),
                        workspace_root = %endpoint.workspace_root().display(),
                        "accepted daemon IPC client connection",
                    );
                    let workspace_root = endpoint.workspace_root().to_path_buf();
                    let daemon = daemon.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    tasks.spawn(async move {
                        if let Err(error) = handle_connection(stream, workspace_root, daemon, shutdown_tx).await {
                            tracing::debug!(%error, "failed to handle daemon IPC client");
                        }
                    });
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = joined {
                        tracing::debug!(%error, "daemon IPC client task panicked");
                    }
                }
            }
        }

        tasks.abort_all();
        while let Some(joined) = tasks.join_next().await {
            if let Err(error) = joined {
                tracing::debug!(%error, "daemon IPC client task finished after abort");
            }
        }
        Ok(())
    }

    pub(super) async fn connect_endpoint(endpoint: &DaemonEndpoint) -> io::Result<UnixStream> {
        UnixStream::connect(endpoint.socket_path()).await
    }

    struct SocketCleanup(std::path::PathBuf);

    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::{future::Future, io, sync::Arc};

    use tokio::{
        net::windows::named_pipe::{ClientOptions, NamedPipeClient, ServerOptions},
        sync::watch,
        task::JoinSet,
    };

    use super::{DaemonEndpoint, DaemonIpcError, WorkspaceFsDaemon, handle_connection};

    pub(super) type Connection = NamedPipeClient;

    pub(super) async fn serve_endpoint_until_shutdown(
        endpoint: DaemonEndpoint,
        daemon: Arc<WorkspaceFsDaemon>,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), DaemonIpcError> {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let mut first_instance = true;
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);

        loop {
            let mut options = ServerOptions::new();
            options.first_pipe_instance(first_instance);
            let server =
                options
                    .create(endpoint.pipe_name())
                    .map_err(|source| DaemonIpcError::Io {
                        context: format!(
                            "failed to create daemon named pipe {}",
                            endpoint.pipe_name()
                        ),
                        source,
                    })?;
            first_instance = false;

            tokio::select! {
                _ = &mut shutdown => break,
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                connected = server.connect() => {
                    connected.map_err(|source| DaemonIpcError::Io {
                        context: format!("failed to connect daemon named pipe {}", endpoint.pipe_name()),
                        source,
                    })?;
                    tracing::info!(
                        endpoint = %endpoint.display_address(),
                        workspace_root = %endpoint.workspace_root().display(),
                        "accepted daemon IPC client connection",
                    );
                    let workspace_root = endpoint.workspace_root().to_path_buf();
                    let daemon = daemon.clone();
                    let shutdown_tx = shutdown_tx.clone();
                    tasks.spawn(async move {
                        if let Err(error) = handle_connection(server, workspace_root, daemon, shutdown_tx).await {
                            tracing::debug!(%error, "failed to handle daemon IPC client");
                        }
                    });
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = joined {
                        tracing::debug!(%error, "daemon IPC client task panicked");
                    }
                }
            }
        }

        tasks.abort_all();
        while let Some(joined) = tasks.join_next().await {
            if let Err(error) = joined {
                tracing::debug!(%error, "daemon IPC client task finished after abort");
            }
        }
        Ok(())
    }

    pub(super) async fn connect_endpoint(endpoint: &DaemonEndpoint) -> io::Result<NamedPipeClient> {
        ClientOptions::new().open(endpoint.pipe_name())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, SystemTime},
    };

    use pixi_compute_fs::InputGlobMTimeProvider;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn endpoint_is_stable_for_same_workspace() {
        let temp = tempdir().unwrap();

        let first = DaemonEndpoint::for_workspace_root(temp.path()).unwrap();
        let second = DaemonEndpoint::for_workspace_root(temp.path()).unwrap();

        assert_eq!(first.workspace_id(), second.workspace_id());
        assert_eq!(first.display_address(), second.display_address());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn serves_ping_requests() {
        let temp = tempdir().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_task = shutdown.clone();
        let root = temp.path().to_path_buf();
        let server_root = root.clone();
        let server = tokio::spawn(async move {
            serve_workspace_until_shutdown(server_root, async move {
                while !shutdown_for_task.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        });

        let pong = wait_for_ping(&root).await;
        shutdown.store(true, Ordering::SeqCst);
        server.await.unwrap().unwrap();

        assert_eq!(pong.protocol_version, PROTOCOL_VERSION);
        assert_eq!(pong.workspace_root, root.canonicalize().unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_reports_running_and_not_running() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();

        let initial = status_workspace(&root).await.unwrap();
        assert!(!initial.is_running());

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_task = shutdown.clone();
        let server_root = root.clone();
        let server = tokio::spawn(async move {
            serve_workspace_until_shutdown(server_root, async move {
                while !shutdown_for_task.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        });

        wait_for_ping(&root).await;
        let running = status_workspace(&root).await.unwrap();
        shutdown.store(true, Ordering::SeqCst);
        server.await.unwrap().unwrap();

        assert!(running.is_running());
        assert_eq!(
            running.endpoint.workspace_root(),
            root.canonicalize().unwrap()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn input_glob_mtime_request_is_served_by_daemon() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let file = root.join("lib.rs");
        std::fs::write(&file, b"lib").unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_mtime(&file, mtime);

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_task = shutdown.clone();
        let server_root = root.clone();
        let server = tokio::spawn(async move {
            serve_workspace_until_shutdown(server_root, async move {
                while !shutdown_for_task.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
        });

        wait_for_ping(&root).await;
        let value = input_glob_mtime_workspace(&root, ".", InputGlobSpec::new(["*.rs"]))
            .await
            .unwrap();
        let provider = IpcInputGlobMTimeProvider::new(&root, "unused-executable");
        let provider_value = provider
            .input_glob_mtime(root.clone(), InputGlobSpec::new(["*.rs"]))
            .await
            .unwrap();
        shutdown.store(true, Ordering::SeqCst);
        server.await.unwrap().unwrap();

        let expected = GlobMTime::MatchesFound {
            modified_at: mtime,
            designated_file: root.canonicalize().unwrap().join("lib.rs"),
        };
        assert_eq!(value, expected);
        assert_eq!(provider_value, expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn daemon_client_multiplexes_concurrent_requests() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let file = root.join("lib.rs");
        std::fs::write(&file, b"lib").unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_123);
        set_mtime(&file, mtime);

        let server_root = root.clone();
        let server = tokio::spawn(async move {
            serve_workspace_until_shutdown(server_root, std::future::pending::<()>()).await
        });

        wait_for_ping(&root).await;
        let client = DaemonClient::new(&root);
        client.connect().await.unwrap();

        let (pong, glob, files, packages) = tokio::join!(
            client.ping(),
            client.input_glob_mtime(&root, InputGlobSpec::new(["*.rs"])),
            client.input_glob_files(&root, InputGlobSpec::new(["*.rs"])),
            client.system_virtual_packages(),
        );

        let _ = stop_workspace(&root).await;
        server.await.unwrap().unwrap();

        assert_eq!(pong.unwrap().protocol_version, PROTOCOL_VERSION);
        assert_eq!(
            glob.unwrap(),
            GlobMTime::MatchesFound {
                modified_at: mtime,
                designated_file: root.canonicalize().unwrap().join("lib.rs"),
            }
        );
        assert_eq!(
            files.unwrap(),
            vec![root.canonicalize().unwrap().join("lib.rs")]
        );
        let expected = VirtualPackages::detect(&VirtualPackageOverrides::default())
            .unwrap()
            .into_generic_virtual_packages()
            .collect::<Vec<_>>();
        assert_eq!(packages.unwrap(), expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn system_virtual_packages_request_is_served_by_daemon() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let server_root = root.clone();
        let server = tokio::spawn(async move {
            serve_workspace_until_shutdown(server_root, std::future::pending::<()>()).await
        });

        wait_for_ping(&root).await;
        let packages = system_virtual_packages_workspace(&root).await.unwrap();
        let _ = stop_workspace(&root).await;
        server.await.unwrap().unwrap();

        let expected = VirtualPackages::detect(&VirtualPackageOverrides::default())
            .unwrap()
            .into_generic_virtual_packages()
            .collect::<Vec<_>>();
        assert_eq!(packages, expected);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_request_stops_server() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let server_root = root.clone();
        let server = tokio::spawn(async move {
            serve_workspace_until_shutdown(server_root, std::future::pending::<()>()).await
        });

        wait_for_ping(&root).await;
        let stopped = stop_workspace(&root).await.unwrap();
        server.await.unwrap().unwrap();
        let status = status_workspace(&root).await.unwrap();

        assert!(stopped.stopped);
        assert!(!status.is_running());
    }

    fn set_mtime(path: impl AsRef<Path>, time: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    async fn wait_for_ping(root: &Path) -> DaemonPong {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(pong) = ping_workspace(root).await {
                return pong;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not accept ping in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
