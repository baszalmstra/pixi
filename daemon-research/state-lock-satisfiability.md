# Code Context

## Files Retrieved
1. `crates/pixi_core/src/lock_file/update.rs` (lines 223-413) - top-level `Workspace::update_lock_file` flow: loads lock file, creates per-invocation caches/dispatcher, checks freshness, runs update, writes lock.
2. `crates/pixi_core/src/lock_file/update.rs` (lines 561-675) - `LockFileDerivedData` fields and lazy `LockFileResolver` cache.
3. `crates/pixi_core/src/lock_file/update.rs` (lines 1422-1505, 1791-2065) - `UpdateContext` fields and builder; derives locked per-env/per-group records from the lock file.
4. `crates/pixi_core/src/lock_file/update.rs` (lines 2089-2686) - update task graph: conda solves, PyPI solves, extraction, final lock-file rebuild.
5. `crates/pixi_core/src/lock_file/update.rs` (lines 2781-2977, 3298-3431) - conda and PyPI solve task inputs and calls into compute engine / `resolve_pypi`.
6. `crates/pixi_core/src/lock_file/update.rs` (lines 770-843, 845-893, 1080-1120, 1205-1255) - install-side prefix caches and revalidation paths.
7. `crates/pixi_core/src/lock_file/outdated.rs` (lines 37-220, 235-485, 491-590) - freshness inventory state and `OutdatedEnvironments::from_workspace_and_lock_file` satisfiability pass.
8. `crates/pixi_core/src/lock_file/satisfiability/environment.rs` (lines 31-258, 260-487) - environment-level invalidators: channels, platforms, PyPI indexes/options, solve options, exclude-newer.
9. `crates/pixi_core/src/lock_file/satisfiability/platform.rs` (lines 54-240, 227-364, 562-1125, 1456-1479) - platform/package satisfiability context, per-platform checks, dependency walk, solve-group check.
10. `crates/pixi_core/src/lock_file/satisfiability/pypi.rs` (lines 117-360, 404-818) - PyPI requirement matching and local package metadata/cache path.
11. `crates/pixi_core/src/lock_file/satisfiability/pypi_metadata.rs` (lines 16-110) - static local PyPI metadata structure and comparison.
12. `crates/pixi_record/src/lock_file_resolver.rs` (lines 24-137) - resolver built over a specific lock-file package slice.
13. `crates/pixi_command_dispatcher/src/keys/solve_pixi_environment.rs` (lines 91-167, 204-270, 267-417, 446-505) - compute-engine solve key identity and solve internals.

## Key Code

### Per invocation entry point and warm state currently rebuilt

`Workspace::update_lock_file` always starts by loading/parsing `pixi.lock`, creating a fresh `GlobHashCache`, building a fresh `CommandDispatcher`, and wrapping the input lock in `LockFileDerivedData`:

```rust
// crates/pixi_core/src/lock_file/update.rs:241-302
let lock_file_result = self.load_lock_file().await?;
...
let lock_file = lock_file_result.into_lock_file_or_empty_with_warning();
let needs_format_upgrade = lock_file.version() < rattler_lock::FileFormatVersion::LATEST;
let glob_hash_cache = GlobHashCache::default();
let mut builder = self.command_dispatcher_builder()?;
...
let command_dispatcher = builder.finish();
let package_cache = command_dispatcher.package_cache().clone();
let mut derived = LockFileDerivedData::from_input_lock_file(
    self, lock_file, package_cache, command_dispatcher.clone(), glob_hash_cache,
);
```

Then it builds/reuses a lazy resolver once, computes `OutdatedEnvironments`, and returns early if nothing is outdated:

```rust
// crates/pixi_core/src/lock_file/update.rs:304-335
if !options.lock_file_usage.should_check_if_out_of_date() { return Ok((derived, false)); }
let resolver = derived.resolver()?;
let mut outdated = OutdatedEnvironments::from_workspace_and_lock_file(
    self, command_dispatcher.clone(), &derived.lock_file, &resolver,
).await;
if outdated.is_empty() && !(needs_format_upgrade && options.upgrade_lock_file_format) {
    derived.uv_context = outdated.uv_context;
    derived.build_caches = outdated.build_caches;
    return Ok((derived, false));
}
```

