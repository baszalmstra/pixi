//! Experimental daemon services for Pixi.
//!
//! The first slice is intentionally narrow: [`WorkspaceFsDaemon`] owns a
//! per-workspace compute engine, indexed VFS, and `notify` watcher. Watcher
//! events invalidate filesystem compute keys immediately, while glob mtime
//! values are recomputed lazily when a client asks for them.

use std::{
    collections::BTreeSet,
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use notify::{
    Event, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode, EventKind},
};
use pixi_compute_engine::{ComputeEngine, ComputeEngineStats, ComputeError, GraphVersion};
use pixi_compute_fs::{ComputeCtxFsExt, ComputeEngineFsExt, FsError, GlobMTime, InputGlobSpec};
use pixi_vfs::{IndexedVfs, VfsStats};

const DEFAULT_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(50);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_BATCH_DURATION: Duration = Duration::from_millis(500);
const MAX_BATCH_EVENTS: usize = 4096;

/// Options for starting a per-workspace filesystem daemon.
#[derive(Clone, Debug)]
pub struct WorkspaceFsDaemonOptions {
    /// Canonical workspace root to watch recursively.
    pub root: PathBuf,
    /// Quiet period used to batch raw watcher events before invalidating graph
    /// keys.
    pub debounce_interval: Duration,
}

impl WorkspaceFsDaemonOptions {
    /// Create options for `root` using the default debounce interval.
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            debounce_interval: DEFAULT_DEBOUNCE_INTERVAL,
        }
    }

    /// Set the watcher-event debounce interval.
    pub fn with_debounce_interval(mut self, debounce_interval: Duration) -> Self {
        self.debounce_interval = debounce_interval;
        self
    }
}

/// A per-workspace filesystem daemon slice.
///
/// This is an in-process service boundary for now. It owns the filesystem
/// compute state and watcher thread that a later process/IPC daemon can wrap.
pub struct WorkspaceFsDaemon {
    root: PathBuf,
    engine: ComputeEngine,
    vfs: Arc<IndexedVfs>,
    counters: Arc<Counters>,
    _watcher: WatcherThread,
}

impl WorkspaceFsDaemon {
    /// Start watching `root` recursively using the default options.
    pub fn start(root: impl AsRef<Path>) -> Result<Self, DaemonError> {
        Self::start_with_options(WorkspaceFsDaemonOptions::new(root))
    }

    /// Start a workspace filesystem daemon with explicit options.
    pub fn start_with_options(options: WorkspaceFsDaemonOptions) -> Result<Self, DaemonError> {
        let root = options
            .root
            .canonicalize()
            .map_err(|source| DaemonError::CanonicalizeRoot {
                path: options.root.clone(),
                source,
            })?;
        let vfs = Arc::new(IndexedVfs::default());
        let engine = ComputeEngine::builder().with_data(vfs.clone()).build();
        let counters = Arc::new(Counters::default());
        let watcher = WatcherThread::start(
            root.clone(),
            engine.clone(),
            vfs.clone(),
            counters.clone(),
            options.debounce_interval,
        )?;

        Ok(Self {
            root,
            engine,
            vfs,
            counters,
            _watcher: watcher,
        })
    }

    /// Return the canonical workspace root watched by this daemon.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the daemon-owned compute engine.
    pub fn engine(&self) -> &ComputeEngine {
        &self.engine
    }

    /// Return the daemon-owned indexed VFS.
    pub fn vfs(&self) -> &Arc<IndexedVfs> {
        &self.vfs
    }

    /// Compute the latest mtime for an ordered input-glob spec under `root` on demand.
    ///
    /// Watcher events never recompute this value eagerly. They only invalidate
    /// graph/VFS state so this request observes the newest graph version.
    pub async fn input_glob_mtime(
        &self,
        root: impl AsRef<Path>,
        spec: InputGlobSpec,
    ) -> Result<GlobMTimeResponse, DaemonError> {
        let root = self.resolve_request_root(root.as_ref())?;
        validate_spec_scope(&spec)?;
        let query_root = root.clone();
        let query_spec = spec.clone();
        let started = Instant::now();
        let value = self
            .engine
            .with_ctx(async |ctx| ctx.input_glob_mtime(&query_root, query_spec).await)
            .await??;
        let elapsed = started.elapsed();

        Ok(GlobMTimeResponse {
            root,
            spec,
            value,
            compute_elapsed: elapsed,
            engine_stats: self.engine.stats(),
            vfs_stats: self.vfs.stats(),
        })
    }

