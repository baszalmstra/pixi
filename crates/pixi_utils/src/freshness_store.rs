//! A reusable, cross-process-safe store for typed JSON marker files.
//!
//! [`FreshnessStore`] persists small "freshness markers" (fingerprints,
//! cached metadata, activation results, ...) as one JSON file per key under
//! a caller-supplied root directory. It lifts the storage discipline of the
//! command dispatcher's `MetadataCache` into a reusable, non-trait form:
//! fd-locked reads and writes, optimistic concurrency, and corruption
//! tolerance.
//!
//! # Concurrency contract
//!
//! * [`FreshnessStore::read`] takes a *shared* file lock: any number of
//!   readers may proceed concurrently, but a reader blocks while a writer
//!   holds the exclusive lock. The lock is released before the result is
//!   returned, so the returned entry may already be stale; the version
//!   counter exists to detect exactly that.
//! * [`FreshnessStore::try_write`] takes an *exclusive* file lock and only
//!   writes if the entry currently on disk still has the version the caller
//!   observed (`expected_version`; use `0` when [`FreshnessStore::read`]
//!   returned `None`). If another process or task wrote in between, the
//!   version no longer matches and [`WriteResult::Conflict`] returns the
//!   entry that won, so the caller can re-read, reconcile, and retry with
//!   the current version — or simply accept the winner's result.
//!
//! Anyone writing to the same root directory (other threads, tasks, or
//! processes on the same machine) can conflict; the fd lock serializes the
//! writers and the version check decides who wins.
//!
//! # Corruption tolerance
//!
//! Markers are advisory caches: losing one must never break pixi, only cost
//! a recomputation. A missing file, unparseable contents (torn write,
//! garbage), or an entry written by an incompatible schema therefore reads
//! as `Ok(None)` — never as an error — and the next successful
//! [`FreshnessStore::try_write`] repairs the file.
//!
//! # Versions and revisions
//!
//! Three independent notions of "version" are involved:
//!
//! * [`SCHEMA_VERSION`] — the on-disk *envelope format*. Bumped when the
//!   store's own file layout changes incompatibly; entries with a different
//!   schema version read as `None`.
//! * [`VersionedEntry::version`] — the optimistic concurrency counter,
//!   managed entirely by the store. It counts *how many times* the file was
//!   written and is used to detect concurrent writers.
//! * [`VersionedEntry::revision`] — an opaque content identity, carried by
//!   the entry. It changes only when the caller stores new content, letting
//!   downstream caches detect *what content* they derived their data from
//!   without comparing full payloads.

use std::path::{Path, PathBuf};

use async_fd_lock::{LockRead, LockWrite};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// The version of the on-disk envelope format written by this build of the
/// store.
///
/// This is the *schema* version: it describes how [`VersionedEntry`] itself
/// is laid out in the JSON file, independent of what the payload contains
/// and independent of the optimistic [`VersionedEntry::version`] counter.
/// Bump it when the envelope changes in a backwards-incompatible way; files
/// written with any other schema version are treated as absent (see the
/// module documentation).
pub const SCHEMA_VERSION: u64 = 1;

/// A store of typed JSON marker files under a caller-supplied root
/// directory.
///
/// Every key maps to a single `<root>/<key>.json` file holding one
/// [`VersionedEntry`]. Keys may contain `/` to shard entries into
/// subdirectories and may contain dots; see [`FreshnessStore::entry_path`].
///
/// See the [module documentation](self) for the concurrency contract and
/// the corruption-tolerance rules.
#[derive(Debug, Clone)]
pub struct FreshnessStore {
    root: PathBuf,
}