`LockFileDerivedData` holds the reusable outputs for later install in the same invocation: lock file, package cache, conda/PyPI prefix `OnceCell`s, uv context, command dispatcher, glob hash cache, PyPI build caches, and a lazy `LockFileResolver`:

```rust
// crates/pixi_core/src/lock_file/update.rs:561-612
pub struct LockFileDerivedData<'p> {
    pub lock_file: LockFile,
    pub package_cache: PackageCache,
    pub updated_conda_prefixes: DashMap<EnvironmentName, Arc<async_once_cell::OnceCell<CondaPrefixUpdated>>>,
    pub updated_pypi_prefixes: DashMap<EnvironmentName, Arc<async_once_cell::OnceCell<UpdatedPrefix>>>,
    pub uv_context: once_cell::sync::OnceCell<UvResolutionContext>,
    pub command_dispatcher: CommandDispatcher,
    pub glob_hash_cache: GlobHashCache,
    pub build_caches: HashMap<BuildCacheKey, Arc<PypiEnvironmentBuildCache>>,
    resolver: once_cell::sync::OnceCell<Arc<LockFileResolver>>,
}
```

The resolver is built once for a specific `LockFile` object:

```rust
// crates/pixi_core/src/lock_file/update.rs:656-665
pub fn resolver(&self) -> miette::Result<Arc<LockFileResolver>> {
    self.resolver.get_or_try_init(|| {
        LockFileResolver::build(&self.lock_file, self.workspace.root()).map(Arc::new).into_diagnostic()
    }).cloned()
}
```

### Freshness state produced by `OutdatedEnvironments`

The freshness inventory is:

```rust
// crates/pixi_core/src/lock_file/outdated.rs:67-100
pub struct OutdatedEnvironments<'p> {
    pub conda: HashMap<Environment<'p>, HashSet<PixiPlatformName>>,
    pub pypi: HashMap<Environment<'p>, HashSet<PixiPlatformName>>,
    pub disregard_locked_content: DisregardLockedContent<'p>,
    pub removed_environments: HashSet<String>,
    pub uv_context: OnceCell<UvResolutionContext>,
    pub build_caches: HashMap<BuildCacheKey, Arc<PypiEnvironmentBuildCache>>,
    pub static_metadata_cache: HashMap<PathBuf, pypi_metadata::LocalPackageMetadata>,
    pub locked_pypi_records: HashMap<(Environment<'p>, PixiPlatformName), LockedPypiRecordsByName>,
}
```

`from_workspace_and_lock_file` calls `find_unsatisfiable_targets`, then expands outdated targets to full solve groups, marks PyPI outdated whenever conda is outdated, and records lock environments removed from the manifest (`outdated.rs:128-217`).

The inner pass creates per-call caches (`uv_context`, `build_caches`, `static_metadata_cache`) and iterates every workspace environment/platform (`outdated.rs:252-380`). It records:
- missing locked env => all platforms `outdated_conda` (`outdated.rs:271-284`)
- environment-level mismatch => all platforms `outdated_conda`; may set `disregard_locked_content` (`outdated.rs:287-360`)
- platform/package mismatch => `outdated_pypi` if `unsat.is_pypi_only()`, otherwise `outdated_conda` (`outdated.rs:383-421`)
- solve-group inconsistency => every env in group `outdated_conda` for platform (`outdated.rs:432-480`)

### What invalidates freshness/satisfiability