    /// Coarsely reset all daemon-owned filesystem state.
    ///
    /// This is used when watcher events indicate an uncertain state. Because
    /// this daemon-owned engine currently stores only filesystem graph data, a
    /// full graph reset is the simplest correct fallback.
    pub fn reset_filesystem_state(&self) -> GraphVersion {
        reset_workspace_state(&self.engine, &self.vfs, &self.counters)
    }

    /// Return current daemon counters and underlying engine/VFS stats.
    pub fn stats(&self) -> WorkspaceFsDaemonStats {
        WorkspaceFsDaemonStats {
            event_batches: self.counters.event_batches.load(Ordering::SeqCst),
            events: self.counters.events.load(Ordering::SeqCst),
            invalidation_batches: self.counters.invalidation_batches.load(Ordering::SeqCst),
            invalidated_paths: self.counters.invalidated_paths.load(Ordering::SeqCst),
            rescan_events: self.counters.rescan_events.load(Ordering::SeqCst),
            resets: self.counters.resets.load(Ordering::SeqCst),
            watcher_errors: self.counters.watcher_errors.load(Ordering::SeqCst),
            last_error: self.counters.last_error(),
            engine: self.engine.stats(),
            vfs: self.vfs.stats(),
        }
    }

    fn resolve_request_root(&self, root: &Path) -> Result<PathBuf, DaemonError> {
        if root.as_os_str().is_empty() || root == Path::new(".") {
            return Ok(self.root.clone());
        }

        if !root.is_absolute() && has_escape_component(root) {
            return Err(DaemonError::OutsideWorkspace {
                workspace_root: self.root.clone(),
                requested_root: root.to_path_buf(),
            });
        }

        let candidate = if root.is_absolute() {
            root.to_path_buf()
        } else {
            self.root.join(root)
        };
        let resolved = if candidate.exists() {
            candidate
                .canonicalize()
                .map_err(|source| DaemonError::CanonicalizeRequestRoot {
                    path: candidate.clone(),
                    source,
                })?
        } else {
            candidate
        };

        if resolved.starts_with(&self.root) {
            Ok(resolved)
        } else {
            Err(DaemonError::OutsideWorkspace {
                workspace_root: self.root.clone(),
                requested_root: resolved,
            })
        }
    }
}

fn has_escape_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir
        )
    })
}

fn validate_spec_scope(spec: &InputGlobSpec) -> Result<(), DaemonError> {
    for pattern in &spec.patterns {
        validate_pattern_scope(pattern.strip_prefix('!').unwrap_or(pattern))?;
    }
    Ok(())
}

fn validate_pattern_scope(pattern: &str) -> Result<(), DaemonError> {
    let normalized = pattern.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized
            .split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .any(|component| component == "..")
        || has_escape_component(Path::new(pattern))
    {
        return Err(DaemonError::GlobPatternEscapesWorkspace {
            pattern: pattern.to_owned(),
        });
    }
    Ok(())
}

/// Result of one daemon glob mtime request.
#[derive(Clone, Debug)]
pub struct GlobMTimeResponse {
    pub root: PathBuf,
    pub spec: InputGlobSpec,
    pub value: GlobMTime,
    pub compute_elapsed: Duration,
    pub engine_stats: ComputeEngineStats,
    pub vfs_stats: VfsStats,
}

/// Point-in-time daemon diagnostics.
#[derive(Clone, Debug)]
pub struct WorkspaceFsDaemonStats {
    pub event_batches: usize,
    pub events: usize,
    pub invalidation_batches: usize,
    pub invalidated_paths: usize,
    pub rescan_events: usize,
    pub resets: usize,
    pub watcher_errors: usize,
    pub last_error: Option<String>,
    pub engine: ComputeEngineStats,
    pub vfs: VfsStats,
}