impl FreshnessStore {
    /// Creates a store rooted at `root`.
    ///
    /// The directory does not have to exist; it is created lazily on the
    /// first write. A missing root simply reads as an empty store.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the root directory under which the marker files are stored.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the path of the file that stores the entry for `key`.
    ///
    /// The key is used verbatim as the file stem with `.json` appended via
    /// string concatenation instead of [`Path::with_extension`]: keys may
    /// contain dots (e.g. derived from a package name like `my.package`)
    /// and `with_extension` would truncate everything after the last dot.
    /// A `/` in the key creates a subdirectory, which callers can use to
    /// shard entries.
    ///
    /// Keys are joined onto the root without sanitization, so they must be
    /// relative paths that stay inside the root: a key containing `..` walks
    /// out of the store and an absolute key replaces the root wholesale (a
    /// property of [`Path::join`]). Keys are expected to be generated by the
    /// caller (constants, hashes, package names) — never from untrusted
    /// input.
    pub fn entry_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{key}.json"))
    }

    /// Reads the entry stored for `key`, if any.
    ///
    /// The file is read under a *shared* fd lock, so concurrent readers do
    /// not block each other but a concurrent [`Self::try_write`] does. The
    /// lock is released before this method returns: the returned entry is a
    /// snapshot and may be overwritten by another writer at any moment.
    /// Pass [`VersionedEntry::version`] of the returned entry (or `0` if
    /// this method returned `None`) as `expected_version` to
    /// [`Self::try_write`] to detect exactly that.
    ///
    /// Returns `Ok(None)` — never an error — when the file is missing,
    /// cannot be parsed (torn write, garbage), or was written with an
    /// incompatible [`SCHEMA_VERSION`]. Markers are advisory: a damaged one
    /// must degrade to "recompute", not break the caller.
    pub async fn read<T: DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<VersionedEntry<T>>, FreshnessStoreError> {
        let path = self.entry_path(key);

        // Try to open the marker file (it may not exist yet).
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(FreshnessStoreError::io("opening marker file", path, err));
            }
        };

        let mut locked_file = file
            .lock_read()
            .await
            .map_err(|err| FreshnessStoreError::io("locking marker file", &path, err.error))?;

        // Read the contents while holding the shared lock, then release it
        // immediately; parsing does not need the lock.
        let mut contents = String::new();
        locked_file
            .read_to_string(&mut contents)
            .await
            .map_err(|err| FreshnessStoreError::io("reading marker file", &path, err))?;
        drop(locked_file);

        Ok(parse_entry(&contents, &path))
    }

    /// Writes `entry` for `key` if the entry on disk still has the version
    /// the caller observed, creating missing directories as needed.
    ///
    /// `expected_version` is the [`VersionedEntry::version`] the caller saw
    /// when it last called [`Self::read`], or `0` if that read returned
    /// `None`. The check and the write happen under a single *exclusive* fd
    /// lock:
    ///
    /// * If the on-disk entry still matches `expected_version` (or there is
    ///   no valid on-disk entry), the entry is written with its version set
    ///   to `expected_version + 1` and [`WriteResult::Written`] is returned.
    /// * If another process or task wrote in between, the stored version no
    ///   longer matches and [`WriteResult::Conflict`] returns the entry
    ///   that is currently on disk without modifying it. The caller can
    ///   reconcile and retry with the conflicting entry's version, or
    ///   accept the winner's result.
    ///
    /// A file that is missing, unparseable, or written with an incompatible
    /// [`SCHEMA_VERSION`] never conflicts: it is treated as absent, exactly
    /// as [`Self::read`] reports it, so a corrupted marker is repaired by
    /// the next successful write.
    ///
    /// The version counter of the passed `entry` is overwritten by the
    /// store; its [`Revision`] is persisted untouched. Re-submitting an
    /// entry returned by [`Self::read`] therefore keeps its revision, while
    /// [`VersionedEntry::new`] mints a fresh revision for new content.
    pub async fn try_write<T: Serialize + DeserializeOwned>(
        &self,
        key: &str,
        entry: VersionedEntry<T>,
        expected_version: u64,
    ) -> Result<WriteResult<T>, FreshnessStoreError> {
        let path = self.entry_path(key);

        // Stamp the envelope before doing any I/O: the store owns both the
        // schema version and the optimistic version counter.
        let mut entry = entry;
        entry.schema_version = SCHEMA_VERSION;
        entry.version = expected_version + 1;
        let bytes = serde_json::to_vec(&entry).map_err(|err| FreshnessStoreError::Serialize {
            path: path.clone(),
            source: err,
        })?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|err| FreshnessStoreError::io("creating marker directory", parent, err))?;
        }

        // Open or create the marker file without truncating it: the current
        // contents are still needed for the version check below.
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .await
            .map_err(|err| FreshnessStoreError::io("opening marker file", &path, err))?;

        let mut locked_file = file
            .lock_write()
            .await
            .map_err(|err| FreshnessStoreError::io("locking marker file", &path, err.error))?;

        // Re-read under the exclusive lock to detect concurrent writers.
        let mut current_contents = String::new();
        locked_file
            .read_to_string(&mut current_contents)
            .await
            .map_err(|err| FreshnessStoreError::io("reading marker file", &path, err))?;

        // If a valid entry exists and was written since the caller's read,
        // report the conflict. An empty, unparseable, or schema-incompatible
        // file is treated as absent — consistent with `read` — so it is
        // simply overwritten.
        if !current_contents.is_empty()
            && let Some(current_entry) = parse_entry::<T>(&current_contents, &path)
            && current_entry.version != expected_version
        {
            drop(locked_file);
            return Ok(WriteResult::Conflict(current_entry));
        }

        // The expected version still holds; write the new entry in place.
        locked_file
            .rewind()
            .await
            .map_err(|err| FreshnessStoreError::io("seeking in marker file", &path, err))?;
        locked_file
            .write_all(&bytes)
            .await
            .map_err(|err| FreshnessStoreError::io("writing marker file", &path, err))?;
        locked_file
            .inner_mut()
            .set_len(bytes.len() as u64)
            .await
            .map_err(|err| FreshnessStoreError::io("truncating marker file", &path, err))?;
        locked_file
            .flush()
            .await
            .map_err(|err| FreshnessStoreError::io("flushing marker file", &path, err))?;
        drop(locked_file);

        Ok(WriteResult::Written)
    }
}

