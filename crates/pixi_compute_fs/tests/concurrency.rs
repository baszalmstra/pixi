use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use futures::future::join_all;
use pixi_compute_engine::ComputeEngine;
use pixi_compute_fs::{ComputeCtxFsExt, ComputeEngineFsExt};
use pixi_vfs::{EntryKind, IndexedVfs, VfsBackend, VfsBackendEntry, VfsBackendMetadata};
use tempfile::tempdir;

struct CountingDelayVfsBackend {
    files: Mutex<BTreeMap<PathBuf, Arc<[u8]>>>,
    read_file_calls: AtomicUsize,
    read_started: (Mutex<bool>, Condvar),
    released: (Mutex<bool>, Condvar),
}

impl CountingDelayVfsBackend {
    fn new() -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            read_file_calls: AtomicUsize::new(0),
            read_started: (Mutex::new(false), Condvar::new()),
            released: (Mutex::new(false), Condvar::new()),
        }
    }

    fn release_reads(&self) {
        let (lock, condvar) = &self.released;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }

    fn wait_read_started(&self) {
        let (lock, condvar) = &self.read_started;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = condvar.wait(started).unwrap();
        }
    }

    fn set_file(&self, path: PathBuf, contents: &'static [u8]) {
        self.files.lock().unwrap().insert(path, Arc::from(contents));
    }

    fn read_file_calls(&self) -> usize {
        self.read_file_calls.load(Ordering::SeqCst)
    }
}

impl VfsBackend for CountingDelayVfsBackend {
    fn metadata(&self, path: &Path) -> io::Result<VfsBackendMetadata> {
        let files = self.files.lock().unwrap();
        let Some(contents) = files.get(path) else {
            return Err(not_found());
        };
        Ok(VfsBackendMetadata {
            kind: EntryKind::File,
            len: contents.len() as u64,
            modified: None,
        })
    }

    fn read_file(&self, path: &Path) -> io::Result<Arc<[u8]>> {
        self.read_file_calls.fetch_add(1, Ordering::SeqCst);
        let (started_lock, started_condvar) = &self.read_started;
        *started_lock.lock().unwrap() = true;
        started_condvar.notify_all();

        let (released_lock, released_condvar) = &self.released;
        let mut released = released_lock.lock().unwrap();
        while !*released {
            released = released_condvar.wait(released).unwrap();
        }

        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(not_found)
    }

    fn read_dir(&self, _path: &Path) -> io::Result<Vec<VfsBackendEntry>> {
        Ok(Vec::new())
    }
}

struct BlockingFirstReadVfsBackend {
    files: Mutex<BTreeMap<PathBuf, Arc<[u8]>>>,
    read_file_calls: AtomicUsize,
    first_read_started: (Mutex<bool>, Condvar),
    unblock_first_read: (Mutex<bool>, Condvar),
}

impl BlockingFirstReadVfsBackend {
    fn new() -> Self {
        Self {
            files: Mutex::new(BTreeMap::new()),
            read_file_calls: AtomicUsize::new(0),
            first_read_started: (Mutex::new(false), Condvar::new()),
            unblock_first_read: (Mutex::new(false), Condvar::new()),
        }
    }

    fn set_file(&self, path: PathBuf, contents: &'static [u8]) {
        self.files.lock().unwrap().insert(path, Arc::from(contents));
    }

    fn wait_first_read_started(&self) {
        let (lock, condvar) = &self.first_read_started;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = condvar.wait(started).unwrap();
        }
    }

    fn unblock_first_read(&self) {
        let (lock, condvar) = &self.unblock_first_read;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }
}

impl VfsBackend for BlockingFirstReadVfsBackend {
    fn metadata(&self, path: &Path) -> io::Result<VfsBackendMetadata> {
        let files = self.files.lock().unwrap();
        let Some(contents) = files.get(path) else {
            return Err(not_found());
        };
        Ok(VfsBackendMetadata {
            kind: EntryKind::File,
            len: contents.len() as u64,
            modified: None,
        })
    }

