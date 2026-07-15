//! A persistent, mtime-gated cache for [`GlobHash`] computations.
//!
//! [`GlobHash`] is authoritative but expensive: it reads and hashes the
//! contents of every matched file. The dispatcher-style caches on the other
//! hand only compare `(mtime, size)` pairs, which is cheap but unreliable
//! across e.g. git checkouts and CI cache restores. This module reconciles the
//! two strategies: **the recorded `(mtime, size)` pairs act as a pre-filter,
//! while the content hash remains the authority**.
//!
//! Every `(root, globs)` pair maps to a single JSON file inside a caller
//! supplied cache directory. An entry records the format version, the key
//! (root + globs), whether the root existed as a directory, the
//! `(mtime, size)` pair of every matched file, and the resulting content
//! hash. A lookup re-enumerates the glob matches and compares the observed
//! `(mtime, size)` pairs against the recorded ones:
//!
//! - if the matched file set and every `(mtime, size)` pair are unchanged, the
//!   recorded hash is returned *without reading any file contents*;
//! - otherwise (or when the entry is missing, unreadable, or of an unknown
//!   format version) the hash is recomputed with [`GlobHash::from_patterns`]
//!   and a fresh entry is written atomically (temp file + rename).
//!
//! Cache corruption is never an error: any entry that cannot be read is
//! treated as absent and silently recomputed. Failing to *write* an entry only
//! degrades future lookups to recomputation and is reported as a warning.
//! There is no locking; concurrent writers race benignly (last writer wins)
//! and a reader that observes a half-written entry treats it as corrupt.
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use rattler_digest::{Sha256, digest::Digest};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{GlobHash, GlobHashError, GlobSet};

/// The version of the on-disk entry format. Entries recorded with any other
/// version are treated as absent. Bump this whenever the shape or meaning of
/// [`CacheEntry`] changes.
const FORMAT_VERSION: u32 = 1;

/// Disambiguates the temp files of concurrent writers within one process; the
/// process id disambiguates across processes.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A persistent cache of [`GlobHash`] results, gated on `(mtime, size)`
/// pre-filters so that a hit never reads file contents.
///
/// See the [module documentation](self) for the on-disk format and the exact
/// lookup semantics. Instances are cheap to clone and multiple instances (or
/// processes) may safely share one cache directory.
#[derive(Debug, Clone)]
pub struct PersistentGlobHashCache {
    /// The directory in which cache entries are stored. Created on demand
    /// when the first entry is written.
    cache_dir: PathBuf,
}

/// An error that can occur while computing a hash through
/// [`PersistentGlobHashCache::get_or_compute`].
///
/// Note that cache IO problems are deliberately *not* represented here: a
/// broken cache entry degrades to recomputation and a failed cache write is
/// only logged, so the only fatal error source is the hash computation itself.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum PersistentGlobHashCacheError {
    #[error(transparent)]
    GlobHash(#[from] GlobHashError),
}