/// Parses marker file contents, applying the corruption-tolerance rules:
/// unparseable contents or an incompatible schema version yield `None`.
fn parse_entry<T: DeserializeOwned>(contents: &str, path: &Path) -> Option<VersionedEntry<T>> {
    let entry: VersionedEntry<T> = match serde_json::from_str(contents) {
        Ok(entry) => entry,
        Err(err) => {
            tracing::debug!("failed to parse marker file '{}': {err}", path.display());
            return None;
        }
    };

    if entry.schema_version != SCHEMA_VERSION {
        tracing::debug!(
            "ignoring marker file '{}' with unsupported schema version {} (expected {SCHEMA_VERSION})",
            path.display(),
            entry.schema_version,
        );
        return None;
    }

    Some(entry)
}

/// A typed payload together with the bookkeeping the store persists next to
/// it: the envelope schema version, the optimistic version counter, and the
/// content [`Revision`].
///
/// Create new entries with [`VersionedEntry::new`]; entries read back from
/// the store come out of [`FreshnessStore::read`]. The version counter is
/// managed exclusively by the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedEntry<T> {
    /// The envelope format version; see [`SCHEMA_VERSION`].
    schema_version: u64,
    /// The optimistic concurrency counter, bumped by the store on every
    /// successful write.
    version: u64,
    /// The identity of the payload content, carried along unmodified.
    revision: Revision,
    /// The caller's typed payload.
    payload: T,
}

impl<T> VersionedEntry<T> {
    /// Wraps `payload` in a new entry with a freshly minted [`Revision`].
    ///
    /// The entry starts at version `0`, meaning "never written":
    /// [`FreshnessStore::try_write`] assigns the real version when the
    /// entry is persisted.
    pub fn new(payload: T) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            version: 0,
            revision: Revision::new(),
            payload,
        }
    }

    /// Returns the typed payload.
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Consumes the entry and returns the typed payload.
    pub fn into_payload(self) -> T {
        self.payload
    }

    /// Returns the optimistic version counter of this entry.
    ///
    /// For an entry returned by [`FreshnessStore::read`] this is the value
    /// to pass as `expected_version` to the next
    /// [`FreshnessStore::try_write`]. Fresh entries report `0`.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Returns the revision identifying the content of this entry.
    pub fn revision(&self) -> &Revision {
        &self.revision
    }
}

/// The outcome of a [`FreshnessStore::try_write`] call.
#[derive(Debug)]
#[must_use = "a Conflict means the entry on disk is not what this writer intended"]
pub enum WriteResult<T> {
    /// The entry was written to disk; it now has version
    /// `expected_version + 1`.
    Written,
    /// Another writer updated the entry between the caller's read and this
    /// write. Contains the entry currently on disk, whose
    /// [`VersionedEntry::version`] can seed a retry.
    Conflict(VersionedEntry<T>),
}