    fn read_file(&self, path: &Path) -> io::Result<Arc<[u8]>> {
        let call = self.read_file_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let contents = self
            .files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(not_found)?;
        if call == 1 {
            let (started_lock, started_condvar) = &self.first_read_started;
            *started_lock.lock().unwrap() = true;
            started_condvar.notify_all();

            let (unblock_lock, unblock_condvar) = &self.unblock_first_read;
            let mut unblocked = unblock_lock.lock().unwrap();
            while !*unblocked {
                unblocked = unblock_condvar.wait(unblocked).unwrap();
            }
        }
        Ok(contents)
    }

    fn read_dir(&self, _path: &Path) -> io::Result<Vec<VfsBackendEntry>> {
        Ok(Vec::new())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_snapshot_reads_share_one_backend_read() {
    let path = PathBuf::from("/workspace/pixi.toml");
    let backend = Arc::new(CountingDelayVfsBackend::new());
    backend.set_file(path.clone(), b"shared");
    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::new(backend.clone())))
        .build();
    let snapshot = engine.snapshot();

    let reads = (0..16).map(|_| {
        let snapshot = snapshot.clone();
        let path = path.clone();
        tokio::spawn(async move {
            snapshot
                .with_ctx(async |ctx| ctx.read_file(&path).await)
                .await
                .unwrap()
                .unwrap()
        })
    });

    let reads: Vec<_> = reads.collect();
    backend.wait_read_started();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    backend.release_reads();

    let results = join_all(reads).await;
    for contents in results {
        let contents = contents.unwrap();
        assert_eq!(&*contents, b"shared");
    }
    assert_eq!(backend.read_file_calls(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_snapshot_file_read_survives_current_invalidation() {
    let path = PathBuf::from("/workspace/pixi.toml");
    let backend = Arc::new(BlockingFirstReadVfsBackend::new());
    backend.set_file(path.clone(), b"old");
    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::new(backend.clone())))
        .build();
    let snapshot = engine.snapshot();

    let snapshot_read = {
        let snapshot = snapshot.clone();
        let path = path.clone();
        tokio::spawn(async move {
            snapshot
                .with_ctx(async |ctx| ctx.read_file(&path).await)
                .await
                .unwrap()
                .unwrap()
        })
    };

    backend.wait_first_read_started();
    backend.set_file(path.clone(), b"new");
    engine.invalidate_path(&path).unwrap();
    backend.unblock_first_read();

    let snapshot_contents = snapshot_read.await.unwrap();
    assert_eq!(&*snapshot_contents, b"old");

    let current_contents = engine
        .with_ctx(async |ctx| ctx.read_file(&path).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*current_contents, b"new");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_invalidations_and_reads_leave_final_state_usable() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::default()))
        .build();

    assert!(
        engine
            .with_ctx(async |ctx| ctx.read_dir(&root).await)
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );

    let tasks = (0..24).map(|index| {
        let engine = engine.clone();
        let path = root.join(format!("file-{index}.txt"));
        tokio::spawn(async move {
            let contents = format!("contents-{index}");
            std::fs::write(&path, contents.as_bytes()).unwrap();
            engine.invalidate_paths([&path]).unwrap();
            let observed = engine
                .with_ctx(async |ctx| ctx.read_file(&path).await)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&*observed, contents.as_bytes());
        })
    });

    for task in tasks {
        task.await.unwrap();
    }

    let listing = engine
        .with_ctx(async |ctx| ctx.read_dir(&root).await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(listing.len(), 24);
    for index in 0..24 {
        let path = root.join(format!("file-{index}.txt"));
        let contents = engine
            .with_ctx(async |ctx| ctx.read_file(&path).await)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&*contents, format!("contents-{index}").as_bytes());
    }
}

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "not found")
}