impl PersistentGlobHashCache {
    /// Constructs a new cache that stores its entries in `cache_dir`.
    ///
    /// The directory does not have to exist yet; it is created when the first
    /// entry is written.
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            cache_dir: cache_dir.into(),
        }
    }

    /// Returns the hash of the files matching `globs` relative to `root`,
    /// reusing a previously recorded result when no matched file changed.
    ///
    /// A recorded result is reused only if the set of files matching `globs`
    /// is identical to the recorded set *and* every file's `(mtime, size)`
    /// pair is unchanged; in that case no file contents are read. In every
    /// other case the hash is recomputed via [`GlobHash::from_patterns`] and
    /// persisted for the next lookup.
    ///
    /// Like [`GlobHashKey::new`](crate::GlobHashKey::new), a `root` that
    /// points at a file is evaluated relative to its containing directory.
    pub fn get_or_compute(
        &self,
        root: &Path,
        globs: &BTreeSet<String>,
    ) -> Result<GlobHash, PersistentGlobHashCacheError> {
        // Mirror `GlobHashKey::new`: when the root points at a file, the
        // globs are evaluated relative to its containing directory.
        let root = if root.is_file() {
            root.parent().unwrap_or(root)
        } else {
            root
        };

        let entry_path = self.entry_path(root, globs);

        // Enumerate the current matches once: the same snapshot serves as the
        // freshness probe and — because it is captured *before* any contents
        // are hashed — as the file states recorded on a miss. That ordering
        // guarantees a concurrent modification can only make the entry look
        // stale later, never mask a change.
        let current = snapshot_file_states(root, globs);

        // A recorded entry whose format version, key, root existence, matched
        // file set, and per-file (mtime, size) pairs are all unchanged is
        // authoritative; the recorded hash is returned without reading any
        // file contents.
        if let Some(current) = &current
            && let Some(entry) = read_entry(&entry_path)
            && entry.version == FORMAT_VERSION
            && entry.root == root.to_string_lossy()
            && &entry.globs == globs
            && entry.root_is_dir == current.root_is_dir
            && entry.files == current.files
            && let Some(hash) = rattler_digest::parse_digest_from_hex::<Sha256>(&entry.hash)
        {
            return Ok(GlobHash::from_hash(hash));
        }

        // Stale or absent: recompute the authoritative content hash.
        let glob_hash = GlobHash::from_patterns(root, globs.iter().map(String::as_str))?;

        match current {
            Some(snapshot) => {
                let entry = CacheEntry {
                    version: FORMAT_VERSION,
                    root: root.to_string_lossy().into_owned(),
                    globs: globs.clone(),
                    root_is_dir: snapshot.root_is_dir,
                    files: snapshot.files,
                    hash: format!("{:x}", glob_hash.hash),
                };
                // A failed write only costs the next lookup a recomputation.
                if let Err(err) = self.write_entry(&entry_path, &entry) {
                    tracing::warn!(
                        "failed to write glob hash cache entry {}: {err}",
                        entry_path.display()
                    );
                }
            }
            None => {
                // Without a consistent stat snapshot there is nothing safe to
                // record; the next lookup recomputes.
                tracing::debug!(
                    "not persisting the glob hash for {}: the matched files could not be enumerated",
                    root.display()
                );
            }
        }

        Ok(glob_hash)
    }

    /// Derives the entry file for a `(root, globs)` pair: a stable SHA-256
    /// over a `NUL`-delimited encoding of the key. The key is also recorded
    /// *inside* the entry and verified on read, so even a (pathological)
    /// filename collision degrades to a recomputation instead of a wrong hash.
    fn entry_path(&self, root: &Path, globs: &BTreeSet<String>) -> PathBuf {
        let mut hasher = Sha256::default();
        hasher.update(root.as_os_str().as_encoded_bytes());
        hasher.update([0u8]);
        for glob in globs {
            hasher.update(glob.as_bytes());
            hasher.update([0u8]);
        }
        let digest = hasher.finalize();
        self.cache_dir.join(format!("{digest:x}.json"))
    }

    /// Serializes `entry` and persists it atomically: the JSON is written to a
    /// uniquely named temp file in the cache directory and then renamed over
    /// the entry path, so concurrent readers never observe a partial write.
    fn write_entry(&self, entry_path: &Path, entry: &CacheEntry) -> io::Result<()> {
        fs_err::create_dir_all(&self.cache_dir)?;
        // Compact JSON: entries can hold thousands of file states and are
        // parsed on every lookup, so indentation is pure overhead.
        let json = serde_json::to_vec(entry).map_err(io::Error::other)?;

        let temp_file_name = format!(
            ".{}.{}-{}.tmp",
            entry_path
                .file_stem()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("entry"),
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let temp_path = self.cache_dir.join(temp_file_name);
        fs_err::write(&temp_path, json)?;
        if let Err(err) = fs_err::rename(&temp_path, entry_path) {
            // Clean up the orphan; the rename error is the interesting one.
            let _ = fs_err::remove_file(&temp_path);
            return Err(err);
        }
        Ok(())
    }
}

/// The on-disk representation of a single cache entry.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    /// The entry format version; entries with a different version are ignored.
    version: u32,
    /// The root directory the globs were evaluated against, lossily encoded
    /// so that non-UTF-8 roots still serialize. The entry *filename* is
    /// digested from the raw path bytes, so distinct roots keep distinct
    /// entries regardless; this field only guards filename collisions.
    root: String,
    /// The glob patterns that produced this entry.
    globs: BTreeSet<String>,
    /// Whether `root` existed as a directory when the entry was recorded.
    /// [`GlobHash`] hashes a missing root differently from an existing
    /// directory with zero matches, so the two states must not share a
    /// recorded hash.
    root_is_dir: bool,
    /// The `(mtime, size)` state of every matched file, keyed by the same
    /// normalized path representation that [`GlobHash`] hashes.
    files: BTreeMap<String, FileState>,
    /// The hex-encoded SHA-256 content hash of the matched files.
    hash: String,
}

