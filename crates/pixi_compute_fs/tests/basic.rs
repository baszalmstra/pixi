use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
use pixi_compute_fs::{ComputeCtxFsExt, ComputeEngineFsExt, FsEntryKind, FsError};
use pixi_vfs::{EntryKind, IndexedVfs, VfsBackend, VfsBackendEntry, VfsBackendMetadata};
use tempfile::tempdir;

#[tokio::test(flavor = "current_thread")]
async fn read_file_uses_compute_ctx_extension() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("pixi.toml");
    std::fs::write(&file, b"hello").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let contents = engine
        .with_ctx(async |ctx| ctx.read_file(&file).await)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(&*contents, b"hello");
}

#[tokio::test(flavor = "current_thread")]
async fn read_dir_returns_sorted_entries() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let entries = engine
        .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
        .await
        .unwrap()
        .unwrap();

    let names: Vec<_> = entries
        .iter()
        .map(|entry| entry.file_name.as_str())
        .collect();
    assert_eq!(names, ["a.txt", "b.txt", "nested"]);
    assert_eq!(entries[0].kind, FsEntryKind::File);
    assert_eq!(entries[2].kind, FsEntryKind::Directory);
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_reports_missing_without_panicking() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing.txt");

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let metadata = engine
        .with_ctx(async |ctx| ctx.metadata(&missing).await)
        .await
        .unwrap()
        .unwrap();

    assert!(!metadata.exists);
    assert_eq!(metadata.kind, None);
}

#[tokio::test(flavor = "current_thread")]
async fn missing_file_recovers_after_create_and_invalidate() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("created-later.txt");

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert!(matches!(
        engine
            .with_ctx(async |ctx| ctx.read_file(&file).await)
            .await
            .unwrap(),
        Err(FsError::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        })
    ));

    std::fs::write(&file, b"now-present").unwrap();
    engine.invalidate_path(&file).unwrap();

    let contents = engine
        .with_ctx(async |ctx| ctx.read_file(&file).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*contents, b"now-present");
}

#[tokio::test(flavor = "current_thread")]
async fn missing_directory_recovers_after_create_and_invalidate() {
    let dir = tempdir().unwrap();
    let missing_dir = dir.path().join("created-later");

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert!(matches!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(&missing_dir).await)
            .await
            .unwrap(),
        Err(FsError::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        })
    ));

    std::fs::create_dir(&missing_dir).unwrap();
    std::fs::write(missing_dir.join("child.txt"), b"child").unwrap();
    engine.invalidate_path(&missing_dir).unwrap();

    let entries = engine
        .with_ctx(async |ctx| ctx.read_dir(&missing_dir).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entries[0].file_name, "child.txt");
}

#[tokio::test(flavor = "current_thread")]
async fn unchanged_missing_file_error_skips_dependent_recompute() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("still-missing.txt");
    let compute_count = Arc::new(AtomicUsize::new(0));

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .with_data(compute_count.clone())
        .build();

    assert!(
        engine
            .compute(&MissingFileObserved(file.clone()))
            .await
            .unwrap()
    );
    assert_eq!(compute_count.load(Ordering::SeqCst), 1);

    engine.invalidate_path(&file).unwrap();

    assert!(engine.compute(&MissingFileObserved(file)).await.unwrap());
    assert_eq!(
        compute_count.load(Ordering::SeqCst),
        1,
        "unchanged missing-file error should be backdated and avoid parent recompute"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_invalidation_recomputes_changed_file() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("data.txt");
    std::fs::write(&file, b"one").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let first = engine
        .with_ctx(async |ctx| ctx.read_file(file.clone()).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*first, b"one");

    std::fs::write(&file, b"two").unwrap();
    engine.invalidate_path(file.clone()).unwrap();

    let second = engine
        .with_ctx(async |ctx| ctx.read_file(file).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*second, b"two");
}

#[tokio::test(flavor = "current_thread")]
async fn invalidating_child_path_refreshes_parent_directory_listing() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("new.txt");

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let first = engine
        .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
        .await
        .unwrap()
        .unwrap();
    assert!(first.is_empty());

    std::fs::write(&file, b"new").unwrap();
    engine.invalidate_path(file).unwrap();

    let second = engine
        .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
        .await
        .unwrap()
        .unwrap();
    let names: Vec<_> = second
        .iter()
        .map(|entry| entry.file_name.as_str())
        .collect();
    assert_eq!(names, ["new.txt"]);
}

