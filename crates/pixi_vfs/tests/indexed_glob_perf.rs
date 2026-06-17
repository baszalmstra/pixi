use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use pixi_glob::GlobModificationTime;
use pixi_vfs::{GlobMTime, IndexedVfs, WalkMode};
use tempfile::tempdir;

#[test]
#[ignore = "manual perf comparison; run with --release -- --ignored --nocapture"]
fn compare_indexed_walker_modes() {
    let dir_count = std::env::var("PIXI_VFS_GLOB_PERF_DIRS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(80usize);
    let files_per_dir = std::env::var("PIXI_VFS_GLOB_PERF_FILES_PER_DIR")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(80usize);

    let temp = tempdir().unwrap();
    let root = temp.path();
    let latest = build_fixture(root, dir_count, files_per_dir);
    let pattern = "**/*.rs";

    let start = Instant::now();
    let baseline = GlobModificationTime::from_patterns(root, [pattern]).unwrap();
    let baseline_elapsed = start.elapsed();
    assert_baseline_latest(&baseline, &latest);

    let diagnostic_vfs = IndexedVfs::default();
    let (_, force_disk_diag) = diagnostic_vfs
        .latest_mtime_with_diagnostics(root, pattern, WalkMode::ForceDisk)
        .unwrap();

    let vfs = IndexedVfs::default();

    let start = Instant::now();
    let force_disk = vfs
        .latest_mtime(root, pattern, WalkMode::ForceDisk)
        .unwrap();
    let force_disk_elapsed = start.elapsed();
    assert_graph_latest(&force_disk, &latest);
    let force_disk_stats = vfs.stats();

    let start = Instant::now();
    let (index_only, index_only_diag) = vfs
        .latest_mtime_with_diagnostics(root, pattern, WalkMode::IndexOnly)
        .unwrap();
    let index_only_elapsed = start.elapsed();
    assert_eq!(index_only, force_disk);
    let index_only_stats = vfs.stats();
    assert_eq!(
        index_only_stats.disk_dir_reads,
        force_disk_stats.disk_dir_reads
    );
    assert_eq!(
        index_only_stats.disk_metadata_reads,
        force_disk_stats.disk_metadata_reads
    );

    let dirty_match = root.join("dir-000").join("file-000.rs");
    let dirty_match_time = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_001);
    set_mtime(&dirty_match, dirty_match_time);
    vfs.mark_file_dirty(&dirty_match).unwrap();
    let before_hybrid = vfs.stats();
    let start = Instant::now();
    let (hybrid, hybrid_diag) = vfs
        .latest_mtime_with_diagnostics(root, pattern, WalkMode::Hybrid)
        .unwrap();
    let hybrid_elapsed = start.elapsed();
    assert_eq!(
        hybrid,
        GlobMTime::MatchesFound {
            modified_at: dirty_match_time,
            designated_file: dirty_match.clone(),
        }
    );
    let after_hybrid = vfs.stats();

    let refresh_match = root.join("dir-001").join("file-001.rs");
    let refresh_match_time = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_002);
    set_mtime(&refresh_match, refresh_match_time);
    let before_refresh = vfs.stats();
    let start = Instant::now();
    let changed = vfs.refresh_file(&refresh_match).unwrap();
    let refresh_elapsed = start.elapsed();
    assert_eq!(changed.len(), 1);
    assert_eq!(
        changed[0].current,
        GlobMTime::MatchesFound {
            modified_at: refresh_match_time,
            designated_file: refresh_match.clone(),
        }
    );
    let after_refresh = vfs.stats();

    let start = Instant::now();
    let (after_refresh_index_only, after_refresh_index_only_diag) = vfs
        .latest_mtime_with_diagnostics(root, pattern, WalkMode::IndexOnly)
        .unwrap();
    let after_refresh_index_only_elapsed = start.elapsed();
    assert_eq!(after_refresh_index_only, changed[0].current);

    eprintln!(
        "indexed glob perf: dirs={dir_count} files_per_dir={files_per_dir} total_files={} matched_files={}\n  pixi_glob parallel cold: {:?}\n  indexed force-disk populate: {:?} (dirs_read={}, metadata_read={}, paths={}, dirs={}, files={})\n    diagnostic force-disk run: total={:?} disk_walk={:?} read_dir_cumulative={:?} glob_match_cumulative={:?} commit_total={:?} sort_dedup={:?} commit_dirs={:?} commit_files={:?}\n    counts: dirs_visited={} entries_seen={} file_candidates={} matched_seen={} dirs_indexed={} files_indexed={}\n  indexed index-only query: {:?} (cache_hit={}, diag_total={:?})\n  indexed hybrid dirty-file repair: {:?} (cache_hit={}, diag_total={:?}, dir_reads_delta={}, metadata_reads_delta={})\n  indexed refresh_file active-query update: {:?} (dir_reads_delta={}, metadata_reads_delta={}, changed_queries={})\n  indexed post-refresh index-only query: {:?} (cache_hit={}, diag_total={:?})",
        dir_count * files_per_dir * 2,
        dir_count * files_per_dir,
        baseline_elapsed,
        force_disk_elapsed,
        force_disk_stats.disk_dir_reads,
        force_disk_stats.disk_metadata_reads,
        force_disk_stats.interned_paths,
        force_disk_stats.dir_count,
        force_disk_stats.file_count,
        force_disk_diag.total,
        force_disk_diag.walk.disk_walk,
        force_disk_diag.walk.read_dir_cumulative,
        force_disk_diag.walk.glob_match_cumulative,
        force_disk_diag.walk.commit_total,
        force_disk_diag.walk.sort_dedup,
        force_disk_diag.walk.commit_dirs,
        force_disk_diag.walk.commit_files,
        force_disk_diag.walk.dirs_visited,
        force_disk_diag.walk.entries_seen,
        force_disk_diag.walk.file_candidates,
        force_disk_diag.walk.matched_files_seen,
        force_disk_diag.walk.dirs_indexed,
        force_disk_diag.walk.files_indexed,
        index_only_elapsed,
        index_only_diag.cache_hit,
        index_only_diag.total,
        hybrid_elapsed,
        hybrid_diag.cache_hit,
        hybrid_diag.total,
        after_hybrid.disk_dir_reads - before_hybrid.disk_dir_reads,
        after_hybrid.disk_metadata_reads - before_hybrid.disk_metadata_reads,
        refresh_elapsed,
        after_refresh.disk_dir_reads - before_refresh.disk_dir_reads,
        after_refresh.disk_metadata_reads - before_refresh.disk_metadata_reads,
        changed.len(),
        after_refresh_index_only_elapsed,
        after_refresh_index_only_diag.cache_hit,
        after_refresh_index_only_diag.total,
    );
}