/// A consistent observation of everything a freshness decision depends on:
/// whether the root is a directory at all, and the `(mtime, size)` state of
/// every file currently matching the globs.
#[derive(Debug, PartialEq, Eq)]
struct FileSetSnapshot {
    /// Whether the root exists and is a directory.
    root_is_dir: bool,
    /// The `(mtime, size)` state of every matched file.
    files: BTreeMap<String, FileState>,
}

/// The recorded stat-derived state of a single matched file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileState {
    /// Whole seconds of the modification time relative to the Unix epoch
    /// (negative for timestamps before the epoch).
    mtime_secs: i64,
    /// Nanosecond fraction of the modification time, offset from
    /// `mtime_secs` (always in `0..1_000_000_000`).
    mtime_nanos: u32,
    /// The file size in bytes.
    size: u64,
}

impl FileState {
    /// Captures the `(mtime, size)` state from file metadata. Fails on
    /// platforms or filesystems that cannot report a modification time.
    fn from_metadata(metadata: &std::fs::Metadata) -> io::Result<Self> {
        let (mtime_secs, mtime_nanos) = system_time_to_parts(metadata.modified()?);
        Ok(Self {
            mtime_secs,
            mtime_nanos,
            size: metadata.len(),
        })
    }
}

/// Enumerates the files currently matching `globs` under `root` and captures
/// every file's [`FileState`], keyed by the same normalized path
/// representation that [`GlobHash`] folds into the hash.
///
/// Returns `None` when the walk or a stat fails; callers treat that as
/// "cannot validate, cannot record" and fall back to recomputation.
fn snapshot_file_states(root: &Path, globs: &BTreeSet<String>) -> Option<FileSetSnapshot> {
    // Mirror `GlobHash::from_patterns`, which treats a root that is not a
    // directory as an empty file set (with a hash distinct from that of an
    // existing directory matching zero files).
    if !root.is_dir() {
        return Some(FileSetSnapshot {
            root_is_dir: false,
            files: BTreeMap::new(),
        });
    }

    let glob_set = GlobSet::create(globs.iter().map(String::as_str));
    let matches = match glob_set.collect_matching(root) {
        Ok(matches) => matches,
        Err(err) => {
            tracing::debug!(
                "failed to enumerate the glob matches under {}: {err}",
                root.display()
            );
            return None;
        }
    };

    let mut files = BTreeMap::new();
    for entry in matches {
        match entry
            .metadata()
            .and_then(|metadata| FileState::from_metadata(&metadata))
        {
            Ok(state) => {
                files.insert(normalized_key(root, entry.path()), state);
            }
            Err(err) => {
                tracing::debug!("failed to stat {}: {err}", entry.path().display());
                return None;
            }
        }
    }
    Some(FileSetSnapshot {
        root_is_dir: true,
        files,
    })
}

/// Normalizes a matched path the way [`GlobHash`] does before hashing it:
/// relative to `root` where possible and with `/` separators on every
/// platform, so entries are portable and stable.
///
/// The conversion is lossy for non-UTF-8 path bytes — exactly as lossy as
/// [`GlobHash`]'s own path handling. In the pathological case of two matched
/// paths collapsing to one key, the map records only one of them and changes
/// to the shadowed file may escape the `(mtime, size)` gate.
fn normalized_key(root: &Path, path: &Path) -> String {
    let relative_path = path.strip_prefix(root).unwrap_or(path);
    relative_path.to_string_lossy().replace('\\', "/")
}

/// Reads and parses a cache entry, returning `None` — never an error — when
/// the file is missing, unreadable, or does not parse: cache corruption
/// degrades to a recomputation.
fn read_entry(path: &Path) -> Option<CacheEntry> {
    let bytes = fs_err::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(entry) => Some(entry),
        Err(err) => {
            tracing::debug!(
                "ignoring unparseable glob hash cache entry {}: {err}",
                path.display()
            );
            None
        }
    }
}