#[tokio::test(flavor = "current_thread")]
async fn delete_file_refreshes_contents_metadata_and_parent_listing() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("gone.txt");
    std::fs::write(&file, b"old").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let initial_contents = engine
        .with_ctx(async |ctx| ctx.read_file(&file).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*initial_contents, b"old");
    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.metadata(&file).await)
            .await
            .unwrap()
            .unwrap()
            .kind,
        Some(FsEntryKind::File)
    );
    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
            .await
            .unwrap()
            .unwrap()
            .len(),
        1
    );

    std::fs::remove_file(&file).unwrap();
    engine.invalidate_path(&file).unwrap();

    let metadata = engine
        .with_ctx(async |ctx| ctx.metadata(&file).await)
        .await
        .unwrap()
        .unwrap();
    assert!(!metadata.exists);
    assert!(matches!(
        engine
            .with_ctx(async |ctx| ctx.read_file(&file).await)
            .await
            .unwrap(),
        Err(FsError::Io { .. })
    ));
    assert!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_retains_precomputed_file_after_current_invalidation() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("data.txt");
    std::fs::write(&file, b"old").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();
    let snapshot = engine.snapshot();

    let snapshot_first = snapshot
        .with_ctx(async |ctx| ctx.read_file(&file).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*snapshot_first, b"old");

    std::fs::write(&file, b"new").unwrap();
    engine.invalidate_path(&file).unwrap();

    let current = engine
        .with_ctx(async |ctx| ctx.read_file(&file).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*current, b"new");

    let snapshot_again = snapshot
        .with_ctx(async |ctx| ctx.read_file(&file).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*snapshot_again, b"old");
}

#[tokio::test(flavor = "current_thread")]
async fn invalidate_paths_batches_multiple_paths_into_one_version() {
    let dir = tempdir().unwrap();
    let old = dir.path().join("old.txt");
    let new = dir.path().join("new.txt");
    std::fs::write(&old, b"contents").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
            .await
            .unwrap()
            .unwrap()[0]
            .file_name,
        "old.txt"
    );
    let before = engine.current_version();

    std::fs::rename(&old, &new).unwrap();
    let report = engine.invalidate_paths([&old, &new]).unwrap();
    assert_eq!(report.previous_version, before);
    assert_eq!(report.current_version, engine.current_version());
    assert!(report.changed);

    let listing = engine
        .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
        .await
        .unwrap()
        .unwrap();
    let names: Vec<_> = listing
        .iter()
        .map(|entry| entry.file_name.as_str())
        .collect();
    assert_eq!(names, ["new.txt"]);
}

#[tokio::test(flavor = "current_thread")]
async fn same_parent_rename_can_be_invalidated_in_one_batch() {
    let dir = tempdir().unwrap();
    let old = dir.path().join("old.txt");
    let new = dir.path().join("new.txt");
    std::fs::write(&old, b"contents").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert_eq!(
        &*engine
            .with_ctx(async |ctx| ctx.read_file(&old).await)
            .await
            .unwrap()
            .unwrap(),
        b"contents"
    );
    let first_listing = engine
        .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_listing[0].file_name, "old.txt");

    std::fs::rename(&old, &new).unwrap();
    let report = engine.invalidate_paths([&old, &new]).unwrap();
    assert!(report.changed);

    let listing = engine
        .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
        .await
        .unwrap()
        .unwrap();
    let names: Vec<_> = listing
        .iter()
        .map(|entry| entry.file_name.as_str())
        .collect();
    assert_eq!(names, ["new.txt"]);
    assert!(
        !engine
            .with_ctx(async |ctx| ctx.metadata(&old).await)
            .await
            .unwrap()
            .unwrap()
            .exists
    );
    assert_eq!(
        &*engine
            .with_ctx(async |ctx| ctx.read_file(&new).await)
            .await
            .unwrap()
            .unwrap(),
        b"contents"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn file_to_directory_transition_refreshes_path_kind() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("entry");
    std::fs::write(&path, b"file").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.metadata(&path).await)
            .await
            .unwrap()
            .unwrap()
            .kind,
        Some(FsEntryKind::File)
    );
    assert_eq!(
        &*engine
            .with_ctx(async |ctx| ctx.read_file(&path).await)
            .await
            .unwrap()
            .unwrap(),
        b"file"
    );

    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("child.txt"), b"child").unwrap();
    engine.invalidate_path(&path).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.metadata(&path).await)
            .await
            .unwrap()
            .unwrap()
            .kind,
        Some(FsEntryKind::Directory)
    );
    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(&path).await)
            .await
            .unwrap()
            .unwrap()[0]
            .file_name,
        "child.txt"
    );
    assert!(matches!(
        engine
            .with_ctx(async |ctx| ctx.read_file(&path).await)
            .await
            .unwrap(),
        Err(FsError::Io { .. })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn directory_to_file_transition_refreshes_path_kind() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("entry");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("child.txt"), b"child").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.metadata(&path).await)
            .await
            .unwrap()
            .unwrap()
            .kind,
        Some(FsEntryKind::Directory)
    );
    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(&path).await)
            .await
            .unwrap()
            .unwrap()[0]
            .file_name,
        "child.txt"
    );

    std::fs::remove_file(path.join("child.txt")).unwrap();
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, b"file").unwrap();
    engine.invalidate_path(&path).unwrap();

    assert_eq!(
        engine
            .with_ctx(async |ctx| ctx.metadata(&path).await)
            .await
            .unwrap()
            .unwrap()
            .kind,
        Some(FsEntryKind::File)
    );
    assert_eq!(
        &*engine
            .with_ctx(async |ctx| ctx.read_file(&path).await)
            .await
            .unwrap()
            .unwrap(),
        b"file"
    );
    assert!(matches!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(&path).await)
            .await
            .unwrap(),
        Err(FsError::Io { .. })
    ));
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct MissingFileObserved(PathBuf);