/// An opaque identifier for the *content* of a stored entry.
///
/// A revision changes only when new content is stored (i.e. when a caller
/// builds a fresh entry via [`VersionedEntry::new`]); re-writing an entry
/// read from the store keeps its revision. Downstream data derived from an
/// entry can record the revision it was derived from and later compare it
/// against the store to detect staleness, without comparing full payloads.
///
/// This is intentionally a random identity (a [`nanoid`]) rather than a
/// content hash: payloads do not need to be hashable, and equality of
/// revisions is only ever compared against revisions from the same store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Revision(String);

impl Revision {
    /// Generates a new unique revision.
    pub fn new() -> Self {
        Self(nanoid::nanoid!())
    }
}

impl Default for Revision {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Errors returned by [`FreshnessStore`] operations.
///
/// Note that damaged *contents* are not an error (see the module
/// documentation): only real I/O failures and unserializable payloads are
/// reported.
#[derive(Debug, thiserror::Error)]
pub enum FreshnessStoreError {
    /// An I/O error occurred while reading or writing a marker file.
    #[error("an IO error occurred while {operation} '{}'", path.display())]
    Io {
        /// The operation that failed, e.g. "opening marker file".
        operation: String,
        /// The path that was being accessed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The entry could not be serialized to JSON. This indicates a payload
    /// type that cannot be represented in JSON (e.g. a map with non-string
    /// keys) and is a bug in the caller rather than a disk problem.
    #[error("failed to serialize the marker entry for '{}'", path.display())]
    Serialize {
        /// The path the entry was destined for.
        path: PathBuf,
        /// The underlying serialization error.
        #[source]
        source: serde_json::Error,
    },
}

impl FreshnessStoreError {
    /// Wraps an I/O error with the operation that failed and the path that
    /// was being accessed.
    fn io(operation: &str, path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            operation: operation.to_string(),
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

    /// A small typed payload standing in for real marker contents.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Marker {
        name: String,
        count: u64,
    }

    fn marker(name: &str, count: u64) -> Marker {
        Marker {
            name: name.to_string(),
            count,
        }
    }

    /// Creates a store rooted in a directory that does not exist yet.
    fn store_in(tmp: &TempDir) -> FreshnessStore {
        FreshnessStore::new(tmp.path().join("store"))
    }

    #[tokio::test]
    async fn read_on_empty_store_returns_none() {
        let tmp = TempDir::new().unwrap();

        // A root that exists but contains no entries.
        let existing_root = FreshnessStore::new(tmp.path());
        assert!(
            existing_root
                .read::<Marker>("missing-key")
                .await
                .unwrap()
                .is_none()
        );

        // A root that does not exist at all.
        let missing_root = store_in(&tmp);
        assert!(
            missing_root
                .read::<Marker>("missing-key")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn write_then_read_roundtrip_preserves_payload_and_increments_version() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        let entry = VersionedEntry::new(marker("a", 1));
        assert_eq!(entry.version(), 0, "a fresh entry has never been written");
        let revision = entry.revision().clone();

        let result = store.try_write("key", entry, 0).await.unwrap();
        assert!(matches!(result, WriteResult::Written));

        let read = store.read::<Marker>("key").await.unwrap().unwrap();
        assert_eq!(read.payload(), &marker("a", 1));
        assert_eq!(read.version(), 1, "the first write stores version 1");
        assert_eq!(
            read.revision(),
            &revision,
            "the revision is persisted as the caller supplied it"
        );

        // A subsequent write on top of the read entry bumps the version again.
        let result = store
            .try_write("key", VersionedEntry::new(marker("b", 2)), read.version())
            .await
            .unwrap();
        assert!(matches!(result, WriteResult::Written));

        let read = store.read::<Marker>("key").await.unwrap().unwrap();
        assert_eq!(read.payload(), &marker("b", 2));
        assert_eq!(read.version(), 2);
        assert_ne!(
            read.revision(),
            &revision,
            "new content carries a new revision"
        );
    }

    #[tokio::test]
    async fn stale_expected_version_conflicts_and_leaves_the_file_unchanged() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        let result = store
            .try_write("key", VersionedEntry::new(marker("current", 1)), 0)
            .await
            .unwrap();
        assert!(matches!(result, WriteResult::Written));

        // The entry is now at version 1, so an expected version of 0 is stale.
        let result = store
            .try_write("key", VersionedEntry::new(marker("stale", 2)), 0)
            .await
            .unwrap();
        let WriteResult::Conflict(current) = result else {
            panic!("expected a conflict, got {result:?}");
        };
        assert_eq!(current.payload(), &marker("current", 1));
        assert_eq!(current.version(), 1);

        // The on-disk entry is unchanged.
        let read = store.read::<Marker>("key").await.unwrap().unwrap();
        assert_eq!(read.payload(), &marker("current", 1));
        assert_eq!(read.version(), 1);
    }