/// Splits a [`SystemTime`] into `(whole seconds, nanosecond fraction)`
/// relative to the Unix epoch. Timestamps before the epoch map onto negative
/// seconds with a nanosecond fraction in `0..1_000_000_000`, so the
/// representation is unique and equality of the parts is equality of the
/// timestamps.
fn system_time_to_parts(time: SystemTime) -> (i64, u32) {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(since_epoch) => (since_epoch.as_secs() as i64, since_epoch.subsec_nanos()),
        Err(err) => {
            // `time` lies before the epoch.
            let before_epoch = err.duration();
            let nanos = before_epoch.subsec_nanos();
            if nanos == 0 {
                (-(before_epoch.as_secs() as i64), 0)
            } else {
                (-(before_epoch.as_secs() as i64) - 1, 1_000_000_000 - nanos)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
    };

    use rattler_digest::Sha256Hash;
    use tempfile::tempdir;

    use super::*;

    /// Builds a `BTreeSet` of glob patterns from string literals.
    fn globs(patterns: &[&str]) -> BTreeSet<String> {
        patterns.iter().map(|s| s.to_string()).collect()
    }

    /// The uncached, authoritative hash for comparison.
    fn direct_hash(root: &Path, globs: &BTreeSet<String>) -> Sha256Hash {
        GlobHash::from_patterns(root, globs.iter().map(String::as_str))
            .unwrap()
            .hash
    }

    /// Lists the persisted cache entry files (ignores temp files).
    fn entry_files(cache_dir: &Path) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = match fs_err::read_dir(cache_dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort();
        files
    }

    /// Returns the single persisted entry file, asserting there is exactly one.
    fn single_entry_file(cache_dir: &Path) -> PathBuf {
        let files = entry_files(cache_dir);
        assert_eq!(
            files.len(),
            1,
            "expected exactly one cache entry, got {files:?}"
        );
        files.into_iter().next().unwrap()
    }

    /// Rewrites `path` with `new_contents` — which must have the same byte
    /// length as the current contents — and restores the original modification
    /// time, making the change invisible to any `(mtime, size)` pre-filter.
    fn mutate_preserving_stat(path: &Path, new_contents: &str) {
        let metadata = fs_err::metadata(path).unwrap();
        assert_eq!(
            metadata.len(),
            new_contents.len() as u64,
            "mutate_preserving_stat requires equal-length contents"
        );
        let mtime = filetime::FileTime::from_last_modification_time(&metadata);
        fs_err::write(path, new_contents).unwrap();
        filetime::set_file_mtime(path, mtime).unwrap();
    }

    /// Moves the modification time of `path` forward by `seconds`.
    fn bump_mtime(path: &Path, seconds: i64) {
        let metadata = fs_err::metadata(path).unwrap();
        let mtime = filetime::FileTime::from_last_modification_time(&metadata);
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_unix_time(mtime.unix_seconds() + seconds, mtime.nanoseconds()),
        )
        .unwrap();
    }

    /// Seam 1: a miss on an empty cache directory computes the hash, persists
    /// an entry, and returns the same hash as a direct computation.
    #[test]
    fn miss_computes_writes_entry_and_returns_direct_hash() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        fs_err::write(root.path().join("a.txt"), "hello\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);

        assert!(entry_files(cache_dir.path()).is_empty());
        let computed = cache.get_or_compute(root.path(), &globs).unwrap();

        assert_eq!(computed.hash, direct_hash(root.path(), &globs));
        // A fresh entry must have been persisted.
        single_entry_file(cache_dir.path());
    }

    /// Seam 2 — the defining test of the mtime gate: when a file's contents
    /// change but its `(mtime, size)` pair does not, the cache must return the
    /// *recorded* hash, proving it did not read any file contents.
    #[test]
    fn hit_returns_recorded_hash_without_reading_contents() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs_err::write(&file, "aaaa\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        // Same byte length, same mtime — only the contents differ.
        mutate_preserving_stat(&file, "bbbb\n");

        // Sanity check: an authoritative recomputation *would* see the change.
        assert_ne!(direct_hash(root.path(), &globs), old.hash);

        // But the cache must serve the recorded hash without reading contents.
        let hit = cache.get_or_compute(root.path(), &globs).unwrap();
        assert_eq!(hit.hash, old.hash);
    }

    /// Seam 3: a bumped mtime with unchanged contents forces a recomputation
    /// (the content hash stays the same) and rewrites the entry with the new
    /// file state.
    #[test]
    fn mtime_change_with_same_content_recomputes_and_rewrites_entry() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs_err::write(&file, "hello\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        let entry_path = single_entry_file(cache_dir.path());
        let entry_before = fs_err::read(&entry_path).unwrap();

        bump_mtime(&file, 10);
        let recomputed = cache.get_or_compute(root.path(), &globs).unwrap();

        // Identical contents hash identically...
        assert_eq!(recomputed.hash, old.hash);
        // ...but the entry must have been rewritten with the new mtime.
        let entry_after = fs_err::read(&entry_path).unwrap();
        assert_ne!(
            entry_before, entry_after,
            "the entry should record the new mtime"
        );
    }

    /// Seam 4: changed contents (with a different size) yield a fresh hash.
    #[test]
    fn content_and_size_change_recomputes() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs_err::write(&file, "hello\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        fs_err::write(&file, "entirely different contents\n").unwrap();
        let recomputed = cache.get_or_compute(root.path(), &globs).unwrap();

        assert_ne!(recomputed.hash, old.hash);
        assert_eq!(recomputed.hash, direct_hash(root.path(), &globs));
    }

    /// Seam 5: a new file that matches the globs makes the entry stale even if
    /// all previously recorded files are untouched.
    #[test]
    fn added_matching_file_invalidates() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        fs_err::write(root.path().join("a.txt"), "aaa\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        fs_err::write(root.path().join("b.txt"), "bbb\n").unwrap();
        let recomputed = cache.get_or_compute(root.path(), &globs).unwrap();

        assert_ne!(recomputed.hash, old.hash);
        assert_eq!(recomputed.hash, direct_hash(root.path(), &globs));
    }

    /// Seam 6: a deleted recorded file makes the entry stale.
    #[test]
    fn removed_recorded_file_invalidates() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        fs_err::write(root.path().join("a.txt"), "aaa\n").unwrap();
        fs_err::write(root.path().join("b.txt"), "bbb\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        fs_err::remove_file(root.path().join("b.txt")).unwrap();
        let recomputed = cache.get_or_compute(root.path(), &globs).unwrap();

        assert_ne!(recomputed.hash, old.hash);
        assert_eq!(recomputed.hash, direct_hash(root.path(), &globs));
    }

    /// Seam 7: a corrupt entry is treated as absent — silently recomputed and
    /// replaced, never surfaced as an error.
    #[test]
    fn corrupt_entry_is_silently_recomputed() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs_err::write(&file, "aaaa\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        let entry_path = single_entry_file(cache_dir.path());
        fs_err::write(&entry_path, "definitely { not json").unwrap();

        // Contents change while (mtime, size) stays fixed: only an (incorrect)
        // cache hit could return the old hash, so a fresh hash proves the
        // corrupt entry was ignored and the value recomputed.
        mutate_preserving_stat(&file, "bbbb\n");
        let recomputed = cache.get_or_compute(root.path(), &globs).unwrap();

        assert_ne!(recomputed.hash, old.hash);
        assert_eq!(recomputed.hash, direct_hash(root.path(), &globs));

        // The corrupt entry must have been replaced by a valid one.
        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs_err::read(&entry_path).unwrap()).unwrap();
        assert!(rewritten.get("version").is_some());
    }

    /// Seam 8: an entry with an unknown format version is treated as absent.
    #[test]
    fn unknown_format_version_is_treated_as_absent() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs_err::write(&file, "aaaa\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        // Rewrite the (otherwise pristine) entry with a version from the future.
        let entry_path = single_entry_file(cache_dir.path());
        let mut entry: serde_json::Value =
            serde_json::from_slice(&fs_err::read(&entry_path).unwrap()).unwrap();
        entry["version"] = serde_json::Value::from(u32::MAX);
        fs_err::write(&entry_path, serde_json::to_vec(&entry).unwrap()).unwrap();

        // Same detector as in the corruption test: a stat-invisible content
        // change distinguishes a (forbidden) hit from the required recompute.
        mutate_preserving_stat(&file, "bbbb\n");
        let recomputed = cache.get_or_compute(root.path(), &globs).unwrap();

        assert_ne!(recomputed.hash, old.hash);
        assert_eq!(recomputed.hash, direct_hash(root.path(), &globs));
    }

    /// Seam 9a: different `(root, globs)` pairs land in distinct entry files,
    /// and repeated lookups do not mint additional entries.
    #[test]
    fn distinct_keys_use_distinct_entries() {
        let root_a = tempdir().unwrap();
        let root_b = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        fs_err::write(root_a.path().join("a.txt"), "root a\n").unwrap();
        fs_err::write(root_b.path().join("a.txt"), "root b\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs_flat = globs(&["*.txt"]);
        let globs_recursive = globs(&["**/*.txt"]);

        let hash_a = cache.get_or_compute(root_a.path(), &globs_flat).unwrap();
        let hash_b = cache.get_or_compute(root_b.path(), &globs_flat).unwrap();
        let hash_a2 = cache
            .get_or_compute(root_a.path(), &globs_recursive)
            .unwrap();

        // Three keys → three entries; the differing roots also hash different
        // contents.
        assert_eq!(entry_files(cache_dir.path()).len(), 3);
        assert_ne!(hash_a.hash, hash_b.hash);

        // Every key resolves to its own authoritative hash.
        assert_eq!(hash_a.hash, direct_hash(root_a.path(), &globs_flat));
        assert_eq!(hash_b.hash, direct_hash(root_b.path(), &globs_flat));
        assert_eq!(hash_a2.hash, direct_hash(root_a.path(), &globs_recursive));

        // Looking the keys up again must reuse the same three entry files.
        cache.get_or_compute(root_a.path(), &globs_flat).unwrap();
        cache.get_or_compute(root_b.path(), &globs_flat).unwrap();
        cache
            .get_or_compute(root_a.path(), &globs_recursive)
            .unwrap();
        assert_eq!(entry_files(cache_dir.path()).len(), 3);
    }

    /// Seam 9b: the entry written by one cache instance is found by a fresh
    /// instance pointed at the same directory (the key derivation is stable).
    #[test]
    fn same_key_is_stable_across_cache_instances() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs_err::write(&file, "aaaa\n").unwrap();

        let globs = globs(&["*.txt"]);
        let old = PersistentGlobHashCache::new(cache_dir.path())
            .get_or_compute(root.path(), &globs)
            .unwrap();

        // A stat-invisible content change: only a genuine cross-instance cache
        // hit can return the old hash.
        mutate_preserving_stat(&file, "bbbb\n");

        let second_instance = PersistentGlobHashCache::new(cache_dir.path());
        let hit = second_instance.get_or_compute(root.path(), &globs).unwrap();
        assert_eq!(hit.hash, old.hash);
        assert_eq!(entry_files(cache_dir.path()).len(), 1);
    }

    /// A root that springs into existence must invalidate the entry: the
    /// authoritative hash of a missing root differs from the hash of an
    /// existing directory with zero matches, so "no matched files" alone is
    /// not sufficient evidence of freshness.
    #[test]
    fn root_created_after_recording_invalidates() {
        let base = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let root = base.path().join("src");

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);

        // Record while the root does not exist.
        let old = cache.get_or_compute(&root, &globs).unwrap();
        assert_eq!(old.hash, direct_hash(&root, &globs));

        // The root now exists but still matches zero files.
        fs_err::create_dir(&root).unwrap();
        let recomputed = cache.get_or_compute(&root, &globs).unwrap();

        assert_ne!(
            recomputed.hash, old.hash,
            "an existing-but-empty root hashes differently from a missing root"
        );
        assert_eq!(recomputed.hash, direct_hash(&root, &globs));
    }

    /// A root that points at a file is evaluated relative to its containing
    /// directory, mirroring `GlobHashKey::new`, and shares that directory's
    /// cache entry.
    #[test]
    fn file_root_is_normalized_to_its_parent_directory() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let file = root.path().join("a.txt");
        fs_err::write(&file, "aaaa\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);

        let via_dir = cache.get_or_compute(root.path(), &globs).unwrap();
        let via_file = cache.get_or_compute(&file, &globs).unwrap();

        assert_eq!(via_file.hash, via_dir.hash);
        assert_eq!(via_file.hash, direct_hash(root.path(), &globs));
        // Both spellings of the key must share one entry.
        assert_eq!(entry_files(cache_dir.path()).len(), 1);
    }

    /// Files that do not match the globs are neither recorded nor consulted:
    /// changing one leaves the entry fresh.
    #[test]
    fn unmatched_files_do_not_affect_freshness() {
        let root = tempdir().unwrap();
        let cache_dir = tempdir().unwrap();
        let matched = root.path().join("a.txt");
        let unmatched = root.path().join("b.log");
        fs_err::write(&matched, "aaaa\n").unwrap();
        fs_err::write(&unmatched, "bbbb\n").unwrap();

        let cache = PersistentGlobHashCache::new(cache_dir.path());
        let globs = globs(&["*.txt"]);
        let old = cache.get_or_compute(root.path(), &globs).unwrap();

        // Change the unmatched file in every observable way...
        fs_err::write(&unmatched, "a change of both size and mtime\n").unwrap();
        // ...and make the matched file's change stat-invisible, so that only
        // a cache hit can return the old hash.
        mutate_preserving_stat(&matched, "cccc\n");

        let hit = cache.get_or_compute(root.path(), &globs).unwrap();
        assert_eq!(hit.hash, old.hash);
    }
}
