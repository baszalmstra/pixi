use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
use pixi_compute_fs::{
    ComputeCtxFsExt, ComputeEngineFsExt, GlobMTime, GlobMTimeKey, InputGlobSpec,
};
use pixi_vfs::IndexedVfs;
use tempfile::tempdir;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct GlobMTimeDependent {
    root: PathBuf,
    pattern: String,
}

impl std::fmt::Display for GlobMTimeDependent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GlobMTimeDependent({}, {})",
            self.root.display(),
            self.pattern
        )
    }
}

impl Key for GlobMTimeDependent {
    type Value = bool;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.global_data()
            .get::<Arc<AtomicUsize>>()
            .fetch_add(1, Ordering::SeqCst);
        matches!(
            ctx.glob_mtime(&self.root, &self.pattern).await.unwrap(),
            GlobMTime::MatchesFound { .. }
        )
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[tokio::test(flavor = "current_thread")]
async fn glob_mtime_returns_latest_matching_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
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

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let mtime = engine
        .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        mtime,
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(20),
            designated_file: newest,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn glob_mtime_reports_no_matches() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("README.md"), b"readme").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let mtime = engine
        .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(mtime, GlobMTime::NoMatches);
}

#[tokio::test(flavor = "current_thread")]
async fn glob_mtime_refreshes_after_adding_matching_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let existing = root.join("src").join("lib.rs");
    let added = root.join("src").join("new.rs");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(&existing, b"lib").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&existing, base + Duration::from_secs(1));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(1),
            designated_file: existing,
        }
    );

    std::fs::write(&added, b"new").unwrap();
    set_mtime(&added, base + Duration::from_secs(2));
    engine.invalidate_path(&added).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: added,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn glob_mtime_refreshes_after_removing_latest_matching_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let kept = root.join("src").join("keep.rs");
    let removed = root.join("src").join("remove.rs");
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(&kept, b"keep").unwrap();
    std::fs::write(&removed, b"remove").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&kept, base + Duration::from_secs(1));
    set_mtime(&removed, base + Duration::from_secs(2));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: removed.clone(),
        }
    );

    std::fs::remove_file(&removed).unwrap();
    engine.invalidate_path(&removed).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(1),
            designated_file: kept,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn glob_mtime_refreshes_after_removing_parent_of_latest_matching_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let kept = root.join("kept").join("keep.rs");
    let removed_dir = root.join("removed");
    let removed = removed_dir.join("remove.rs");
    std::fs::create_dir(root.join("kept")).unwrap();
    std::fs::create_dir(&removed_dir).unwrap();
    std::fs::write(&kept, b"keep").unwrap();
    std::fs::write(&removed, b"remove").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&kept, base + Duration::from_secs(1));
    set_mtime(&removed, base + Duration::from_secs(2));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: removed,
        }
    );

    std::fs::remove_dir_all(&removed_dir).unwrap();
    engine.invalidate_path(&removed_dir).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "**/*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(1),
            designated_file: kept,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn glob_mtime_batches_multiple_matching_path_invalidations() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let old = root.join("old.rs");
    let newer = root.join("newer.rs");
    let newest = root.join("newest.rs");
    std::fs::write(&old, b"old").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&old, base + Duration::from_secs(1));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();
    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(1),
            designated_file: old,
        }
    );

    std::fs::write(&newer, b"newer").unwrap();
    std::fs::write(&newest, b"newest").unwrap();
    set_mtime(&newer, base + Duration::from_secs(2));
    set_mtime(&newest, base + Duration::from_secs(3));

    engine.invalidate_paths([&newer, &newest]).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(root, "*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(3),
            designated_file: newest,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_vfs_glob_mtime_refreshes_from_invalidation_bridge() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let old = root.join("old.rs");
    let newest = root.join("newest.rs");
    std::fs::write(&old, b"old").unwrap();
    std::fs::write(&newest, b"newest").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&old, base + Duration::from_secs(1));
    set_mtime(&newest, base + Duration::from_secs(2));

    let vfs = Arc::new(IndexedVfs::default());
    let engine = ComputeEngine::builder().with_data(vfs.clone()).build();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(&root, "*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: newest,
        }
    );
    assert_eq!(vfs.stats().disk_metadata_reads, 0);

    let new_old_time = base + Duration::from_secs(3);
    set_mtime(&old, new_old_time);
    engine.invalidate_file(&old).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.glob_mtime(&root, "*.rs").await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: new_old_time,
            designated_file: old,
        }
    );
    assert_eq!(vfs.stats().disk_metadata_reads, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_vfs_nonmatching_change_keeps_dependent_backdated() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let src = root.join("src");
    let note = src.join("note.txt");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("lib.rs"), b"lib").unwrap();
    std::fs::write(&note, b"before").unwrap();

    let compute_count = Arc::new(AtomicUsize::new(0));
    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .with_data(compute_count.clone())
        .build();

    assert!(
        engine
            .compute(&GlobMTimeDependent {
                root: root.clone(),
                pattern: "**/*.rs".to_owned(),
            })
            .await
            .unwrap()
    );
    assert_eq!(compute_count.load(Ordering::SeqCst), 1);

    std::fs::write(&note, b"after").unwrap();
    engine.invalidate_file(&note).unwrap();

    assert!(
        engine
            .compute(&GlobMTimeDependent {
                root,
                pattern: "**/*.rs".to_owned(),
            })
            .await
            .unwrap()
    );
    assert_eq!(compute_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn non_matching_file_change_keeps_glob_mtime_dependent_backdated() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let src = root.join("src");
    let note = src.join("note.txt");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("lib.rs"), b"lib").unwrap();
    std::fs::write(&note, b"before").unwrap();

    let compute_count = Arc::new(AtomicUsize::new(0));
    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .with_data(compute_count.clone())
        .build();

    assert!(
        engine
            .compute(&GlobMTimeDependent {
                root: root.clone(),
                pattern: "**/*.rs".to_owned(),
            })
            .await
            .unwrap()
    );
    assert_eq!(compute_count.load(Ordering::SeqCst), 1);

    std::fs::write(&note, b"after").unwrap();
    engine.invalidate_path(&note).unwrap();

    assert!(
        engine
            .compute(&GlobMTimeDependent {
                root,
                pattern: "**/*.rs".to_owned(),
            })
            .await
            .unwrap()
    );
    assert_eq!(
        compute_count.load(Ordering::SeqCst),
        1,
        "unchanged glob mtime should avoid dependent recompute"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_glob_mtime_key_delegates_to_rich_input_glob_key() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let old = root.join("old.rs");
    let newest = root.join("newest.rs");
    std::fs::write(&old, b"old").unwrap();
    std::fs::write(&newest, b"newest").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&old, base + Duration::from_secs(1));
    set_mtime(&newest, base + Duration::from_secs(2));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();
    let key = GlobMTimeKey {
        root: root.clone(),
        pattern: "*.rs".to_string(),
    };

    assert_eq!(
        engine.compute(&key).await.unwrap().unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: newest,
        }
    );
    let graph = pixi_compute_engine::DependencyGraph::from_engine(&engine);
    assert!(
        graph
            .nodes()
            .any(|node| node.type_name.ends_with("InputGlobMTimeKey")),
        "legacy key should delegate to the rich input-glob key"
    );

    let new_old_time = base + Duration::from_secs(3);
    set_mtime(&old, new_old_time);
    engine.invalidate_file(&old).unwrap();

    assert_eq!(
        engine.compute(&key).await.unwrap().unwrap(),
        GlobMTime::MatchesFound {
            modified_at: new_old_time,
            designated_file: old,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn input_glob_mtime_honors_ordered_excludes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let included = root.join("included.rs");
    let excluded = root.join("excluded.rs");
    std::fs::write(&included, b"included").unwrap();
    std::fs::write(&excluded, b"excluded").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&included, base + Duration::from_secs(1));
    set_mtime(&excluded, base + Duration::from_secs(2));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let mtime = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(root, InputGlobSpec::new(["**/*.rs", "!excluded.rs"]))
                .await
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        mtime,
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(1),
            designated_file: included,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn input_glob_mtime_honors_hidden_filtering() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let visible = root.join("visible.txt");
    let hidden_dir = root.join(".pixi");
    let hidden = hidden_dir.join("hidden.txt");
    std::fs::create_dir(&hidden_dir).unwrap();
    std::fs::write(&visible, b"visible").unwrap();
    std::fs::write(&hidden, b"hidden").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&visible, base + Duration::from_secs(1));
    set_mtime(&hidden, base + Duration::from_secs(2));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let excluded = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(root, InputGlobSpec::new(["**/*.txt"]))
                .await
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        excluded,
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(1),
            designated_file: visible.clone(),
        }
    );

    let included = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(
                root,
                InputGlobSpec::new(["**/*.txt"]).with_exclude_hidden(false),
            )
            .await
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        included,
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: hidden,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn input_glob_mtime_honors_marker_leaf_and_prune_semantics() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let pkg = root.join("pkg");
    let ignored = root.join("ignored");
    std::fs::create_dir(&pkg).unwrap();
    std::fs::create_dir(&ignored).unwrap();
    let marker = pkg.join("package.xml");
    let ignored_marker = ignored.join("package.xml");
    let ignored_newest = ignored.join("newest.rs");
    let ordinary = root.join("ordinary.rs");
    std::fs::write(&marker, b"marker").unwrap();
    std::fs::write(&ignored_marker, b"marker").unwrap();
    std::fs::write(&ignored_newest, b"ignored newest").unwrap();
    std::fs::write(&ordinary, b"ordinary").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&ordinary, base + Duration::from_secs(1));
    set_mtime(&marker, base + Duration::from_secs(2));
    set_mtime(&ignored_marker, base + Duration::from_secs(3));
    set_mtime(&ignored_newest, base + Duration::from_secs(4));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let mtime = engine
        .with_ctx(async |ctx| {
            ctx.input_glob_mtime(
                root,
                InputGlobSpec::new(["**/*.rs", "**/package.xml", "!ignored/package.xml"])
                    .with_markers(["package.xml"]),
            )
            .await
        })
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        mtime,
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: marker,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn input_glob_mtime_recomputes_after_marker_membership_change() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let pkg = root.join("pkg");
    std::fs::create_dir(&pkg).unwrap();
    let marker = pkg.join("package.xml");
    let hidden_by_marker = pkg.join("newest.rs");
    std::fs::write(&marker, b"marker").unwrap();
    std::fs::write(&hidden_by_marker, b"newest").unwrap();
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    set_mtime(&marker, base + Duration::from_secs(2));
    set_mtime(&hidden_by_marker, base + Duration::from_secs(3));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();
    let spec = InputGlobSpec::new(["**/*.rs", "**/package.xml"]).with_markers(["package.xml"]);

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.input_glob_mtime(root, spec.clone()).await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(2),
            designated_file: marker.clone(),
        }
    );

    std::fs::remove_file(&marker).unwrap();
    engine.invalidate_path(&marker).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.input_glob_mtime(root, spec).await)
            .await
            .unwrap()
            .unwrap(),
        GlobMTime::MatchesFound {
            modified_at: base + Duration::from_secs(3),
            designated_file: hidden_by_marker,
        }
    );
}

fn set_mtime(path: &Path, time: SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}
