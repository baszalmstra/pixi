# Pixi workspace daemon IPC design

## Goal

Make Pixi filesystem freshness/build-input functionality be served through a daemon boundary only.

Normal Pixi operations should not bypass the daemon-owned filesystem state by directly using `IndexedVfs` or direct glob mtime calls. The daemon owns the indexed VFS, compute graph, filesystem watcher, and invalidation. Pixi commands call a daemon service interface.

## Desired UX

Initial explicit mode:

```bash
pixi --daemon
```

This starts a foreground per-workspace daemon using the current workspace root. It runs until idle for about 30 minutes.

Client use during rollout:

```bash
PIXI_USE_DAEMON=1 pixi install
```

Later, normal `pixi install`/`pixi build` can auto-connect or auto-start by default once IPC is stable.

## Core decision

Use one executable with two process modes:

1. **Client mode**: normal Pixi subcommands.
2. **Daemon mode**: `pixi --daemon` runs the workspace daemon.

Expose a daemon service interface with two implementations:

- `InProcessWorkspaceDaemonClient`
  - used for tests and early fallback
  - calls daemon/service functions directly in-process
- `IpcWorkspaceDaemonClient`
  - used by normal Pixi clients when daemon mode is enabled
  - connects to a long-lived daemon process over local IPC

All production filesystem freshness queries should depend on this service abstraction, not on direct `IndexedVfs` access.

## Why not stdio pipes?

Pixi already uses stdio JSON-RPC for build backends, but that fits parent-owned child processes. A 30-minute daemon must outlive one client process and accept later Pixi invocations. Stdio pipes are not appropriate for reconnectable multi-client daemon service.

## IPC recommendation

Use local IPC with per-client connections:

- Windows: named pipe
- Unix: Unix domain socket

Protocol can be newline-delimited JSON request/response initially. The request model should be explicit and versioned. We can use lightweight serde structs first and only move to jsonrpsee/server machinery if we need full JSON-RPC behavior.

### Windows named pipes and concurrency

Multiple Pixi processes can connect to one daemon using the same pipe name because Windows named pipes support multiple pipe instances.

All clients connect to the same name, e.g.:

```text
\\.\pipe\pixi-daemon-<workspace-hash>-<user>
```

The daemon creates/accepts one pipe instance, then immediately creates another instance for future clients while handling the connected instance in a task:

```rust
loop {
    let server = ServerOptions::new()
        .first_pipe_instance(false)
        .create(pipe_name)?;

    server.connect().await?;

    tokio::spawn(async move {
        handle_client(server).await;
    });
}
```

Notes:

- A single daemon process owns the workspace.
- Each client gets its own pipe instance.
- Requests from multiple clients can run concurrently.
- Mutations/invalidation remain serialized by the daemon's compute/VFS internals.
- Clients should retry briefly if they race while the daemon creates the next pipe instance.

### Unix socket concurrency

Use one Unix domain socket listener per workspace. The accept loop spawns a task per connection.

Example path:

```text
<pixi-cache>/daemon/<workspace-hash>/daemon.sock
```

## Daemon identity and locking

Daemon identity should be based on:

- canonical workspace root
- current user identity if needed
- protocol version

Recommended workspace daemon directory:

```text
<pixi-cache>/daemon/<workspace-hash>/
```

Files:

```text
lock          # single owner lock
metadata.json # pid, protocol version, executable path, started_at, last_activity maybe
socket        # unix socket path, on Unix
```

Windows named-pipe name can be stored in `metadata.json` too.

A lock file prevents two daemon processes from owning the same workspace daemon simultaneously.

Startup behavior:

1. Client computes workspace hash.
2. Client checks daemon metadata/lock/socket or pipe.
3. If existing daemon responds to `ping`, use it.
4. If stale metadata/lock, clean it up if safe.
5. If auto-start enabled, spawn `pixi --daemon --workspace <root>` and wait for `ping`.
6. Otherwise return a clear error telling user to start `pixi --daemon`.

