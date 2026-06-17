use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use pixi_glob::GlobModificationTime;
use pixi_vfs::{
    EntryKind, GlobMTime, IndexedVfs, VfsBackend, VfsBackendEntry, VfsBackendMetadata, WalkMode,
};
use tempfile::tempdir;

#[test]
fn force_disk_uses_configured_backend_for_missing_real_paths() {
    #[derive(Debug)]
    struct VirtualBackend;

    impl VfsBackend for VirtualBackend {
        fn metadata(&self, _path: &Path) -> io::Result<VfsBackendMetadata> {
            Err(io::Error::new(io::ErrorKind::NotFound, "metadata unused"))
        }

        fn read_file(&self, _path: &Path) -> io::Result<Arc<[u8]>> {
            Err(io::Error::new(io::ErrorKind::NotFound, "read_file unused"))
        }

        fn read_dir(&self, path: &Path) -> io::Result<Vec<VfsBackendEntry>> {
            if path == Path::new("virtual-root") {
                Ok(vec![VfsBackendEntry {
                    path: PathBuf::from("virtual-root/lib.rs"),
                    kind: EntryKind::File,
                    len: Some(3),
                    modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10)),
                }])
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
            }
        }
    }

    let vfs = IndexedVfs::new(Arc::new(VirtualBackend));
    assert_eq!(
        vfs.latest_mtime("virtual-root", "*.rs", WalkMode::ForceDisk)
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            designated_file: PathBuf::from("virtual-root/lib.rs"),
        }
    );
}

#[test]
fn force_disk_populates_query_index_without_nonmatching_files() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.rs");
    let newest = root.join("src").join("newest.rs");
    let ignored = root.join("src").join("ignored.txt");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(&old, b"old").unwrap();
    std::fs::write(&newest, b"newest").unwrap();
    std::fs::write(&ignored, b"ignored").unwrap();

    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&old, base + Duration::from_secs(10));
    set_mtime(&newest, base + Duration::from_secs(20));
    set_mtime(&ignored, base + Duration::from_secs(30));

    let vfs = IndexedVfs::default();
    let result = vfs
        .latest_mtime(root, "**/*.rs", WalkMode::ForceDisk)
        .unwrap();

    assert_eq!(
        result,
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(20),
            designated_file: newest,
        }
    );
    let stats = vfs.stats();
    assert_eq!(stats.disk_dir_reads, 2);
    assert!(stats.disk_metadata_reads <= 2);
    assert_eq!(
        stats.file_count, 2,
        "only matching files should be indexed for the glob query"
    );
    assert_eq!(stats.dir_count, 2);
    assert!(stats.interned_paths >= 4);
}

#[test]
fn index_only_uses_the_populated_index_without_disk_reads() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("src")).unwrap();
    let file = root.join("src").join("lib.rs");
    std::fs::write(&file, b"lib").unwrap();
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100);
    set_mtime(&file, mtime);

    let vfs = IndexedVfs::default();
    assert_eq!(
        vfs.latest_mtime(root, "src/**/*.rs", WalkMode::ForceDisk)
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: mtime,
            designated_file: file.clone(),
        }
    );
    let after_disk = vfs.stats();

    assert_eq!(
        vfs.latest_mtime(root, "src/**/*.rs", WalkMode::IndexOnly)
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: mtime,
            designated_file: file,
        }
    );
    let after_index = vfs.stats();
    assert_eq!(after_index.disk_dir_reads, after_disk.disk_dir_reads);
    assert_eq!(
        after_index.disk_metadata_reads,
        after_disk.disk_metadata_reads
    );
}

#[test]
fn clear_index_drops_warm_glob_state_and_allows_repopulate() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("lib.rs");
    std::fs::write(&file, b"lib").unwrap();
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_200);
    set_mtime(&file, mtime);

    let vfs = IndexedVfs::default();
    assert_eq!(
        vfs.latest_mtime(root, "*.rs", WalkMode::ForceDisk).unwrap(),
        GlobMTime::MatchesFound {
            modified_at: mtime,
            designated_file: file.clone(),
        }
    );
    assert_eq!(vfs.stats().active_globs, 1);

    vfs.clear_index();
    let after_clear = vfs.stats();
    assert_eq!(after_clear.interned_paths, 0);
    assert_eq!(after_clear.dir_count, 0);
    assert_eq!(after_clear.file_count, 0);
    assert_eq!(after_clear.active_globs, 0);

    assert_eq!(
        vfs.latest_mtime(root, "*.rs", WalkMode::Hybrid).unwrap(),
        GlobMTime::MatchesFound {
            modified_at: mtime,
            designated_file: file,
        }
    );
    assert_eq!(vfs.stats().active_globs, 1);
}

#[test]
fn literal_non_recursive_glob_does_not_scan_subdirectories() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join("pixi.toml"), b"root").unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested").join("pixi.toml"), b"nested").unwrap();

    let root_time = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    let nested_time = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
    set_mtime(root.join("pixi.toml"), root_time);
    set_mtime(root.join("nested").join("pixi.toml"), nested_time);

    let vfs = IndexedVfs::default();
    assert_eq!(
        vfs.latest_mtime(root, "pixi.toml", WalkMode::ForceDisk)
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: root_time,
            designated_file: root.join("pixi.toml"),
        }
    );
    assert_eq!(vfs.stats().disk_dir_reads, 1);
}

