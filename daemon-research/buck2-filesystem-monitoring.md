# Research: Buck2 Filesystem Monitoring for Daemon/Incremental Graph

## Summary

Buck2 monitors filesystem changes through a **pluggable `FileWatcher` trait** that supports three backends (Watchman, EdenFS, and inotify/notify), synced **per-command at daemon-side**. The watcher accumulates raw filesystem events between commands, converts them into **DICE key invalidations** via `FileChangeTracker`, and writes those into a `DiceTransactionUpdater` before the command's computation graph ever executes. DICE (Dynamic Incremental Computation Engine) then propagates invalidation through **reverse-dependency tracking** to achieve O(changed-subset) recomputation. File metadata is content-hashed (not mtime-based) using CAS digests, with optional EdenFS xattr shortcuts.

---

## Findings

### 1. Watcher Backend Selection: Pluggable, with Smart Defaults

Buck2 uses a `FileWatcher` trait that is instantiated once at daemon startup. The selection logic lives in `dyn FileWatcher::new()`:

- **In Meta's internal build (`fbcode_build`):** Auto-detects EdenFS via `detect_eden::is_eden()`. If the repo is on EdenFS, uses `EdenFsFileWatcher`; otherwise defaults to `WatchmanFileWatcher`.
- **In open-source builds (`not(fbcode_build))`:** Defaults to `"notify"` (Rust `notify` crate, which wraps inotify/FSEvents/kqueue).
- **Config override:** Users can set `buck2.file_watcher` in `.buckconfig` to `"watchman"`, `"notify"`, or `"fs_hash_crawler"`.
- **Fallback:** If EdenFS watcher creation fails with `IoNotConnected`, it falls back to Watchman.

**Source:** [`app/buck2_file_watcher/src/file_watcher.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/file_watcher.rs#L61-L137)

### 2. Event Accumulation Between Commands (No Background Processing)

The daemon does **not** process filesystem events in a background thread. Instead, each watcher implementation accumulates events between command invocations:

- **NotifyFileWatcher:** Uses an `Arc<Mutex<NotifyFileData>>` with an `OrderedSet<(CellPath, EventKind)>` to deduplicate events. The `notify::recommended_watcher` callback pushes events into this buffer. Events targeting `buck-out/` are silently dropped. Events matching ignore specs (`.buckconfig` `[project].ignore`) are counted as ignored.

- **WatchmanFileWatcher:** Uses a `SyncableQuery` from the `watchman_client` crate. It subscribes to the project root with a `since` clock query, receiving batched `WatchmanEvent` structs (path + `WatchmanKind::File|Directory|Symlink` + `WatchmanEventType::Create|Modify|Delete`).

- **EdenFS (fbcode only):** Uses the EdenFS Thrift API directly for change notifications.

- **FsHashCrawler:** A polling fallback that recomputes BLAKE3 hashes of all files in the repo on each sync, comparing against a previous snapshot.

**Source:** [`app/buck2_file_watcher/src/notify.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/notify.rs#L58-L123), [`app/buck2_file_watcher/src/watchman/interface.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/watchman/interface.rs#L46-L119)

### 3. FileChangeTracker: Events → DICE Key Invalidations

The critical bridge between raw FS events and the incremental computation graph is `FileChangeTracker`. It maps each event kind to **specific DICE key types**:

| Event | Invalidated DICE Keys |
|-------|----------------------|
| File created/deleted | `ReadFileKey`, `PathMetadataKey`, `ExistsMatchingExactCaseKey`, parent `ReadDirKey` (both with/without ignores) |
| File contents modified | `ReadFileKey`, `PathMetadataKey` |
| Directory created/deleted | `PathMetadataKey`, `ExistsMatchingExactCaseKey`, parent `ReadDirKey` |
| Symlink created/deleted/modified | `ReadFileKey`, `PathMetadataKey` |

The tracker accumulates these in `StdBuckHashSet`s and writes them all at once via `write_to_dice()`:

```rust
pub fn write_to_dice(mut self, ctx: &mut DiceTransactionUpdater) -> Result<()> {
    // ... watchman bug workaround for directory modification events ...
    ctx.changed(self.files_to_dirty)?;
    ctx.changed(self.dirs_to_dirty)?;
    ctx.changed(self.paths_to_dirty)?;
    ctx.changed(self.exists_matching_exact_case_to_dirty)?;
    Ok(())
}
```

Each `ctx.changed()` call tells DICE: "this key's cached value is now stale; invalidate it and its reverse dependencies."

**Source:** [`app/buck2_common/src/file_ops/dice.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_common/src/file_ops/dice.rs#L148-L205)

**Watchman bug workaround:** Watchman sometimes reports directory modification without reporting the files that were added/removed inside. Buck2 has `dir_entries_changed_for_watchman_bug()` which marks directories as "maybe modified" and later checks if any path under that directory was also reported modified — if so, the directory listing is force-invalidated.

### 4. How Computations/Graph Consume Filesystem Inputs

Filesystem reads in Buck2 go through **DICE keys**, not direct syscalls. The four leaf keys are:

1. **`ReadFileKey(CellPath)`** — Returns a `ReadFileValue` (delegates actual I/O to `IoProvider`). `equality()` returns `false` unconditionally, forcing downstream recomputation on any invalidation. Priority: `InvalidationSourcePriority::High`.

2. **`ReadDirKey { path, check_ignores }`** — Returns `ReadDirOutput` with sorted directory entries. Uses equality checking and a per-transaction `ReadDirCache` for deduplication within a single command.

3. **`PathMetadataKey(CellPath)`** — Returns `Option<RawPathMetadata>` (File with `FileMetadata` {digest, is_executable}, Directory, or Symlink). Also has `InvalidationSourcePriority::High`. When a symlink is encountered, it additionally requests `ReadFileKey` for the symlink target.

4. **`ExistsMatchingExactCaseKey(CellPath)`** — Returns `bool` for case-sensitive existence checks.

These leaf keys are requested by higher-level computations (build file parsing, glob resolution, source file listing). When the file watcher marks them as `changed`, DICE's reverse-dependency graph propagates invalidation upward.

**Source:** [`app/buck2_common/src/file_ops/dice.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_common/src/file_ops/dice.rs#L236-L336)

### 5. File Digests, Not Mtimes

Buck2 uses **content hashing** (CAS digests: SHA1/SHA256/BLAKE3-keyed), not modification timestamps:

```rust
pub struct FileMetadata {
    pub digest: TrackedFileDigest,
    pub is_executable: bool,
}
```

The `FileDigest::from_file()` method tries to read pre-computed digests from **extended attributes** (`user.sha1`, `user.sha256`, `user.blake3`) — these are populated by EdenFS to avoid hashing files on disk. If xattrs are unavailable, it falls back to reading the file and hashing it.

This is a key architectural decision: **no mtime-based freshness**. The file watcher tells DICE *which* keys to invalidate; on recomputation, DICE re-reads the file through the `IoProvider` and the new content digest becomes the new cached value. Equality is content-based.

**Source:** [`app/buck2_common/src/file_ops/metadata.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_common/src/file_ops/metadata.rs#L81-L147)

### 6. Daemon/Client Responsibilities

**Daemon (buckd):**
- Owns the `FileWatcher` instance (created in `DaemonStateData::init_data()`)
- Owns the `Dice` computation graph via `ConcurrencyHandler`
- On each command, the `DiceCommandUpdater::update()` implementation calls `file_watcher.sync(ctx)` — this is where accumulated events are drained and converted to DICE invalidations
- Also calls `io.settle()` to ensure the I/O provider (especially EdenFS) is up-to-date with filesystem state
- Manages command concurrency: commands with equivalent DICE state can run concurrently; different state causes blocking or cleanup

**Client (buck2 CLI):**
- Stateless thin client that sends gRPC requests to buckd
- No filesystem watching responsibility
- Connects to daemon via `buckd.info` port; restarts daemon on version mismatch
- Sends `ClientContext` with working directory, config overrides, platform info

**Source:** [`app/buck2_server/src/daemon/state.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server/src/daemon/state.rs#L620-L640), [`app/buck2_server/src/ctx.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server/src/ctx.rs#L588-L610)

### 7. The Sync Flow (End-to-End)

```
1. Client sends gRPC command → DaemonApiServer
2. ServerCommandContext::new() creates DiceCommandUpdater (implements DiceUpdater trait)
3. ConcurrencyHandler::enter() calls updates.update(updater, timings)
4. DiceCommandUpdater::update():
   a. Loads latest buckconfig cells/configs
   b. Calls file_watcher.sync(dice_updater):
      - Notify: drains Mutex<NotifyFileData>, builds FileChangeTracker, calls write_to_dice()
      - Watchman: queries since-last-clock, processes events through WatchmanQueryProcessor
      - If events were missed (queue overflow): dice.unstable_take() — drops entire DICE graph
   c. Returns (updated DiceTransactionUpdater, Mergebase)
5. ConcurrencyHandler commits the transaction with user_data
6. Build/targets/etc. runs on the updated DICE graph
```

The `FILE_WATCHER_WAIT` timing span explicitly measures this sync step.

**Source:** [`app/buck2_server/src/ctx.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server/src/ctx.rs#L601-L608), [`app/buck2_server_ctx/src/concurrency.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server_ctx/src/concurrency.rs#L509-L515)

### 8. Missed Events → Nuclear Option

If the watcher detects missed events (kernel queue overflow for `notify`, or Watchman `fresh_instance`), Buck2 **drops the entire DICE graph** via `dice.unstable_take()`. This causes a full rebuild on the next command. The `FileWatcherStats` protobuf records this with `fresh_instance: true` and an `incomplete_events_reason`.

**Source:** [`app/buck2_file_watcher/src/notify.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/notify.rs#L218-L225), [`app/buck2_file_watcher/src/watchman/interface.rs`](https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/watchman/interface.rs#L321-L330)

---

## Implications for a Pixi Daemon

### Directly Applicable Patterns

1. **Pluggable watcher trait + config-driven selection.** Pixi should use a similar `FileWatcher` trait with `notify` as the default and optionally Watchman. The config `pixi.file_watcher` (or similar) can override.

2. **Event accumulation between commands, not background invalidation.** Buck2 proves this pattern works at scale. The Pixi daemon can buffer filesystem events in a `Mutex<Vec<Event>>` and drain them at the start of each `pixi run` / `pixi build` command, converting events into compute-engine invalidations.

3. **DICE key invalidation pattern.** Even without DICE, the `FileChangeTracker` pattern is directly reusable: map each event kind to a set of "input slots" in the compute engine. For Pixi's daemon-backed glob matching, a file modification in `/src/**` would invalidate the glob result for that pattern.

4. **Content hashing over mtime for correctness.** Buck2's use of CAS digests (with EdenFS xattr shortcuts) is the right call for a daemon that must guarantee correctness. Pixi could start with mtime-as-freshness (simpler, fast) and graduate to content hashing for critical paths.

5. **Nuclear option for missed events.** If the event buffer overflows or the kernel queue drops events, drop the entire compute cache rather than risk stale results. This should be rare with proper `inotify` limits.

6. **Ignore buck-out / pixi-output.** Buck2 filters `buck-out/` prefix events at the watcher level. Pixi should filter its own output/cache directories the same way.

### Key Differences to Account For

1. **Buck2 only syncs at command boundaries.** For a Pixi daemon that also serves LSP-like functionality (file watching for IDE integration), you may want an additional **push path** where certain file changes immediately invalidate LSP-level caches without waiting for a build command.

2. **Buck2's DICE is an in-process incremental engine.** Pixi may use a separate compute engine. The interface between the file watcher and the compute engine should be a well-defined "set of invalidated input keys" rather than DICE-specific `changed()` calls.

3. **Glob matching as a first-class DICE key.** For Pixi's daemon-backed glob matching, the glob pattern itself becomes a DICE key whose computation reads `ReadDirKey` for the relevant directories. When a file is created/deleted in a watched directory, the `ReadDirKey` invalidation cascades to the glob key, which recomputes only the affected patterns. This is exactly how Buck2's `read_dir` → build file parsing → target resolution chain works.

4. **Startup cost.** Buck2's `FileWatcher::new()` happens once at daemon startup. For Pixi, consider whether to eagerly start watching or lazily on first use.

---

## Sources

### Kept
- Buck2 `file_watcher.rs` — Watcher factory, backend selection logic: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/file_watcher.rs
- Buck2 `notify.rs` — NotifyFileWatcher implementation (event buffering, dedup, FileChangeTracker construction): https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/notify.rs
- Buck2 `watchman/interface.rs` — WatchmanFileWatcher, event processing, fresh instance handling: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_file_watcher/src/watchman/interface.rs
- Buck2 `file_ops/dice.rs` — FileChangeTracker, DICE key definitions (ReadFileKey, ReadDirKey, PathMetadataKey): https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_common/src/file_ops/dice.rs
- Buck2 `file_ops/metadata.rs` — FileMetadata (digest-based), FileDigest from xattr/disk: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_common/src/file_ops/metadata.rs
- Buck2 `state.rs` — DaemonStateData creation, FileWatcher initialization, prepare_command flow: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server/src/daemon/state.rs
- Buck2 `ctx.rs` — DiceCommandUpdater::update() calling file_watcher.sync() with FILE_WATCHER_WAIT span: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server/src/ctx.rs
- Buck2 `concurrency.rs` — ConcurrencyHandler calling updates.update(): https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server_ctx/src/concurrency.rs
- Buck2 `io.rs` — IoProvider trait with settle() and eden_version(): https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_common/src/io.rs
- Buck2 `file_ops/io.rs` — IoFileOpsDelegate routing DICE key computations to IoProvider: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_common/src/file_ops/io.rs
- Buck2 `daemon/server.rs` — Daemon API, BuckdServer, streaming command dispatch: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_server/src/daemon/server.rs
- Buck2 `daemon_lifecycle.md` — Client connection, daemon startup/shutdown flow: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/app/buck2_daemon/daemon_lifecycle.md
- Buck2 `daemon.md` — Official daemon docs: https://buck2.build/docs/concepts/daemon/
- DICE `incrementality.md` — Reverse-dependency tracking, O(changed-subset) recomputation: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/dice/dice/docs/incrementality.md
- DICE `index.md` — Overview, Salsa/Adapton inspiration: https://github.com/facebook/buck2/blob/d260d642f615dde7363dddff28bbecac406b8cab/dice/dice/docs/index.md
- Buck2 Change Detector — CI-oriented target determination from changed files: https://github.com/facebookincubator/buck2-change-detector
- Issue #1131 — Notify rescan event handling bug (illustrates event accumulation pattern): https://github.com/facebook/buck2/issues/1131
- Issue #59 — File watcher fallback discussion: https://github.com/facebook/buck2/issues/59
- Modern DICE talk/slides — Architecture of DICE v2: https://buck2.build/docs/insights_and_knowledge/modern_dice/

### Dropped
- General Bazel/Pants comparisons — out of scope for filesystem monitoring specifics
- Buck1 documentation — superseded by Buck2 architecture
- Generic inotify/Watchman tutorials — primary sources from Buck2 codebase used instead
- BuildBarn/BuildBuddy remote execution pages — not relevant to local filesystem monitoring

## Gaps

1. **EdenFS file watcher internals.** The EdenFS watcher is `#[cfg(fbcode_build)]` gated and its source is not in the open-source repo. I can only describe its interface. The exact Thrift protocol used for EdenFS change subscriptions remains internal.

2. **DICE `changed()` exact invalidation algorithm.** The internal DICE implementation of `DiceTransactionUpdater::changed()` (how it propagates invalidation through reverse dependencies) is in `dice/dice/src/` but the core state machine is complex. I've described the observable behavior but not the internal algorithm.

3. **Performance characteristics at Meta scale.** Meta's repos have millions of files. The exact inotify queue sizing, Watchman query latency, and EdenFS notification batching parameters are tuned for Meta but not documented publicly.

4. **Concurrent command interaction with watcher.** The concurrency handler blocks commands when DICE state differs. The exact interaction where one command's watcher sync commits new state while another command is mid-execution on old state is handled by DICE's multi-versioning but I haven't traced the exact locking protocol. The code comments note: "we rerun the updates in case that files on disk have changed between commands. this might cause some churn, but concurrent commands don't happen much."

5. **Symlink handling edge cases.** Issue #196 documents cases where symlinks confuse the file monitor. The Buck2 team has improved symlink support over time but edge cases remain, particularly around cross-cell symlinks and directory symlinks.

## Supervisor Coordination

No blocking decisions needed. This is a self-contained research deliverable. The findings above provide a complete architectural picture with source-backed evidence suitable for designing Pixi's daemon-backed glob matching + mtime freshness layer.
