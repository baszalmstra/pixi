use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use pixi_compute_engine::ComputeEngine;
use pixi_compute_fs::{ComputeCtxFsExt, ComputeEngineFsExt, GlobMTime, InputGlobSpec};
use pixi_glob::GlobModificationTime;
use pixi_vfs::IndexedVfs;
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "manual perf comparison; run with --release -- --ignored --nocapture"]
async fn compare_glob_mtime_with_multithreaded_walker() {
    let dir_count = std::env::var("PIXI_FS_GLOB_PERF_DIRS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(80usize);
    let files_per_dir = std::env::var("PIXI_FS_GLOB_PERF_FILES_PER_DIR")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(80usize);

    let temp = tempdir().unwrap();
    let root = temp.path();
    let latest = build_fixture(root, dir_count, files_per_dir);
    let pattern = "**/*.rs";

    let matching_paths = matching_rs_paths(root, dir_count, files_per_dir);

    let start = Instant::now();
    let raw_metadata: Vec<_> = matching_paths
        .iter()
        .map(std::fs::metadata)
        .collect::<Result<_, _>>()
        .unwrap();
    let raw_metadata_elapsed = start.elapsed();
    assert_eq!(raw_metadata.len(), dir_count * files_per_dir);

    let start = Instant::now();
    let raw_walk_latest = raw_recursive_std_walk_mtime(root, "rs").unwrap().unwrap();
    let raw_walk_elapsed = start.elapsed();
    assert_eq!(raw_walk_latest.1, latest);

    let start = Instant::now();
    let baseline = GlobModificationTime::from_patterns(root, [pattern]).unwrap();
    let baseline_elapsed = start.elapsed();
    assert_baseline_latest(&baseline, &latest);

    let vfs = Arc::new(IndexedVfs::default());
    let engine = ComputeEngine::builder().with_data(vfs.clone()).build();

    let start = Instant::now();
    let cold = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(root, InputGlobSpec::new([pattern]))
                .await
        })
        .await
        .unwrap()
        .unwrap();
    let cold_elapsed = start.elapsed();
    assert_graph_latest(&cold, &latest);
    let cold_stats = engine.stats();
    let cold_vfs_stats = vfs.stats();

    let start = Instant::now();
    let warm = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(root, InputGlobSpec::new([pattern]))
                .await
        })
        .await
        .unwrap()
        .unwrap();
    let warm_elapsed = start.elapsed();
    assert_eq!(warm, cold);

    let touched = root.join("dir-000").join("nonmatch-000.txt");
    std::fs::write(&touched, b"changed nonmatching file").unwrap();
    engine.invalidate_path(&touched).unwrap();

    let nonmatch_stats_before = engine.stats();
    let vfs_nonmatch_before = vfs.stats();
    let start = Instant::now();
    let incremental_nonmatch = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(root, InputGlobSpec::new([pattern]))
                .await
        })
        .await
        .unwrap()
        .unwrap();
    let incremental_nonmatch_elapsed = start.elapsed();
    let nonmatch_stats_after = engine.stats();
    let vfs_nonmatch_after = vfs.stats();
    assert_eq!(incremental_nonmatch, cold);

    let new_latest = root.join("dir-000").join("file-000.rs");
    let new_latest_time = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_001);
    set_mtime(&new_latest, new_latest_time);
    engine.invalidate_file(&new_latest).unwrap();

    let match_stats_before = engine.stats();
    let vfs_match_before = vfs.stats();
    let start = Instant::now();
    let incremental_match = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(root, InputGlobSpec::new([pattern]))
                .await
        })
        .await
        .unwrap()
        .unwrap();
    let incremental_match_elapsed = start.elapsed();
    let match_stats_after = engine.stats();
    let vfs_match_after = vfs.stats();
    assert_eq!(
        incremental_match,
        GlobMTime::MatchesFound {
            modified_at: new_latest_time,
            designated_file: new_latest,
        }
    );

    eprintln!(
        "glob mtime perf: dirs={dir_count} files_per_dir={files_per_dir} total_files={} matched_files={}\n  raw std::fs::metadata over matches: {:?}\n  raw recursive std walk with mtime: {:?}\n  pixi_glob parallel cold: {:?}\n  compute_fs indexed cold: {:?} (nodes={}, completed_versions={}, vfs={:?})\n  compute_fs indexed warm cache hit: {:?}\n  compute_fs indexed incremental nonmatch: {:?} (completed_versions_delta={}, dirty_keys_delta={}, unchanged_ranges_delta={}, metadata_reads_delta={}, dir_reads_delta={})\n  compute_fs indexed incremental matching mtime: {:?} (completed_versions_delta={}, dirty_keys_delta={}, unchanged_ranges_delta={}, metadata_reads_delta={}, dir_reads_delta={})",
        dir_count * files_per_dir * 2,
        dir_count * files_per_dir,
        raw_metadata_elapsed,
        raw_walk_elapsed,
        baseline_elapsed,
        cold_elapsed,
        cold_stats.node_count,
        cold_stats.completed_versions,
        cold_vfs_stats,
        warm_elapsed,
        incremental_nonmatch_elapsed,
        nonmatch_stats_after.completed_versions - nonmatch_stats_before.completed_versions,
        nonmatch_stats_after.dirty_keys - nonmatch_stats_before.dirty_keys,
        nonmatch_stats_after.unchanged_ranges - nonmatch_stats_before.unchanged_ranges,
        vfs_nonmatch_after.disk_metadata_reads - vfs_nonmatch_before.disk_metadata_reads,
        vfs_nonmatch_after.disk_dir_reads - vfs_nonmatch_before.disk_dir_reads,
        incremental_match_elapsed,
        match_stats_after.completed_versions - match_stats_before.completed_versions,
        match_stats_after.dirty_keys - match_stats_before.dirty_keys,
        match_stats_after.unchanged_ranges - match_stats_before.unchanged_ranges,
        vfs_match_after.disk_metadata_reads - vfs_match_before.disk_metadata_reads,
        vfs_match_after.disk_dir_reads - vfs_match_before.disk_dir_reads,
    );
}

