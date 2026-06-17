//! Development-mode filesystem daemon demo.
//!
//! Run with:
//!
//! ```text
//! cargo run -p pixi_daemon --example glob_mtime_daemon -- <workspace-root> '<glob-pattern>' [query-ms]
//! ```
//!
//! The daemon invalidates filesystem state immediately from `notify` events but
//! only recomputes the glob mtime when this client loop sends a request.

use std::{
    env,
    error::Error,
    ffi::OsString,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pixi_compute_fs::{GlobMTime, InputGlobSpec};
use pixi_daemon::{GlobMTimeResponse, WorkspaceFsDaemon, WorkspaceFsDaemonOptions};

const DEFAULT_QUERY_INTERVAL: Duration = Duration::from_millis(500);
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
    let daemon = WorkspaceFsDaemon::start_with_options(
        WorkspaceFsDaemonOptions::new(&args.root).with_debounce_interval(args.debounce_interval),
    )?;

    println!(
        "workspace daemon watching {} for {:?}; query_interval={:?}; debounce={:?}",
        daemon.root().display(),
        args.pattern,
        args.query_interval,
        args.debounce_interval,
    );

    loop {
        let response = daemon
            .input_glob_mtime(".", InputGlobSpec::new([args.pattern.as_str()]))
            .await?;
        print_response(&response, &daemon.stats());
        tokio::time::sleep(args.query_interval).await;
    }
}

fn print_response(response: &GlobMTimeResponse, stats: &pixi_daemon::WorkspaceFsDaemonStats) {
    match &response.value {
        GlobMTime::NoMatches => {
            println!(
                "request: compute={:?} result=no-matches events={} invalidations={} resets={}",
                response.compute_elapsed, stats.events, stats.invalidation_batches, stats.resets,
            );
        }
        GlobMTime::MatchesFound {
            modified_at,
            designated_file,
        } => {
            println!(
                "request: compute={:?} latest={} mtime={} events={} invalidations={} resets={}",
                response.compute_elapsed,
                designated_file.display(),
                format_system_time(*modified_at),
                stats.events,
                stats.invalidation_batches,
                stats.resets,
            );
        }
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
    query_interval: Duration,
    debounce_interval: Duration,
}

impl Args {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, Box<dyn Error>> {
        let mut args = args.into_iter();
        let program = args
            .next()
            .and_then(|arg| arg.into_string().ok())
            .unwrap_or_else(|| "glob_mtime_daemon".to_owned());

        let Some(root) = args.next() else {
            return Err(usage_error(&program).into());
        };
        let Some(pattern) = args.next() else {
            return Err(usage_error(&program).into());
        };
        let query_interval = match args.next() {
            Some(value) => Duration::from_millis(value.to_string_lossy().parse::<u64>()?),
            None => DEFAULT_QUERY_INTERVAL,
        };
        if let Some(extra) = args.next() {
            return Err(format!("unexpected extra argument {extra:?}\n{}", usage(&program)).into());
        }

        Ok(Self {
            root: PathBuf::from(root),
            pattern: pattern
                .into_string()
                .map_err(|value| format!("glob pattern is not valid UTF-8: {value:?}"))?,
            query_interval,
            debounce_interval: DEFAULT_DEBOUNCE_INTERVAL,
        })
    }
}

fn usage_error(program: &str) -> String {
    format!("missing arguments\n{}", usage(program))
}

fn usage(program: &str) -> String {
    format!("usage: {program} <workspace-root> <glob-pattern> [query-ms]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_default_and_custom_query_interval() {
        let default =
            Args::parse(["glob_mtime_daemon", ".", "**/*.rs"].map(OsString::from)).unwrap();
        assert_eq!(default.root, PathBuf::from("."));
        assert_eq!(default.pattern, "**/*.rs");
        assert_eq!(default.query_interval, DEFAULT_QUERY_INTERVAL);

        let custom =
            Args::parse(["glob_mtime_daemon", ".", "*.toml", "25"].map(OsString::from)).unwrap();
        assert_eq!(custom.query_interval, Duration::from_millis(25));
    }
}