/// Errors returned by the experimental daemon service.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("failed to canonicalize workspace root {path}: {source}")]
    CanonicalizeRoot { path: PathBuf, source: io::Error },
    #[error("failed to canonicalize request root {path}: {source}")]
    CanonicalizeRequestRoot { path: PathBuf, source: io::Error },
    #[error("glob pattern {pattern:?} escapes daemon workspace")]
    GlobPatternEscapesWorkspace { pattern: String },
    #[error("failed to start watcher thread: {0}")]
    ThreadStart(#[source] io::Error),
    #[error("notify watcher failed to start: {message}")]
    WatcherStart { message: String },
    #[error("notify watcher start handshake failed")]
    WatcherHandshake,
    #[error("requested root {requested_root} is outside daemon workspace {workspace_root}")]
    OutsideWorkspace {
        workspace_root: PathBuf,
        requested_root: PathBuf,
    },
    #[error(transparent)]
    Compute(#[from] ComputeError),
    #[error(transparent)]
    Filesystem(#[from] FsError),
}

struct WatcherThread {
    stop_tx: mpsc::Sender<()>,
    join: Option<thread::JoinHandle<()>>,
}

impl WatcherThread {
    fn start(
        root: PathBuf,
        engine: ComputeEngine,
        vfs: Arc<IndexedVfs>,
        counters: Arc<Counters>,
        debounce_interval: Duration,
    ) -> Result<Self, DaemonError> {
        let (stop_tx, stop_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let join = thread::Builder::new()
            .name("pixi-daemon-fs-watch".to_owned())
            .spawn(move || {
                run_watcher_thread(
                    root,
                    engine,
                    vfs,
                    counters,
                    debounce_interval,
                    stop_rx,
                    ready_tx,
                )
            })
            .map_err(DaemonError::ThreadStart)?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_tx,
                join: Some(join),
            }),
            Ok(Err(message)) => {
                let _ = join.join();
                Err(DaemonError::WatcherStart { message })
            }
            Err(_) => {
                let _ = join.join();
                Err(DaemonError::WatcherHandshake)
            }
        }
    }
}

impl Drop for WatcherThread {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_watcher_thread(
    root: PathBuf,
    engine: ComputeEngine,
    vfs: Arc<IndexedVfs>,
    counters: Arc<Counters>,
    debounce_interval: Duration,
    stop_rx: mpsc::Receiver<()>,
    ready_tx: mpsc::Sender<Result<(), String>>,
) {
    let (event_tx, event_rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = match notify::recommended_watcher(event_tx) {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = ready_tx.send(Err(error.to_string()));
            return;
        }
    };
    if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
        let _ = ready_tx.send(Err(error.to_string()));
        return;
    }
    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    let _watcher = watcher;
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let first = match event_rx.recv_timeout(STOP_POLL_INTERVAL) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                counters.record_error("notify watcher channel disconnected".to_owned());
                break;
            }
        };

        let batch = collect_batch(first, &event_rx, debounce_interval);
        process_batch(batch, &root, &engine, &vfs, &counters);
    }
}