fn raw_recursive_std_walk_mtime(
    root: &Path,
    extension: &str,
) -> std::io::Result<Option<(SystemTime, PathBuf)>> {
    let metadata = raw_recursive_std_matching_metadata(root, extension)?;
    Ok(metadata
        .into_iter()
        .fold(None, |latest, (modified, path)| match latest {
            Some((current, current_path))
                if modified < current || (modified == current && path >= current_path) =>
            {
                Some((current, current_path))
            }
            _ => Some((modified, path)),
        }))
}

fn raw_recursive_std_matching_metadata(
    root: &Path,
    extension: &str,
) -> std::io::Result<Vec<(SystemTime, PathBuf)>> {
    let mut metadata = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let entry_metadata = entry.metadata()?;
            if entry_metadata.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|actual| actual == extension)
                && let Ok(modified) = entry_metadata.modified()
            {
                metadata.push((modified, path));
            }
        }
    }
    Ok(metadata)
}

fn matching_rs_paths(root: &Path, dir_count: usize, files_per_dir: usize) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(dir_count * files_per_dir);
    for dir_index in 0..dir_count {
        let dir = root.join(format!("dir-{dir_index:03}"));
        for file_index in 0..files_per_dir {
            paths.push(dir.join(format!("file-{file_index:03}.rs")));
        }
    }
    paths
}

fn build_fixture(root: &Path, dir_count: usize, files_per_dir: usize) -> PathBuf {
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for dir_index in 0..dir_count {
        let dir = root.join(format!("dir-{dir_index:03}"));
        std::fs::create_dir(&dir).unwrap();
        for file_index in 0..files_per_dir {
            let matched = dir.join(format!("file-{file_index:03}.rs"));
            let nonmatched = dir.join(format!("nonmatch-{file_index:03}.txt"));
            std::fs::write(&matched, b"rs").unwrap();
            std::fs::write(&nonmatched, b"txt").unwrap();
            set_mtime(
                &matched,
                base + Duration::from_secs((dir_index * files_per_dir + file_index) as u64),
            );
            set_mtime(
                &nonmatched,
                base + Duration::from_secs((dir_index * files_per_dir + file_index) as u64),
            );
        }
    }
    let latest = root
        .join(format!("dir-{:03}", dir_count - 1))
        .join(format!("file-{:03}.rs", files_per_dir - 1));
    set_mtime(
        &latest,
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000),
    );
    latest
}

fn set_mtime(path: &Path, time: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

fn assert_graph_latest(actual: &GlobMTime, latest: &Path) {
    match actual {
        GlobMTime::MatchesFound {
            designated_file, ..
        } => assert_eq!(designated_file, latest),
        GlobMTime::NoMatches => panic!("expected matching files"),
    }
}

fn assert_baseline_latest(actual: &GlobModificationTime, latest: &Path) {
    match actual {
        GlobModificationTime::MatchesFound {
            designated_file, ..
        } => assert_eq!(designated_file, latest),
        GlobModificationTime::NoMatches => panic!("expected matching files"),
    }
}