Environment-level invalidators (`verify_environment_satisfiability`):
- Channel URLs/order: exact match OK; appended lower-priority channels return `ChannelsExtended` (update lock metadata but do not disregard locked packages); removal/reorder/prepend invalidates conda locked content (`environment.rs:37-69`, `outdated.rs:312-333`).
- Platforms: extra platforms in lock, or changed platform identity (`subdir` or non-default declared virtual packages) invalidate conda (`environment.rs:71-177`).
- PyPI indexes: mismatch/missing where required invalidates PyPI content (`environment.rs:179-190`, `454-487`).
- PyPI `no-build` and wheel tag compatibility invalidate PyPI (`environment.rs:192-207`, `260-452`).
- Solve strategy, channel priority, PyPI prerelease mode, and `exclude-newer` mismatch invalidate (`environment.rs:210-258`). Most conda-level option mismatches cause `disregard_locked_content.conda`; PyPI option mismatches cause `.pypi` (`outdated.rs:319-350`).

Platform/package invalidators (`verify_platform_satisfiability`):
- Lock platform lookup falls back from rich platform name to bare subdir when virtual packages cover requested platform (`platform.rs:78-102`).
- Locked conda packages are converted via the shared `LockFileResolver`; pre-v7 source envs are reified; partial/mutable source records call the backend and verify build/host deps (`platform.rs:161-240`). Immutable full source records are trusted.
- Missing purls when PyPI deps exist invalidates conda/PyPI mapping (`platform.rs:273-289`).
- Constraints over locked binary packages are checked; source constraints are invalid (`platform.rs:694-735`).
- Dependency walk checks direct conda/source deps, transitive conda deps, PyPI deps (including conda packages that provide PyPI names), extras, markers, Python compatibility, and unused/too-many packages (`platform.rs:737-1125`).
- Solve-group satisfiability fails if a conda package is only present because it satisfied a PyPI requirement and is not expected by conda deps (`platform.rs:1456-1479`).

PyPI-specific invalidators:
- Name/version mismatch, direct URL mismatch, git URL/ref/subdirectory mismatch, index mismatch/removed/added semantics (`pypi.rs:117-360`).
- Local path directory metadata may be statically read and cached. If current `requires_dist`, version, or `requires_python` differs from lock, platform records delayed PyPI error (`pypi.rs:404-505`, `531-818`; `pypi_metadata.rs:51-110`; call site `platform.rs:1071-1110`). Missing/unparsable/dynamic static metadata often trusts the lock (`pypi.rs:748-787`).

### Update path derived state

`UpdateContextBuilder::finish` rebuilds derived maps from the input lock file:
- `locked_repodata_records`: per environment/platform unresolved conda records from the resolver (`update.rs:1836-1883`).
- `locked_pypi_records`: per environment/platform PyPI records (`update.rs:1885-1905`).
- `locked_grouped_repodata_records` and `locked_grouped_pypi_records`: merged per solve group, unless `disregard_locked_content` says to drop locked hints (`update.rs:1907-2018`).
- `pre_resolved_pypi_records`: metadata-enriched records from satisfiability, to avoid rereading source trees (`update.rs:2036-2045`).

`UpdateContext::update` builds a task graph:
- one conda solve per outdated group/platform, using locked group records as `installed` stability hints and `preferred_build_source` pins (`update.rs:2113-2216`, `2781-2977`);
- one PyPI solve per outdated group/platform, waiting on conda records, reusing uv context/build cache, locked PyPI records, and optional conda prefix (`update.rs:2219-2343`, `3298-3431`);
- extraction tasks that distill grouped solve results back into env/platform records (`update.rs:2345-2404`, `2979-3145`);
- final full lock-file rebuild from latest solved or existing records (`update.rs:2568-2665`).

The conda solve key identity includes dependencies, constraints, dev sources, installed unresolved records, installed source hints, solve strategy, preferred build-source pins, and `env_ref` (`solve_pixi_environment.rs:91-167`). Inside compute, it also resolves source packages, constructs source repodata, and calls `SolveCondaKey` with channels, VPs, channel priority, and exclude-newer (`solve_pixi_environment.rs:267-417`).

### What a daemon could keep warm