fn collect_batch(
    first: notify::Result<Event>,
    event_rx: &mpsc::Receiver<notify::Result<Event>>,
    debounce_interval: Duration,
) -> Vec<notify::Result<Event>> {
    let mut batch = vec![first];
    let started = Instant::now();
    let max_deadline = started + MAX_BATCH_DURATION;
    let mut quiet_deadline = started + debounce_interval;

    loop {
        if batch.len() >= MAX_BATCH_EVENTS {
            break;
        }

        let now = Instant::now();
        let deadline = quiet_deadline.min(max_deadline);
        if now >= deadline {
            break;
        }

        match event_rx.recv_timeout(deadline - now) {
            Ok(event) => {
                batch.push(event);
                quiet_deadline = Instant::now() + debounce_interval;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    batch
}

fn process_batch(
    batch: Vec<notify::Result<Event>>,
    root: &Path,
    engine: &ComputeEngine,
    vfs: &IndexedVfs,
    counters: &Counters,
) {
    counters.event_batches.fetch_add(1, Ordering::SeqCst);
    let decision = classify_batch(batch, root, counters);

    if decision.reset_required {
        reset_workspace_state(engine, vfs, counters);
        return;
    }

    if decision.paths.is_empty() {
        return;
    }

    counters
        .invalidated_paths
        .fetch_add(decision.paths.len(), Ordering::SeqCst);
    match engine.invalidate_paths(decision.paths.iter()) {
        Ok(update) => {
            if update.changed {
                counters.invalidation_batches.fetch_add(1, Ordering::SeqCst);
            }
        }
        Err(error) => {
            counters.record_error(format!("filesystem invalidation failed: {error}"));
            reset_workspace_state(engine, vfs, counters);
        }
    }
}

fn classify_batch(
    batch: Vec<notify::Result<Event>>,
    _root: &Path,
    counters: &Counters,
) -> BatchDecision {
    let mut paths = BTreeSet::new();
    let mut reset_required = false;
    let mut saw_pathless_mutation = false;

    for event in batch {
        match event {
            Ok(event) => {
                counters.events.fetch_add(1, Ordering::SeqCst);
                if event.need_rescan() {
                    counters.rescan_events.fetch_add(1, Ordering::SeqCst);
                    reset_required = true;
                    continue;
                }
                if is_ignored_access_event(event.kind) {
                    continue;
                }
                if event.paths.is_empty() {
                    saw_pathless_mutation = true;
                    reset_required = true;
                } else {
                    paths.extend(event.paths);
                }
            }
            Err(error) => {
                counters.watcher_errors.fetch_add(1, Ordering::SeqCst);
                counters.record_error(format!("notify watcher error: {error}"));
                reset_required = true;
            }
        }
    }

    if saw_pathless_mutation {
        counters.rescan_events.fetch_add(1, Ordering::SeqCst);
    }

    BatchDecision {
        reset_required,
        paths: paths.into_iter().collect(),
    }
}

fn is_ignored_access_event(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(
            AccessKind::Read
                | AccessKind::Open(AccessMode::Read | AccessMode::Execute)
                | AccessKind::Close(AccessMode::Read)
        )
    )
}

fn reset_workspace_state(
    engine: &ComputeEngine,
    vfs: &IndexedVfs,
    counters: &Counters,
) -> GraphVersion {
    vfs.clear_index();
    counters.resets.fetch_add(1, Ordering::SeqCst);
    engine.unstable_drop_everything()
}

#[derive(Debug)]
struct BatchDecision {
    reset_required: bool,
    paths: Vec<PathBuf>,
}

#[derive(Default)]
struct Counters {
    event_batches: AtomicUsize,
    events: AtomicUsize,
    invalidation_batches: AtomicUsize,
    invalidated_paths: AtomicUsize,
    rescan_events: AtomicUsize,
    resets: AtomicUsize,
    watcher_errors: AtomicUsize,
    last_error: Mutex<Option<String>>,
}

impl Counters {
    fn record_error(&self, error: String) {
        if let Ok(mut last_error) = self.last_error.lock() {
            *last_error = Some(error);
        }
    }

    fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .ok()
            .and_then(|last_error| last_error.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use notify::event::{AccessKind, AccessMode, EventKind, Flag};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn classify_batch_ignores_access_and_sorts_mutating_paths() {
        let counters = Counters::default();
        let batch = vec![
            Ok(Event::new(EventKind::Access(AccessKind::Read)).add_path(PathBuf::from("read.rs"))),
            Ok(
                Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                    .add_path(PathBuf::from("z.rs"))
                    .add_path(PathBuf::from("a.rs")),
            ),
        ];

        let decision = classify_batch(batch, Path::new("root"), &counters);

        assert!(!decision.reset_required);
        assert_eq!(
            decision.paths,
            vec![PathBuf::from("a.rs"), PathBuf::from("z.rs")]
        );
        assert_eq!(counters.events.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn classify_batch_treats_write_close_as_mutating() {
        let counters = Counters::default();
        let event = Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write)))
            .add_path(PathBuf::from("written.rs"));

        let decision = classify_batch(vec![Ok(event)], Path::new("root"), &counters);

        assert!(!decision.reset_required);
        assert_eq!(decision.paths, vec![PathBuf::from("written.rs")]);
    }

    #[test]
    fn classify_batch_requires_reset_for_pathless_mutation() {
        let counters = Counters::default();
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any));

        let decision = classify_batch(vec![Ok(event)], Path::new("root"), &counters);

        assert!(decision.reset_required);
        assert!(decision.paths.is_empty());
        assert_eq!(counters.rescan_events.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn classify_batch_requires_reset_for_rescan_event() {
        let counters = Counters::default();
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);

        let decision = classify_batch(vec![Ok(event)], Path::new("root"), &counters);

        assert!(decision.reset_required);
        assert!(decision.paths.is_empty());
        assert_eq!(counters.rescan_events.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn daemon_rejects_roots_and_patterns_that_escape_workspace() {
        let temp = tempdir().unwrap();
        let daemon = WorkspaceFsDaemon::start_with_options(
            WorkspaceFsDaemonOptions::new(temp.path())
                .with_debounce_interval(Duration::from_millis(10)),
        )
        .unwrap();

        assert!(
            daemon
                .input_glob_mtime(daemon.root(), InputGlobSpec::new(["*.rs"]))
                .await
                .is_ok()
        );
        assert!(matches!(
            daemon
                .input_glob_mtime("..", InputGlobSpec::new(["*.rs"]))
                .await,
            Err(DaemonError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            daemon
                .input_glob_mtime(".", InputGlobSpec::new(["../*.rs"]))
                .await,
            Err(DaemonError::GlobPatternEscapesWorkspace { .. })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn glob_mtime_requests_are_warm_without_changes() {
        let temp = tempdir().unwrap();
        let file = temp.path().join("lib.rs");
        std::fs::write(&file, b"lib").unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_mtime(&file, mtime);

        let daemon = WorkspaceFsDaemon::start_with_options(
            WorkspaceFsDaemonOptions::new(temp.path())
                .with_debounce_interval(Duration::from_millis(10)),
        )
        .unwrap();

        let first = daemon
            .input_glob_mtime(".", InputGlobSpec::new(["*.rs"]))
            .await
            .unwrap();
        assert_eq!(
            first.value,
            GlobMTime::MatchesFound {
                modified_at: mtime,
                designated_file: daemon.root().join("lib.rs"),
            }
        );
        let second = daemon
            .input_glob_mtime(".", InputGlobSpec::new(["*.rs"]))
            .await
            .unwrap();
        assert_eq!(second.value, first.value);
        assert_eq!(
            second.vfs_stats.disk_dir_reads,
            first.vfs_stats.disk_dir_reads
        );
        assert_eq!(
            second.vfs_stats.disk_metadata_reads,
            first.vfs_stats.disk_metadata_reads
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notify_create_invalidates_and_next_request_recomputes() {
        let temp = tempdir().unwrap();
        let old = temp.path().join("old.rs");
        let new = temp.path().join("new.rs");
        std::fs::write(&old, b"old").unwrap();
        let old_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let new_time = old_time + Duration::from_secs(10);
        set_mtime(&old, old_time);

        let daemon = WorkspaceFsDaemon::start_with_options(
            WorkspaceFsDaemonOptions::new(temp.path())
                .with_debounce_interval(Duration::from_millis(10)),
        )
        .unwrap();
        assert_eq!(
            daemon
                .input_glob_mtime(".", InputGlobSpec::new(["*.rs"]))
                .await
                .unwrap()
                .value,
            GlobMTime::MatchesFound {
                modified_at: old_time,
                designated_file: daemon.root().join("old.rs"),
            }
        );

        std::fs::write(&new, b"new").unwrap();
        set_mtime(&new, new_time);

        let latest = wait_for_latest(&daemon, "*.rs", new_time, daemon.root().join("new.rs")).await;
        assert!(
            latest,
            "daemon did not observe notify event and recompute latest file; stats={:?}",
            daemon.stats()
        );
        assert!(daemon.stats().events > 0);
    }

    fn set_mtime(path: impl AsRef<Path>, time: SystemTime) {
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
    }

    async fn wait_for_latest(
        daemon: &WorkspaceFsDaemon,
        pattern: &str,
        modified_at: SystemTime,
        designated_file: PathBuf,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if daemon.stats().invalidation_batches > 0 {
                let value = daemon
                    .input_glob_mtime(".", InputGlobSpec::new([pattern]))
                    .await
                    .unwrap()
                    .value;
                if value
                    == (GlobMTime::MatchesFound {
                        modified_at,
                        designated_file: designated_file.clone(),
                    })
                {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}