fn build_fixture(root: &Path, dir_count: usize, files_per_dir: usize) -> PathBuf {
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut latest = root.to_path_buf();
    for dir_index in 0..dir_count {
        let dir = root.join(format!("dir-{dir_index:03}"));
        std::fs::create_dir(&dir).unwrap();
        for file_index in 0..files_per_dir {
            let rs = dir.join(format!("file-{file_index:03}.rs"));
            let txt = dir.join(format!("file-{file_index:03}.txt"));
            std::fs::write(&rs, b"rs").unwrap();
            std::fs::write(&txt, b"txt").unwrap();
            let mut rs_mtime =
                base + Duration::from_secs((dir_index * files_per_dir + file_index) as u64);
            if dir_index == dir_count - 1 && file_index == files_per_dir - 1 {
                rs_mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
                latest = rs.clone();
            }
            set_mtime(&rs, rs_mtime);
            set_mtime(
                &txt,
                base + Duration::from_secs(
                    (dir_count * files_per_dir + dir_index * files_per_dir + file_index) as u64,
                ),
            );
        }
    }
    latest
}

fn assert_baseline_latest(baseline: &GlobModificationTime, latest: &Path) {
    match baseline {
        GlobModificationTime::MatchesFound {
            designated_file, ..
        } => assert_eq!(designated_file, latest),
        GlobModificationTime::NoMatches => panic!("expected baseline matches"),
    }
}

fn assert_graph_latest(actual: &GlobMTime, latest: &Path) {
    match actual {
        GlobMTime::MatchesFound {
            designated_file, ..
        } => assert_eq!(designated_file, latest),
        GlobMTime::NoMatches => panic!("expected indexed matches"),
    }
}

fn set_mtime(path: impl AsRef<Path>, time: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
}