    #[tokio::test]
    async fn second_writer_with_the_same_expected_version_conflicts() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        // Two writers that both observed "no entry yet" (expected version 0).
        let first = store
            .try_write("key", VersionedEntry::new(marker("first", 1)), 0)
            .await
            .unwrap();
        assert!(matches!(first, WriteResult::Written));

        let second = store
            .try_write("key", VersionedEntry::new(marker("second", 2)), 0)
            .await
            .unwrap();
        let WriteResult::Conflict(current) = second else {
            panic!("expected a conflict, got {second:?}");
        };
        assert_eq!(
            current.payload(),
            &marker("first", 1),
            "the conflict carries the entry the first writer stored"
        );
    }

    #[tokio::test]
    async fn corrupted_file_reads_as_none_and_is_recovered_by_the_next_write() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        let path = store.entry_path("key");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"{ definitely not json")
            .await
            .unwrap();

        assert!(
            store.read::<Marker>("key").await.unwrap().is_none(),
            "garbage contents degrade to a cache miss, not an error"
        );

        // A write with expected version 0 (which is what the failed read
        // implies) repairs the file.
        let result = store
            .try_write("key", VersionedEntry::new(marker("fresh", 1)), 0)
            .await
            .unwrap();
        assert!(matches!(result, WriteResult::Written));

