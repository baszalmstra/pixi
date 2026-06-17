use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};
use pixi_compute_fs::{ComputeCtxFsExt, ComputeEngineFsExt, FsError};
use pixi_vfs::{EntryKind, IndexedVfs, VfsBackend, VfsBackendEntry, VfsBackendMetadata};

#[derive(Default)]
struct TrackingFs {
    dirs: Mutex<BTreeMap<PathBuf, BTreeSet<String>>>,
    files: Mutex<BTreeMap<PathBuf, Arc<[u8]>>>,
    read_file_calls: AtomicUsize,
}

impl TrackingFs {
    fn set_file(&self, path: PathBuf, contents: &'static [u8]) {
        self.files.lock().unwrap().insert(path, Arc::from(contents));
    }

    fn remove_file(&self, path: &PathBuf) {
        self.files.lock().unwrap().remove(path);
    }

    fn set_dir_entries(&self, path: PathBuf, entries: impl IntoIterator<Item = &'static str>) {
        self.dirs.lock().unwrap().insert(
            path,
            entries
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>(),
        );
    }

    fn read_file_calls(&self) -> usize {
        self.read_file_calls.load(Ordering::SeqCst)
    }
}

impl VfsBackend for TrackingFs {
    fn metadata(&self, path: &Path) -> io::Result<VfsBackendMetadata> {
        let files = self.files.lock().unwrap();
        if let Some(contents) = files.get(path) {
            return Ok(VfsBackendMetadata {
                kind: EntryKind::File,
                len: contents.len() as u64,
                modified: None,
            });
        }
        drop(files);

        if self.dirs.lock().unwrap().contains_key(path) {
            return Ok(VfsBackendMetadata {
                kind: EntryKind::Directory,
                len: 0,
                modified: None,
            });
        }

        Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
    }

    fn read_file(&self, path: &Path) -> io::Result<Arc<[u8]>> {
        self.read_file_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_else(|| Arc::from(&b"missing"[..])))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<VfsBackendEntry>> {
        let entries = self
            .dirs
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default();
        Ok(entries
            .into_iter()
            .map(|file_name| VfsBackendEntry {
                path: path.join(&file_name),
                kind: EntryKind::File,
                len: self
                    .files
                    .lock()
                    .unwrap()
                    .get(&path.join(&file_name))
                    .map(|contents| contents.len() as u64),
                modified: None,
            })
            .collect())
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ConditionalReadme(PathBuf);

impl std::fmt::Display for ConditionalReadme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConditionalReadme({})", self.0.display())
    }
}

impl Key for ConditionalReadme {
    type Value = Result<usize, FsError>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let entries = ctx.read_dir(&self.0).await?;
        if entries.iter().any(|entry| entry.file_name == "README.md") {
            let contents = ctx.read_file(self.0.join("README.md")).await?;
            Ok(contents.len())
        } else {
            Ok(0)
        }
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[tokio::test(flavor = "current_thread")]
async fn changed_serial_branch_condition_does_not_recheck_skipped_old_file_dep() {
    let root = PathBuf::from("/workspace");
    let readme = root.join("README.md");
    let backend = Arc::new(TrackingFs::default());
    backend.set_dir_entries(root.clone(), ["README.md"]);
    backend.set_file(readme.clone(), b"hello");

    let engine = ComputeEngine::builder()
        .with_data(Arc::new(IndexedVfs::new(backend.clone())))
        .build();

    assert_eq!(
        engine
            .compute(&ConditionalReadme(root.clone()))
            .await
            .unwrap()
            .unwrap(),
        5
    );
    assert_eq!(backend.read_file_calls(), 1);

    backend.set_dir_entries(root.clone(), []);
    backend.remove_file(&readme);
    engine.invalidate_path(&readme).unwrap();

    assert_eq!(
        engine
            .compute(&ConditionalReadme(root))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(
        backend.read_file_calls(),
        1,
        "check-deps must not re-read a serial dep that the new execution skips"
    );
}