impl std::fmt::Display for MissingFileObserved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MissingFileObserved({})", self.0.display())
    }
}

impl Key for MissingFileObserved {
    type Value = bool;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.global_data()
            .get::<Arc<AtomicUsize>>()
            .fetch_add(1, Ordering::SeqCst);
        matches!(
            ctx.read_file(&self.0).await,
            Err(FsError::Io {
                kind: std::io::ErrorKind::NotFound,
                ..
            })
        )
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

struct ReadOnlyVfsBackend;

#[tokio::test(flavor = "current_thread")]
async fn indexed_vfs_supplies_metadata_when_registered() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("data.txt");
    std::fs::write(&file, b"real bytes").unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let metadata = engine
        .with_ctx(async |ctx| ctx.metadata(&file).await)
        .await
        .unwrap()
        .unwrap();

    assert!(metadata.exists);
    assert_eq!(metadata.kind, Some(FsEntryKind::File));
    assert_eq!(metadata.len, Some(10));
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_vfs_supplies_read_dir_when_registered() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let entries = engine
        .with_ctx(async |ctx| ctx.read_dir(dir.path()).await)
        .await
        .unwrap()
        .unwrap();

    let names: Vec<_> = entries
        .iter()
        .map(|entry| entry.file_name.as_str())
        .collect();
    assert_eq!(names, ["a.txt", "b.txt", "nested"]);
    assert_eq!(entries[0].kind, FsEntryKind::File);
    assert_eq!(entries[2].kind, FsEntryKind::Directory);
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_vfs_directory_listing_repairs_after_path_invalidation() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let new_file = root.join("new.txt");

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    let first = engine
        .with_ctx(async |ctx| ctx.read_dir(&root).await)
        .await
        .unwrap()
        .unwrap();
    assert!(first.is_empty());

    std::fs::write(&new_file, b"new").unwrap();
    engine.invalidate_path(&new_file).unwrap();

    let second = engine
        .with_ctx(async |ctx| ctx.read_dir(&root).await)
        .await
        .unwrap()
        .unwrap();
    let names: Vec<_> = second
        .iter()
        .map(|entry| entry.file_name.as_str())
        .collect();
    assert_eq!(names, ["new.txt"]);
}

impl VfsBackend for ReadOnlyVfsBackend {
    fn metadata(&self, _path: &std::path::Path) -> std::io::Result<VfsBackendMetadata> {
        Ok(VfsBackendMetadata {
            kind: EntryKind::File,
            len: 9,
            modified: None,
        })
    }

    fn read_file(&self, _path: &std::path::Path) -> std::io::Result<Arc<[u8]>> {
        Ok(Arc::from(&b"vfs bytes"[..]))
    }

    fn read_dir(&self, _path: &std::path::Path) -> std::io::Result<Vec<VfsBackendEntry>> {
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_vfs_supplies_read_file_when_registered() {
    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::new(Arc::new(ReadOnlyVfsBackend))))
        .build();

    let contents = engine
        .with_ctx(async |ctx| ctx.read_file(PathBuf::from("anything")).await)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(&*contents, b"vfs bytes");
}

#[tokio::test(flavor = "current_thread")]
async fn vfs_backend_is_swappable_for_tests() {
    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::new(Arc::new(ReadOnlyVfsBackend))))
        .build();

    let contents = engine
        .with_ctx(async |ctx| ctx.read_file(PathBuf::from("anything")).await)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(&*contents, b"vfs bytes");
}