#[test]
fn refresh_file_respects_non_recursive_glob_boundaries() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let root_file = root.join("root.rs");
    let nested_file = root.join("src").join("lib.rs");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(&root_file, b"root").unwrap();
    std::fs::write(&nested_file, b"nested").unwrap();
    let root_time = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    let nested_time = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
    set_mtime(&root_file, root_time);
    set_mtime(&nested_file, nested_time);

    let vfs = IndexedVfs::default();
    assert_eq!(
        vfs.latest_mtime(root, "*.rs", WalkMode::ForceDisk).unwrap(),
        GlobMTime::MatchesFound {
            modified_at: root_time,
            designated_file: root_file.clone(),
        }
    );

    let changed = vfs.refresh_file(&nested_file).unwrap();
    assert!(changed.is_empty());
    assert_eq!(
        vfs.latest_mtime(root, "*.rs", WalkMode::IndexOnly).unwrap(),
        GlobMTime::MatchesFound {
            modified_at: root_time,
            designated_file: root_file,
        }
    );
}

#[test]
fn hybrid_repairs_dirty_matching_file_without_directory_rescan() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let file = root.join("src").join("lib.rs");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(&file, b"lib").unwrap();
    let first = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let second = SystemTime::UNIX_EPOCH + Duration::from_secs(200);
    set_mtime(&file, first);

    let vfs = IndexedVfs::default();
    assert_eq!(
        vfs.latest_mtime(root, "**/*.rs", WalkMode::ForceDisk)
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: first,
            designated_file: file.clone(),
        }
    );
    let after_disk = vfs.stats();

    set_mtime(&file, second);
    vfs.mark_file_dirty(&file).unwrap();
    assert_eq!(
        vfs.latest_mtime(root, "**/*.rs", WalkMode::Hybrid).unwrap(),
        GlobMTime::MatchesFound {
            modified_at: second,
            designated_file: file,
        }
    );
    let after_hybrid = vfs.stats();
    assert_eq!(after_hybrid.disk_dir_reads, after_disk.disk_dir_reads);
    assert_eq!(
        after_hybrid.disk_metadata_reads,
        after_disk.disk_metadata_reads + 1
    );
}

#[test]
fn refresh_file_updates_active_query_state_incrementally() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    let old = root.join("old.rs");
    let newest = root.join("newest.rs");
    std::fs::write(&old, b"old").unwrap();
    std::fs::write(&newest, b"newest").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&old, base + Duration::from_secs(1));
    set_mtime(&newest, base + Duration::from_secs(2));

    let vfs = IndexedVfs::default();
    assert_eq!(
        vfs.latest_mtime(root, "*.rs", WalkMode::ForceDisk).unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: newest.clone(),
        }
    );
    let after_walk = vfs.stats();

    let new_old_time = base + Duration::from_secs(3);
    set_mtime(&old, new_old_time);
    let changed = vfs.refresh_file(&old).unwrap();

    assert_eq!(changed.len(), 1);
    assert_eq!(
        vfs.latest_mtime(root, "*.rs", WalkMode::IndexOnly).unwrap(),
        GlobMTime::MatchesFound {
            modified_at: new_old_time,
            designated_file: old,
        }
    );
    let after_refresh = vfs.stats();
    assert_eq!(after_refresh.disk_dir_reads, after_walk.disk_dir_reads);
    assert_eq!(
        after_refresh.disk_metadata_reads,
        after_walk.disk_metadata_reads + 1
    );
}

#[test]
fn results_match_existing_parallel_glob_mtime() {
    let temp = tempdir().unwrap();
    let root = temp.path();
    build_fixture(root, 8, 8);

    let vfs = IndexedVfs::default();
    let indexed = vfs
        .latest_mtime(root, "**/*.rs", WalkMode::ForceDisk)
        .unwrap();
    let baseline = GlobModificationTime::from_patterns(root, ["**/*.rs"]).unwrap();

    match (indexed, baseline) {
        (
            GlobMTime::MatchesFound {
                modified_at,
                designated_file,
            },
            GlobModificationTime::MatchesFound {
                modified_at: baseline_mtime,
                designated_file: baseline_file,
            },
        ) => {
            assert_eq!(modified_at, baseline_mtime);
            assert_eq!(designated_file, baseline_file);
        }
        (GlobMTime::NoMatches, GlobModificationTime::NoMatches) => {}
        other => panic!("mismatch: {other:?}"),
    }
}

fn build_fixture(root: &Path, dirs: usize, files: usize) {
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for dir_index in 0..dirs {
        let dir = root.join(format!("dir-{dir_index:03}"));
        std::fs::create_dir(&dir).unwrap();
        for file_index in 0..files {
            let rs = dir.join(format!("file-{file_index:03}.rs"));
            let txt = dir.join(format!("file-{file_index:03}.txt"));
            std::fs::write(&rs, b"rs").unwrap();
            std::fs::write(&txt, b"txt").unwrap();
            set_mtime(
                &rs,
                base + Duration::from_secs((dir_index * files + file_index) as u64),
            );
            set_mtime(
                &txt,
                base + Duration::from_secs((dirs * files + dir_index * files + file_index) as u64),
            );
        }
    }
}

fn set_mtime(path: impl AsRef<Path>, time: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time)).unwrap();
}
