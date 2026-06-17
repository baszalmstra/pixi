//! Technology demo: monitor a directory tree and recompute one glob mtime key.
//!
//! Run with:
//!
//! ```text
//! cargo run -p pixi_compute_fs --example glob_watch -- <root> '<glob-pattern>' [debounce-ms]
//! ```
//!
//! This uses `notify` as the event source: watcher events are batched for a
//! short debounce window, their paths are sent through `ComputeEngineFsExt`, and
//! the latest-mtime query is recomputed through the compute graph and indexed
//! VFS.

use std::{
    collections::BTreeSet,
    env,
    error::Error,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use notify::{Event, RecursiveMode, Watcher};
use pixi_compute_engine::{ComputeEngine, UpdateResult};
use pixi_compute_fs::{ComputeCtxFsExt, ComputeEngineFsExt, GlobMTime, InputGlobSpec};
use pixi_vfs::IndexedVfs;

const DEFAULT_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(50);

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let args = Args::parse(env::args_os())?;
    let root = args.root.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to canonicalize {}: {error}", args.root.display()),
        )
    })?;

    println!(
        "watching {} for {:?} with notify debounce {:?}",
        root.display(),
        args.pattern,
        args.debounce_interval
    );

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    let initial_started = Instant::now();
    let initial = compute_glob_mtime(&engine, &root, &args.pattern).await?;
    print_result("initial", initial_started.elapsed(), &initial);

    // Keep `watcher` alive for the whole loop.
    let _watcher = watcher;

    loop {
        let batch = receive_event_batch(&rx, &root, args.debounce_interval)?;
        if batch.paths.is_empty() {
            continue;
        }

        let batch_started = Instant::now();
        let invalidate_started = Instant::now();
        let update = engine.invalidate_paths(batch.paths.iter())?;
        let invalidate_elapsed = invalidate_started.elapsed();

        let recompute_started = Instant::now();
        let result = compute_glob_mtime(&engine, &root, &args.pattern).await?;
        let recompute_elapsed = recompute_started.elapsed();
        let total_elapsed = batch_started.elapsed();

        print_change_batch(
            &batch,
            &update,
            invalidate_elapsed,
            recompute_elapsed,
            total_elapsed,
        );
        print_result("latest", recompute_elapsed, &result);
    }
}

async fn compute_glob_mtime(
    engine: &ComputeEngine,
    root: &Path,
    pattern: &str,
) -> Result<GlobMTime, Box<dyn Error>> {
    let value = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(root, InputGlobSpec::new([pattern]))
                .await
        })
        .await??;
    Ok(value)
}

fn receive_event_batch(
    rx: &mpsc::Receiver<notify::Result<Event>>,
    root: &Path,
    debounce_interval: Duration,
) -> Result<EventBatch, Box<dyn Error>> {
    let mut events = Vec::new();
    push_event(rx.recv()?, &mut events)?;

    let mut deadline = Instant::now() + debounce_interval;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        match rx.recv_timeout(deadline - now) {
            Ok(event) => {
                push_event(event, &mut events)?;
                deadline = Instant::now() + debounce_interval;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("notify watcher channel disconnected".into());
            }
        }
    }

    Ok(EventBatch {
        paths: event_paths(&events, root),
        events,
    })
}

fn push_event(event: notify::Result<Event>, events: &mut Vec<Event>) -> Result<(), Box<dyn Error>> {
    match event {
        Ok(event) => {
            events.push(event);
            Ok(())
        }
        Err(error) => Err(format!("notify watcher error: {error}").into()),
    }
}

fn event_paths(events: &[Event], root: &Path) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    let mut saw_empty_event = false;

    for event in events {
        if event.paths.is_empty() {
            saw_empty_event = true;
        }
        paths.extend(event.paths.iter().cloned());
    }

    if paths.is_empty() && saw_empty_event {
        paths.insert(root.to_path_buf());
    }

    paths.into_iter().collect()
}

fn print_result(label: &str, elapsed: Duration, value: &GlobMTime) {
    match value {
        GlobMTime::NoMatches => {
            println!("{label}: recompute={elapsed:?} result=no matches");
        }
        GlobMTime::MatchesFound {
            modified_at,
            designated_file,
        } => {
            println!(
                "{label}: recompute={elapsed:?} latest={} mtime={}",
                designated_file.display(),
                format_system_time(*modified_at)
            );
        }
    }
}