## Idle shutdown

Daemon should shut down after no client activity for 30 minutes.

Track:

- `last_client_activity: Instant`
- `active_requests: AtomicUsize` or equivalent

Every accepted request updates `last_client_activity`.

Idle loop:

```text
if active_requests == 0 && now - last_client_activity > idle_timeout:
    gracefully stop watcher
    exit
```

Important: filesystem watcher events should **not** reset the idle timer. Otherwise noisy workspaces can keep the daemon alive forever.

Configurable default:

```text
30 minutes
```

Potential flags/env:

```bash
pixi --daemon --idle-timeout 30m
PIXI_DAEMON_IDLE_TIMEOUT=30m
```

## Service boundary

Introduce a daemon filesystem service trait, for example:

```rust
#[async_trait]
pub trait WorkspaceFsService: Send + Sync {
    async fn metadata(&self, path: PathBuf) -> Result<FsMetadata, FsError>;
    async fn read_file(&self, path: PathBuf) -> Result<Arc<[u8]>, FsError>;
    async fn read_dir(&self, path: PathBuf) -> Result<Arc<[DirectoryEntry]>, FsError>;
    async fn input_glob_mtime(
        &self,
        root: PathBuf,
        spec: InputGlobSpec,
    ) -> Result<GlobMTime, FsError>;
}
```

The exact trait location can be `pixi_compute_fs` or `pixi_daemon` depending on dependency direction. Prefer avoiding a cycle:

- `pixi_compute_fs` defines request/response traits/types used by compute keys.
- `pixi_daemon` implements the trait using `WorkspaceFsDaemon`.
- IPC client implementation can live in `pixi_daemon` or a daemon-client module/crate.

Current direct registration:

```rust
ComputeEngine::builder()
    .with_data(Arc::new(IndexedVfs::default()))
```

Should become service registration:

```rust
ComputeEngine::builder()
    .with_data(Arc::<dyn WorkspaceFsService>::from(...))
```

Then `pixi_compute_fs` keys call the service abstraction rather than `IndexedVfs` directly.

## Daemon internals

The daemon process owns:

- `ComputeEngine`
- `Arc<IndexedVfs>`
- notify watcher
- invalidation bridge
- request handler
- idle shutdown loop

The daemon should continue to recompute lazily/request-driven:

- watcher events invalidate VFS/compute graph
- client requests trigger recomputation

Pathless/uncertain watcher events should retain current behavior:

- `IndexedVfs::clear_index()`
- compute graph reset

## Request protocol sketch

Versioned envelopes:

```rust
#[derive(Serialize, Deserialize)]
struct RequestEnvelope {
    id: u64,
    protocol_version: u32,
    workspace_root: PathBuf,
    request: DaemonRequest,
}

#[derive(Serialize, Deserialize)]
enum DaemonRequest {
    Ping,
    Stats,
    Metadata { path: PathBuf },
    ReadFile { path: PathBuf },
    ReadDir { path: PathBuf },
    InputGlobMTime { root: PathBuf, spec: InputGlobSpec },
    ResetFilesystemState,
    Shutdown,
}

#[derive(Serialize, Deserialize)]
struct ResponseEnvelope {
    id: u64,
    response: Result<DaemonResponse, DaemonErrorWire>,
}
```

`DaemonErrorWire` should be stable/serializable and converted back to local error types on client side.

Binary file responses can initially use JSON with byte arrays only if needed, but `read_file` may become expensive. If read_file crosses IPC frequently, consider length-prefixed binary framing. For first slice, glob mtime/stats/ping are enough.

## Integration with dispatcher freshness

Current dispatcher freshness calls `ctx.input_glob_mtime(...)` through `pixi_compute_fs`.

After service abstraction, those compute keys remain the call site, but instead of requiring `IndexedVfs` in engine data, they require `Arc<dyn WorkspaceFsService>`.

`CommandDispatcherBuilder::finish()` should decide the service implementation:

- daemon disabled/fallback/tests: in-process service
- daemon enabled: IPC service, optionally auto-starting daemon

The goal is that production paths never call `pixi_glob` or `IndexedVfs` directly for freshness. Exact input discovery via `collect_input_files` remains separate and should eventually either:

- be served through the daemon too, or
- stay as exact fallback only if explicitly accepted

If goal is strict daemon-only, exact input discovery should also be added to the service as a request, not direct local walking.

## CLI shape

Add global options:

```rust
pub struct Args {
    #[clap(long, global = true)]
    daemon: bool,

    #[clap(long, global = true)]
    no_daemon: bool,
}
```

Possible meanings:

- `pixi --daemon`: run daemon server for current workspace and do not run a normal command.
- `pixi --no-daemon <cmd>`: force in-process fallback.
- `PIXI_USE_DAEMON=1`: client mode uses/starts daemon.

Because `Args` currently has `arg_required_else_help = true`, `--daemon` mode needs to be accepted as a top-level action even without a subcommand.

Pseudo-flow in `pixi_cli::execute()`:

```rust
let args = Args::parse();
setup_logging(...)?;

if args.daemon {
    let workspace = Workspace::from_current_dir(...)?;
    return pixi_daemon::serve_workspace(workspace.root(), options).await;
}

let (Some(command), global_options) = ... else { exit(2) };
execute_command(command, &global_options).await
```

## Implementation phases

### Phase 1: Service trait and in-process adapter

- Define `WorkspaceFsService` trait.
- Implement `InProcessWorkspaceFsService` using existing `WorkspaceFsDaemon` or direct daemon internals.
- Change `pixi_compute_fs` to depend on service trait instead of direct `IndexedVfs`.
- Register in-process service in `CommandDispatcherBuilder::finish()`.
- Keep tests passing.

This enforces daemon/service boundary without IPC complexity.

### Phase 2: IPC protocol and daemon CLI

- Add `pixi --daemon` foreground server.
- Add request/response protocol.
- Add Unix socket/named-pipe listener.
- Add `Ping`, `Stats`, `InputGlobMTime` requests first.
- Add idle shutdown.
- Add tests for idle timeout using short duration.

### Phase 3: IPC client and opt-in use

- Add `IpcWorkspaceFsService`.
- Add daemon discovery and client retry.
- Add `PIXI_USE_DAEMON=1` / config toggle.
- In dispatcher builder, use IPC client when enabled.
- If enabled and daemon unavailable, either:
  - auto-start `pixi --daemon`, or
  - return actionable error for first milestone.

### Phase 4: Strict daemon-only functionality

- Move exact input discovery (`collect_input_files`) behind daemon service if strict daemon-only is required.
- Consider read_file/read_dir/metadata over IPC if needed by compute_fs consumers.
- Add robust serialization for file contents or avoid cross-process file contents initially.

### Phase 5: Auto-start and production hardening

- Add daemon single-owner lock.
- Add stale daemon cleanup.
- Add protocol-version compatibility checks.
- Add logs and diagnostics.
- Add `pixi daemon status/stop` subcommands if useful.

## Open questions

1. Should normal commands auto-start the daemon by default, or only with `PIXI_USE_DAEMON=1` initially?
2. Should `pixi --daemon` be a global mode or a hidden `pixi daemon start` subcommand?
3. Is strict daemon-only required for exact input discovery immediately, or only for fast freshness checks first?
4. Should IPC use custom line-delimited JSON or jsonrpsee local transport?
5. Where should daemon metadata live on Windows if cache dir is on a network filesystem?

## Recommendation

Start with Phase 1 + Phase 2 minimal:

- Introduce `WorkspaceFsService` now.
- Route compute filesystem freshness through that service only.
- Implement `pixi --daemon` foreground server with local IPC and 30-minute idle timeout.
- Keep auto-start as a follow-up; begin with explicit `pixi --daemon` and opt-in client connection.

This minimizes risk while making the architectural boundary explicit and testable.