Safe/worthwhile warm candidates, provided invalidation is precise:
1. Parsed `LockFile` plus aligned platform names from `Workspace::load_lock_file` (`update.rs:426-470`). Invalidate on `pixi.lock` file content/mtime/hash change, lock format support change, workspace root change, or platform rename/manifest platform identity inputs changing.
2. `LockFileResolver` for that exact in-memory `LockFile` (`update.rs:656-665`; `lock_file_resolver.rs:24-120`). Important constraint: `LockFileResolver::get_for_package` uses pointer offsets into the same lock file package slice; invalidate on any new/cloned/rewritten `LockFile`, not just equivalent content.
3. `OutdatedEnvironments` result (conda/pypi env-platform sets, disregard flags, removed envs) for a tuple of workspace manifest/config/env-vars + lock file + host/platform info. Invalidate on any manifest/dependency/feature/solve-group/platform/channel/PyPI option/exclude-newer change, config affecting channels/auth/uv/pypi options, current host platform/env var override changes, lock file change, or source metadata changes for mutable/local sources.
4. `UvResolutionContext` (`outdated.rs:86-88`; `update.rs:2283-2289`) keyed by workspace config and authenticated client/proxy/TLS/cache settings. Invalidate on config/client/auth/proxy/TLS/cache options change.
5. `PypiEnvironmentBuildCache` by `(environment name, platform)`/host platform, including lazy build dispatch deps and optional `CondaPrefixUpdater` (`outdated.rs:37-60`; `pypi.rs:565-574`, `683-739`). Invalidate on environment deps, conda locked records/building records, platform/VPs, Python record, PyPI build options/indexes/link mode, project env vars, no-build/no-binary/no-build-isolation, or source tree changes.
6. Static local PyPI metadata cache by absolute path (`outdated.rs:94-96`; `pypi.rs:531-540`, `808-817`). Invalidate on any file under that local package that can affect `pyproject.toml`/metadata (at minimum `pyproject.toml` content/mtime; safer source-tree fingerprint if dynamic metadata is possible). Note the current cache is per invocation and platform-independent.
7. Command-dispatcher compute-engine results for `SolvePixiEnvironmentKey` / nested source resolution / `SolveCondaKey`, if the daemon owns a long-lived dispatcher. Invalidate by the full key identity: deps/constraints/dev sources/installed records/source hints/strategy/preferred build source/env ref plus environment spec data (channels, VPs, variants, exclude-newer, channel priority). Also invalidate lower-level network/source caches on channel repodata changes, source checkout changes, build backend metadata changes, credentials/config changes.
8. Final derived `LockFileDerivedData` after an update, including updated conda prefixes and PyPI prefixes (`update.rs:2668-2685`, install paths `770-843`, `845-893`). Invalidate prefix cache on lock environment hash mismatch, filter/reinstall mode, source packages present, target platform override, prefix marker file changes, or external prefix mutation.

## Architecture

Per invocation, Pixi does not simply compare lock-file timestamps. It parses the lock, builds a lock resolver, constructs a new command dispatcher/compute engine, then performs semantic satisfiability checks over every environment/platform. The checks classify stale state into conda vs PyPI env-platform sets. Conda staleness forces PyPI staleness for the same target, and solve-group membership broadens a single stale env to all envs in that solve group for affected platforms.

If stale and updates are allowed, `UpdateContext` converts the old lock into per-env and per-solve-group locked records. These records are either reused as final content (if not stale) or passed as stability hints to conda/PyPI solvers. The lock file is rebuilt wholesale from latest solved records plus still-satisfying locked records.

The current warm state is process-local: `GlobHashCache`, `CommandDispatcher`/compute engine, `LockFileResolver`, uv context, static metadata cache, PyPI build caches, and prefix `OnceCell`s are created or populated during a single `update_lock_file` call and then returned only through `LockFileDerivedData` for same-invocation install flows.

## Start Here

Start with `crates/pixi_core/src/lock_file/update.rs` at `Workspace::update_lock_file` (lines 223-413). It shows exactly what gets recreated per invocation and how the freshness result feeds the solve/update graph. Then open `crates/pixi_core/src/lock_file/outdated.rs` lines 67-217 to see the complete freshness state shape and propagation rules.