fn print_change_batch(
    batch: &EventBatch,
    update: &UpdateResult,
    invalidate_elapsed: Duration,
    recompute_elapsed: Duration,
    total_elapsed: Duration,
) {
    println!(
        "notify batch: events={} paths={} update_changed={} update_changes={} invalidate={:?} recompute={:?} total={:?}",
        batch.events.len(),
        batch.paths.len(),
        update.changed,
        update.change_count,
        invalidate_elapsed,
        recompute_elapsed,
        total_elapsed,
    );
    for event in batch.events.iter().take(4) {
        println!("  event: {:?} paths={:?}", event.kind, event.paths);
    }
    if batch.events.len() > 4 {
        println!("  ... {} more events", batch.events.len() - 4);
    }
    for path in batch.paths.iter().take(8) {
        println!("  invalidated: {}", path.display());
    }
    if batch.paths.len() > 8 {
        println!("  ... {} more paths", batch.paths.len() - 8);
    }
}

fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => format!(
            "{}.{:09}s since epoch",
            duration.as_secs(),
            duration.subsec_nanos()
        ),
        Err(error) => format!("{error:?} before epoch"),
    }
}

#[derive(Debug)]
struct Args {
    root: PathBuf,
    pattern: String,
    debounce_interval: Duration,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, Box<dyn Error>> {
        let mut args = args.into_iter();
        let program = args
            .next()
            .and_then(|arg| arg.into_string().ok())
            .unwrap_or_else(|| "glob_watch".to_owned());

        let Some(root) = args.next() else {
            return Err(usage_error(&program).into());
        };
        let Some(pattern) = args.next() else {
            return Err(usage_error(&program).into());
        };
        let debounce_interval = match args.next() {
            Some(value) => {
                let millis = value
                    .to_string_lossy()
                    .parse::<u64>()
                    .map_err(|error| format!("invalid debounce interval {value:?}: {error}"))?;
                Duration::from_millis(millis)
            }
            None => DEFAULT_DEBOUNCE_INTERVAL,
        };
        if let Some(extra) = args.next() {
            return Err(format!("unexpected extra argument {extra:?}\n{}", usage(&program)).into());
        }

        Ok(Self {
            root: PathBuf::from(root),
            pattern: pattern
                .into_string()
                .map_err(|value| format!("glob pattern is not valid UTF-8: {value:?}"))?,
            debounce_interval,
        })
    }
}

fn usage_error(program: &str) -> String {
    format!("missing arguments\n{}", usage(program))
}

fn usage(program: &str) -> String {
    format!("usage: {program} <root> <glob-pattern> [debounce-ms]")
}

#[derive(Debug)]
struct EventBatch {
    events: Vec<Event>,
    paths: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use notify::event::{AccessKind, EventKind};

    use super::*;

    #[test]
    fn args_default_and_custom_debounce_interval() {
        let default = Args::parse(["glob_watch", "root", "**/*.rs"].map(OsString::from)).unwrap();
        assert_eq!(default.root, PathBuf::from("root"));
        assert_eq!(default.pattern, "**/*.rs");
        assert_eq!(default.debounce_interval, DEFAULT_DEBOUNCE_INTERVAL);

        let custom =
            Args::parse(["glob_watch", "root", "*.toml", "25"].map(OsString::from)).unwrap();
        assert_eq!(custom.debounce_interval, Duration::from_millis(25));
    }

    #[test]
    fn event_paths_are_deduplicated_and_sorted() {
        let events = vec![
            Event::new(EventKind::Access(AccessKind::Any))
                .add_path(PathBuf::from("z.rs"))
                .add_path(PathBuf::from("a.rs")),
            Event::new(EventKind::Access(AccessKind::Any)).add_path(PathBuf::from("z.rs")),
        ];

        assert_eq!(
            event_paths(&events, Path::new("root")),
            vec![PathBuf::from("a.rs"), PathBuf::from("z.rs")]
        );
    }

    #[test]
    fn event_paths_falls_back_to_root_for_empty_path_events() {
        let events = vec![Event::new(EventKind::Access(AccessKind::Any))];
        assert_eq!(
            event_paths(&events, Path::new("root")),
            vec![PathBuf::from("root")]
        );
    }
}