        let read = store.read::<Marker>("key").await.unwrap().unwrap();
        assert_eq!(read.payload(), &marker("fresh", 1));
        assert_eq!(read.version(), 1);
    }

    #[tokio::test]
    async fn overwriting_with_shorter_content_truncates_the_file() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        // Store an entry whose serialized form is much longer than its
        // replacement, so the rewrite must shrink the file.
        let long = marker(&"long".repeat(100), 1);
        let result = store
            .try_write("key", VersionedEntry::new(long), 0)
            .await
            .unwrap();
        assert!(matches!(result, WriteResult::Written));
        let long_len = tokio::fs::metadata(store.entry_path("key"))
            .await
            .unwrap()
            .len();

        let short = marker("s", 2);
        let result = store
            .try_write("key", VersionedEntry::new(short.clone()), 1)
            .await
            .unwrap();
        assert!(matches!(result, WriteResult::Written));

        // The file must contain exactly the new entry: no trailing bytes of
        // the longer previous contents may survive the in-place rewrite.
        let raw = tokio::fs::read_to_string(store.entry_path("key"))
            .await
            .unwrap();
        assert!(
            (raw.len() as u64) < long_len,
            "the file must have shrunk (was {long_len}, is {})",
            raw.len()
        );
        serde_json::from_str::<serde_json::Value>(&raw)
            .expect("the shrunken file is valid JSON with no trailing garbage");

        let read = store.read::<Marker>("key").await.unwrap().unwrap();
        assert_eq!(read.payload(), &short);
        assert_eq!(read.version(), 2);
    }

    #[tokio::test]
    async fn unknown_schema_version_reads_as_none() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        // Persist a valid entry, then rewrite it in place with a future
        // schema version, as a newer pixi would leave behind.
        let result = store
            .try_write("key", VersionedEntry::new(marker("future", 1)), 0)
            .await
            .unwrap();
        assert!(matches!(result, WriteResult::Written));

        let path = store.entry_path("key");
        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&contents).unwrap();
        value["schema_version"] = serde_json::json!(SCHEMA_VERSION + 1);
        tokio::fs::write(&path, serde_json::to_vec(&value).unwrap())
            .await
            .unwrap();

        assert!(
            store.read::<Marker>("key").await.unwrap().is_none(),
            "an entry from an incompatible schema is treated as absent"
        );

        // try_write treats the incompatible entry as absent as well, so the
        // file can be reclaimed without a conflict.
        let result = store
            .try_write("key", VersionedEntry::new(marker("replacement", 2)), 0)
            .await
            .unwrap();
        assert!(matches!(result, WriteResult::Written));
        let read = store.read::<Marker>("key").await.unwrap().unwrap();
        assert_eq!(read.payload(), &marker("replacement", 2));
        assert_eq!(read.version(), 1);
    }

    #[test]
    fn entry_path_keeps_dots_and_shards_on_slashes() {
        let store = FreshnessStore::new("/tmp/store");

        // A key with dots (e.g. derived from a package name like
        // "my.package") must NOT have the part after the last dot replaced,
        // the way `Path::with_extension` would.
        assert_eq!(
            store.entry_path("source-dir/my.package-osx-arm64-HASH"),
            PathBuf::from("/tmp/store/source-dir/my.package-osx-arm64-HASH.json")
        );

        // A key without dots also maps straight to a `.json` file.
        assert_eq!(
            store.entry_path("source-dir/my-package-osx-arm64-HASH"),
            PathBuf::from("/tmp/store/source-dir/my-package-osx-arm64-HASH.json")
        );
    }

    #[tokio::test]
    async fn dotted_and_slashed_keys_store_distinct_entries() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        for (key, payload) in [
            ("shard/my.package", marker("dotted", 1)),
            ("shard/my/package", marker("slashed", 2)),
            ("my.package", marker("plain", 3)),
        ] {
            let result = store
                .try_write(key, VersionedEntry::new(payload), 0)
                .await
                .unwrap();
            assert!(matches!(result, WriteResult::Written));
        }

        for (key, payload) in [
            ("shard/my.package", marker("dotted", 1)),
            ("shard/my/package", marker("slashed", 2)),
            ("my.package", marker("plain", 3)),
        ] {
            let read = store.read::<Marker>(key).await.unwrap().unwrap();
            assert_eq!(read.payload(), &payload, "key {key:?} kept its payload");
            assert!(
                store.entry_path(key).is_file(),
                "key {key:?} maps to a real file"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn racing_writers_exactly_one_written() {
        let tmp = TempDir::new().unwrap();
        let store = store_in(&tmp);

        for round in 0..8 {
            let key = format!("key-{round}");

            let task_a = tokio::spawn({
                let store = store.clone();
                let key = key.clone();
                async move {
                    store
                        .try_write(&key, VersionedEntry::new(marker("a", 1)), 0)
                        .await
                        .unwrap()
                }
            });
            let task_b = tokio::spawn({
                let store = store.clone();
                let key = key.clone();
                async move {
                    store
                        .try_write(&key, VersionedEntry::new(marker("b", 2)), 0)
                        .await
                        .unwrap()
                }
            });

            let results = [task_a.await.unwrap(), task_b.await.unwrap()];

            // The fd lock serializes the two writers and the version check
            // makes the loser observe the winner's entry. Which task wins is
            // timing-dependent, so only assert the invariant.
            let written = results
                .iter()
                .filter(|result| matches!(result, WriteResult::Written))
                .count();
            let conflicted = results
                .iter()
                .filter(|result| matches!(result, WriteResult::Conflict(_)))
                .count();
            assert_eq!(
                (written, conflicted),
                (1, 1),
                "exactly one racing writer must win (round {round}): {results:?}"
            );

            // The loser observed exactly what was persisted.
            let read = store.read::<Marker>(&key).await.unwrap().unwrap();
            assert_eq!(read.version(), 1);
            let observed = results
                .iter()
                .find_map(|result| match result {
                    WriteResult::Conflict(current) => Some(current),
                    WriteResult::Written => None,
                })
                .unwrap();
            assert_eq!(observed.payload(), read.payload());
            assert_eq!(observed.version(), read.version());
        }
    }
}
