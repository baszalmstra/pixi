# Research: How Bazel Monitors Filesystem Changes

## Summary

Bazel uses a **two-tier filesystem change detection architecture**. The primary tier is the **`DiffAwareness` interface** — platform-native filesystem watchers (FSEvents on macOS, inotify on Linux via WatchService) that provide precise lists of changed files between invocations. When `DiffAwareness` is unavailable or broken, Bazel falls back to the **`FilesystemValueChecker`** — a full graph scan that compares current filesystem state against cached `FileStateValue` nodes (mtime/inode/digest). Both paths feed into a **`RecordingDifferencer`** that marks Skyframe nodes dirty, triggering bottom-up re-evaluation of only the affected transitive closure. The **server daemon** holds the Skyframe graph in memory across invocations; the **client** is a thin CLI launcher that finds/starts the server.

---

## Findings

### 1. Architecture Overview: Daemon + Skyframe Graph

Bazel runs as a **long-lived server process** (`bazel(workspace)` in `ps` output), while the `bazel` CLI binary is a thin client that finds or starts the server. [Source](https://github.com/bazelbuild/bazel/blob/master/site/en/run/client-server.md)

The server holds the **Skyframe dependency graph** in memory across invocations. Skyframe is Bazel's incremental evaluation framework: `SkyFunction`s build `SkyValue` nodes by requesting dependent nodes via `env.getValue()`. The graph records all dependencies from input files through analysis to output artifacts. [Source](https://bazel.build/reference/skyframe)

Key node types for filesystem monitoring:
- **`FileStateValue`** — The leaf node, equivalent to `lstat()`. Stores file type + either digest or `FileContentsProxy` (mtime + inode). Has no dependencies. [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/actions/FileStateValue.java)
- **`FileValue`** — Resolves symlinks and depends on `FileStateValue` for the file and its ancestors.
- **`DirectoryListingStateValue`** — The leaf for directory listings, equivalent to `readdir()`.
- **`DirectoryListingValue`** — Wraps `DirectoryListingStateValue` with symlink resolution.

### 2. The Two-Tier Change Detection Flow

At the start of every build command, `SkyframeExecutor.handleDiffs()` is called (via `BuildTool.buildTargets()`). The flow: [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/SkyframeExecutor.java) — see `handleDiffs()` method at ~line 2800+

**Tier 1 — `DiffAwareness` (fast path):** For each package path entry, the `DiffAwarenessManager` queries a `DiffAwareness` instance via `getCurrentView()` → `getDiff(oldView, newView)`. If this succeeds and returns specific files, they're passed via `handleDiffsWithCompleteDiffInformation()`.

**Tier 2 — `FilesystemValueChecker` (fallback):** When DiffAwareness reports "everything modified" or is unavailable, `handleDiffsWithMissingDiffInformation()` kicks in. This creates a `FilesystemValueChecker` that iterates over **all nodes in the Skyframe graph**, calls `SkyValueDirtinessChecker.check()` on each relevant node, and compares new `FileStateValue`/`DirectoryListingStateValue` instances against the cached ones.

The two tiers are combined in `SkyframeExecutor.handleDiffs()`:
```
if diffAwareness returns specific files → handleDiffsWithCompleteDiffInformation()
if diffAwareness reports EVERYTHING_MODIFIED → handleDiffsWithMissingDiffInformation()
```

### 3. `DiffAwareness` Interface — Platform-Native Watchers

The `DiffAwareness` interface is a straightforward snapshot-then-diff contract: [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/DiffAwareness.java)

```java
// Key methods:
View getCurrentView(OptionsProvider options);      // Take a snapshot
ModifiedFileSet getDiff(View oldView, View newView); // Find changes between snapshots
```

**MacOSXFsEventsDiffAwareness** — Uses the macOS FSEvents API via JNI. A native C++ thread runs a `CFRunLoop` with an `FSEventStream`. Events accumulate in a shared list protected by a mutex. On each `getCurrentView()`, the Java side calls `poll()` to drain the native event queue. [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/native/darwin/fsevents.cc) — see `FsEventsDiffAwarenessCallback` and `poll()`

- Events are coalesced with a default 5ms latency.
- If `kFSEventStreamEventFlagMustScanSubDirs` is set (dropped events), `everything_changed = true` → returns `ModifiedFileSet.EVERYTHING_MODIFIED`.
- Directory renames also force `everything_changed = true` (can't track inode renaming reliably).
- Supports up to 8 excluded paths (e.g., `.build` folders). [Source](https://github.com/bazelbuild/bazel/pull/26921)

**WatchServiceDiffAwareness** — Uses Java's `WatchService` (backed by inotify on Linux). On first call, recursively registers all directories in the workspace. Each subsequent call drains `WatchKey` events and translates `ENTRY_CREATE/ENTRY_DELETE/ENTRY_MODIFY` to changed paths. New directories are recursively registered on-the-fly. [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/WatchServiceDiffAwareness.java) — see `collectChanges()` method

- Both implementations extend `LocalDiffAwareness` which enforces sequential view semantics via `SequentialView` (position counter + owner identity check). [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/LocalDiffAwareness.java)
- If --watchfs is toggled between invocations, the DiffAwareness is discarded and recreated, ensuring `ModifiedFileSet.EVERYTHING_MODIFIED` for the transitional build.
- A Watchman-backed implementation was also proposed. [Source](https://github.com/bazelbuild/bazel/pull/22615)

### 4. FilesystemValueChecker — The Manual Scan Fallback

When `DiffAwareness` can't provide precise diffs, Bazel falls back to a parallel graph scan: [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/FilesystemValueChecker.java) — see `getDirtyKeys()` and `getDirtyValues()`

The scanning works by:
1. Iterating over **all keys in the Skyframe graph** (or a filtered subset for external/output files)
2. For each key matching a `SkyValueDirtinessChecker`:
   - Call `checker.check(key, oldValue)` which creates a new filesystem value
   - Compare: `newValue.equals(oldValue)` → not dirty; otherwise dirty
3. **Key optimization**: New values are injected directly into the graph via `Differencer.DiffWithDelta` (which pairs old and new values), avoiding re-computation during evaluation

The `FileDirtinessChecker` is the primary checker for file state: [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/DirtinessCheckerUtils.java) — lines ~40-55

```java
public static class FileDirtinessChecker extends SkyValueDirtinessChecker {
  @Override
  public boolean applies(SkyKey skyKey) {
    return skyKey.functionName().equals(FILE_STATE);
  }
  @Override
  public SkyValue createNewValue(SkyKey key, SyscallCache syscallCache, ...) {
    return FileStateValue.create((RootedPath) key.argument(), syscallCache, tsgm);
  }
}
```

### 5. mtime/Digest Handling — `FileStateValue` Internals

`FileStateValue.create()` determines what metadata to store: [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/actions/FileStateValue.java) — lines ~70-120

Three cases for regular files:
- **Fast digest available** (e.g., xattr on Linux ext4/XFS): `RegularFileStateValueWithDigest(size, digestBytes)` — most robust, handled via `tryGetDigest()` which first tries `stat.getDigest()` then `xattrProvider.getFastDigest()`
- **No fast digest → `FileContentsProxy`**: `RegularFileStateValueWithContentsProxy(size, contentsProxy)` — where `FileContentsProxy` is mtime + inode number (and ctime on Linux). This is the **fallback detection** mechanism.
- **Metadata-backed** (for output artifacts): `RegularFileStateValueWithMetadata(FileArtifactValue)`

The equality comparison (`FileStateValue.equals()`) is what determines "changed":
- `RegularFileStateValueWithDigest`: compares `size` + `Arrays.equals(digest)`
- `RegularFileStateValueWithContentsProxy`: compares `size` + `Objects.equals(contentsProxy)`

**`FileContentsProxy`** contains: mtime (lastModifiedTime), ctime (lastChangeTime), and nodeId (inode). On filesystems where mtime alone isn't reliable (e.g., `tar` extraction preserves mtime), ctime and inode provide additional change detection. [Source](https://github.com/bazelbuild/bazel/commit/11f7d8040fadc595589ee264561606dc2a83685d) — commit message explains ctime addition

**`TimestampGranularityMonitor`** handles the edge case where a file is modified, rebuilt, and modified again within the same filesystem timestamp granularity (typically 1s). If any checked file has a timestamp equal to the command start time, Bazel waits until the clock advances before exiting. [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/util/io/TimestampGranularityMonitor.java)

### 6. Invalidation and Evaluation (Skyframe Integration)

The diff results (from either tier) flow into the **`RecordingDifferencer`**: [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/SkyframeExecutor.java) — see `handleChangedFiles()`

```java
// In handleChangedFiles():
recordingDiffer.invalidate(changedKeysWithoutNewValues);  // Mark dirty
recordingDiffer.inject(changedKeysWithNewValues);          // Inject new values + mark dirty
modifiedFiles.addAndGet(count);
incrementalBuildMonitor.accrue(allChangedKeys);
```

Skyframe uses **bottom-up invalidation**: when leaf nodes (FileStateValue) are marked dirty, the reverse transitive closure of dependents is invalidated. During evaluation:
1. The `SkyFunction` for each dirty node is re-run
2. If the re-evaluation produces the same value as before, **change pruning** "resurrects" the downstream dependents — they remain valid
3. This is why changing a comment in a C++ file may not trigger re-linking (the `.o` hasn't changed)

**`SkyValueDirtinessChecker`** is the abstract base for all dirtiness checking: [Source](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/SkyValueDirtinessChecker.java) — its default `check()` method creates a new value, compares with `equals()`, and returns `DirtyResult.dirtyWithNewValue(newValue)` or `DirtyResult.notDirty()`.

### 7. Daemon/Client Responsibilities

**Server (daemon):**
- Holds the Skyframe graph in memory
- Owns the `DiffAwareness` instances (platform watchers) between commands
- Runs `handleDiffs()` at the start of each command to invalidate changed nodes
- Manages the `RecordingDifferencer` that bridges diff results to Skyframe
- Controls `SyscallCache` for filesystem operations
- Handles `TimestampGranularityMonitor` lifecycle
- Performs `discardPreExecutionCache()` to drop analysis-phase data before execution

**Client (CLI):**
- Finds or starts the server process
- Sends command requests over a gRPC/Netty connection
- Has **no involvement** in filesystem monitoring
- Checks server version matches client version

### 8. Implications for a Pixi Daemon

**What Bazel does that Pixi should adopt:**

1. **Two-tier monitoring**: A fast native watcher (`notify` on Linux, `ReadDirectoryChangesW` on Windows, FSEvents on macOS) for the common case + a full scan fallback for robustness. Bazel's approach of treating watcher failures as non-fatal (falling back to scan) is key for reliability.

2. **Content-addressable leaf nodes**: `FileStateValue` as an immutable, equatable snapshot of file state enables trivial change detection via `equals()`. For Pixi's glob matching + mtime freshness use case, a similar lightweight struct (storing type + mtime + optional digest) would let the compute engine detect changes without re-stat'ing.

3. **Single change detection point**: All file inputs route through one mechanism (Skyframe's `FileStateValue` keyed by path). The glob matching function and the mtime freshness checker would both depend on the same leaf values — change once, invalidate all consumers.

4. **`RecordingDifferencer` pattern**: A queue of pending invalidations processed at evaluation start. This decouples change detection from graph evaluation — the watcher can accumulate changes independently.

5. **`SyscallCache`**: Bazel's `SyscallCache` batches and deduplicates filesystem operations (stat, readdir). For a Pixi daemon where many globs hit the same directories, caching directory listings would avoid redundant syscalls.

**What Bazel does that Pixi should simplify:**

1. **Bottom-up invalidation** may be overkill for Pixi's first slice. A simpler approach: the daemon's filesystem watcher directly notifies the compute engine of changed paths, and the engine invalidates dependent computations. No need for a full Skyframe graph initially.

2. **Bazel's daemon is command-driven** (each `bazel build` is a separate command). Pixi's daemon should be **event-driven** — the watcher thread continuously feeds the compute engine, which eagerly re-evaluates dirty computations.

3. **Bazel's mtime-vs-digest complexity** stems from correctness requirements across many filesystems. For Pixi's glob+mtime freshness use case, straight mtime comparison (with `TimestampGranularityMonitor` for timestamp race conditions) is sufficient.

4. **Symlink resolution complexity**: Bazel's `FileFunction` has hundreds of lines handling symlink cycles and ancestor resolution. Pixi can likely punt on this initially.

---

## Sources

**Kept (primary sources with permalinks):**

1. [FileStateValue.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/actions/FileStateValue.java) — The leaf Skyframe node for file state. Shows the three variants (digest, FileContentsProxy, metadata) and how `create()` chooses between them.

2. [FileStateFunction.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/FileStateFunction.java) — The SkyFunction wrapper that calls `FileStateValue.create()`. Shows how external files get special handling.

3. [FileFunction.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/FileFunction.java) — Symlink-resolving SkyFunction. Depends on `FileStateValue` leaf nodes.

4. [DiffAwareness.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/DiffAwareness.java) — The interface defining the snapshot/diff contract for platform watchers.

5. [DiffAwarenessManager.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/DiffAwarenessManager.java) — Manages one `DiffAwareness` per package path entry. Caches `View` snapshots between calls.

6. [MacOSXFsEventsDiffAwareness.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/MacOSXFsEventsDiffAwareness.java) — Java side of FSEvents watcher.

7. [fsevents.cc](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/native/darwin/fsevents.cc) — Native C++ FSEvents implementation. Shows `kFSEventStreamEventFlagMustScanSubDirs` handling.

8. [WatchServiceDiffAwareness.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/WatchServiceDiffAwareness.java) — Linux inotify-based watcher (via Java WatchService). Shows recursive directory registration and event replay.

9. [LocalDiffAwareness.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/LocalDiffAwareness.java) — Base class for platform watchers. Enforces sequential view ordering.

10. [FilesystemValueChecker.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/FilesystemValueChecker.java) — The manual graph scan fallback. Parallel `getDirtyKeys()` and `getDirtyActionValues()`.

11. [DirtinessCheckerUtils.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/DirtinessCheckerUtils.java) — `FileDirtinessChecker`, `DirectoryDirtinessChecker`, and `MissingDiffDirtinessChecker`.

12. [SkyValueDirtinessChecker.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/SkyValueDirtinessChecker.java) — Abstract base for dirtiness checking. Default `check()` creates new value, compares with `equals()`.

13. [SkyframeExecutor.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/skyframe/SkyframeExecutor.java) — Central orchestrator. `handleDiffs()`, `handleDiffsWithCompleteDiffInformation()`, `handleDiffsWithMissingDiffInformation()`, `handleChangedFiles()`.

14. [TimestampGranularityMonitor.java](https://bazel.googlesource.com/bazel/+/refs/heads/release-9.1.0/src/main/java/com/google/devtools/build/lib/util/io/TimestampGranularityMonitor.java) — Prevents timestamp race conditions across command boundaries.

15. [Skyframe documentation](https://bazel.build/reference/skyframe) — Official docs on Skyframe data model, evaluation, and incrementality.

16. [Client/Server documentation](https://github.com/bazelbuild/bazel/blob/master/site/en/run/client-server.md) — Explains the daemon/client architecture and lifecycle.

17. [FileContentsProxy ctime addition](https://github.com/bazelbuild/bazel/commit/11f7d8040fadc595589ee264561606dc2a83685d) — Commit showing mtime + ctime + nodeId in proxy.

18. [Watchman DiffAwareness PR](https://github.com/bazelbuild/bazel/pull/22615) — Proposed Watchman-backed implementation showing intent to support multiple watcher backends.

**Dropped:**

- `ibazel` (bazel-watcher) documentation — This is a separate tool for auto-rebuilding on file changes; not Bazel's internal filesystem monitoring.
- Skyframe invalidation with disk/remote cache issues (#22367, #24763) — Bug reports about cache invalidation edge cases, not core architecture documentation.
- ActionCacheChecker.java — About action cache invalidation, not filesystem monitoring.

---

## Gaps

1. **Windows `ReadDirectoryChangesW` watcher**: The `--experimental_windows_watchfs` flag is present but the implementation wasn't examined. The existing `WatchServiceDiffAwareness` may work on Windows but with caveats.

2. **`SyscallCache` internal batching logic**: Not examined in detail. Could be relevant for optimizing Pixi's glob operations that hit the same directories.

3. **The `SequencedSkyframeExecutor` subclass**: This concrete class was not fully examined — it may contain additional diff-handling logic specific to Bazel's build flow.

4. **Network filesystem handling**: Bazel excludes known network filesystem prefixes from `DiffAwareness` (configurable via `--experimental_watchfs_excluded_prefixes`), but the full list wasn't surfaced.

5. **Performance numbers**: No benchmarks were found comparing the three detection strategies (FSEvents, inotify, manual scan) for real-world workspace sizes.

## Suggested Next Steps

- Examine `SyscallCache` for directory listing caching that could be reused in Pixi's glob matching.
- Prototype a Pixi-native watcher using `notify` (Rust) with a content-addressable file-state struct similar to `FileStateValue`, feeding a simplified compute engine.
- Investigate whether `notify`'s debouncing matches Bazel's FSEvents latency approach (5ms default) for Pixi's use case.
