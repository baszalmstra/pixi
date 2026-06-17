# Code Context

## Files Retrieved
1. `crates/pixi_cli/src/run.rs` (lines 55-122, 127-483, 541-568, 570-613, 638-727) - CLI args, full `pixi run` pre-child flow, task execution boundary, interactive task disambiguation, signal/terminal ownership.
2. `crates/pixi_core/src/workspace/discovery.rs` (lines 70-78, 147-151, 203-310, 313-353) - `WorkspaceLocator` state, CLI discovery, env override handling.
3. `crates/pixi_core/src/workspace/mod.rs` (lines 146-191, 684-693, 918-924, 1068-1125) - `Workspace` fields, env selection, cache folders, activation env memoization.
4. `crates/pixi_core/src/workspace/environment.rs` (lines 101-150) - environment prefix dir and installed platform marker reads.
5. `crates/pixi_core/src/environment/mod.rs` (lines 37-65, 147-208, 431-448) - prefix relocation validation, activation hash inputs, workspace sanity check.
6. `crates/pixi_core/src/lock_file/update.rs` (lines 223-240, 720-814) - lock update entry point and prefix validation/install/environment marker write.
7. `crates/pixi_core/src/workspace/virtual_packages.rs` (lines 109-150, 373-435, 442-499) - current/target platform runnability and post-prefix validation.
8. `crates/pixi_cli/src/shared/install_platform.rs` (lines 1-26) - `--platform` alias resolution to workspace platform name.
9. `crates/pixi_cli/src/cli_config.rs` (lines 29-55, 162-195) - workspace discovery flags and lock/install behavior flags.
10. `crates/pixi_config/src/lib.rs` (lines 153-205, 865-883, 2263-2269) - global config source flags and activation cache/force activation config.
11. `crates/pixi_task/src/task_environment.rs` (lines 43-98, 101-149, 151-284) - task search environment state, platform choice for task lookup, ambiguity handling.
12. `crates/pixi_task/src/task_graph.rs` (lines 180-225, 226-310, 420-515, 644-670) - task graph structure, argv parsing, dependency graph expansion, topological order.
13. `crates/pixi_task/src/executable_task.rs` (lines 93-119, 140-217, 219-234, 530-571) - executable task materialization, render context, deno shell script parsing, cwd, command env creation.
14. `crates/pixi_core/src/activation.rs` (lines 37-49, 120-199, 291-350, 350-430, 506-545) - activation cache, activator construction, activation script execution, env merging.

## Key Code

### `pixi run` pre-child execution flow
`crates/pixi_cli/src/run.rs` is the primary entry. Before a child is executed, `execute(args)`:

```rust
let cli_config = args.activation_config.merge_config(args.config.clone().into());
let workspace = WorkspaceLocator::for_cli()
    .with_global_config_source(args.config_source.source())
    .with_search_start(args.workspace_config.workspace_locator_start())
    .locate()?
    .with_cli_config(cli_config);
let environment = workspace.environment_from_name_or_env_var(args.environment.clone())?;
```
(lines 134-146)

Then it validates prefix location, resolves run platform, updates/loads lock data, builds the task graph, and for each executable task validates/install prefix and computes activation env before `execute_task` calls `deno_task_shell::execute` (lines 166-279, 301-455, 541-568).

### State inventory by category

#### Disk-authoritative
- Workspace manifests: discovered by `WorkspaceLocator::locate` through `pixi_manifest::WorkspaceDiscoverer`, then loaded into `Workspace::from_manifests` (`workspace/discovery.rs` lines 231-308). Includes `pixi.toml`, `pyproject.toml`, current package manifest, tasks, environments, platform declarations, activation scripts/env declarations.
- Project-local config and global config files: selected by `ConfigSourceCli` (`pixi_config/src/lib.rs` lines 153-205) and merged into `Workspace.config` (`run.rs` lines 134-143). Project-local `.pixi/config.toml` remains loaded even when global config is disabled per comments in `ConfigSourceCli`.
- Lock file: `workspace.update_lock_file(...)` loads and validates/upgrades `pixi.lock` (`lock_file/update.rs` lines 223-240; `run.rs` lines 226-238). Effective lock usage is controlled by `LockAndInstallConfig::lock_file_usage` (`cli_config.rs` lines 180-189).
- Prefix directories: each environment prefix is `workspace.environments_dir()/environment.name` (`workspace/environment.rs` lines 101-106). `sanity_check_workspace` checks `.pixi/envs/conda-meta/prefix` and `.pixi` setup (`environment/mod.rs` lines 431-448).
- Prefix relocation marker: `verify_prefix_location_unchanged` reads `conda-meta/prefix`; if it contains an old path, interactive repair may delete/recreate (`environment/mod.rs` lines 37-65, 71-90).
- Prefix installed platform marker: `Environment::installed_platforms` reads `conda-meta/pixi` and returns `(resolved_platform, minimum_supported_platform)` (`workspace/environment.rs` lines 108-120). `LockFileDerivedData::prefix` writes these markers after install/update (`lock_file/update.rs` lines 800-814).
- Environment fingerprint and activation cache: activation cache stores an `EnvironmentHash` plus env vars (`activation.rs` lines 37-49). Cache validity depends on prefix fingerprint (`activation.rs` lines 291-315, 388-430; `environment/mod.rs` lines 147-208). Cache files live under `workspace.activation_env_cache_folder()` (`workspace/mod.rs` lines 922-924).
- Task cache/filesystem hashes: before child, `can_skip` may skip based on cache (in `run.rs` lines 376-400), and filesystem caches are cleared before activation because tasks may mutate disk (`run.rs` lines 429-434). Details are in `pixi_task` cache/hash code not deeply retrieved.

#### Derived in-memory
- `Workspace`: root, workspace/package manifests, activation env memoization cells, config, repodata/client lazies (`workspace/mod.rs` lines 146-191). It is derived from disk manifests plus config sources.
- Selected environment: `environment_from_name_or_env_var` resolves CLI/env name to `Environment` (`workspace/mod.rs` lines 684-693; `run.rs` line 146).
- `explicit_environment`: derived from whether CLI/env selected non-default environment (`run.rs` lines 148-153); changes task search scope.
- Platform decision: `user_platform` from `--platform`, `installed_platform` from prefix marker, `run_platform = user_platform.or(installed_platform)`, and `best_declared_platform` from environment membership (`run.rs` lines 169-204). `lock_file.target_platform` is set in memory before prefix install (`run.rs` lines 240-242).
- `LockFileDerivedData`: returned by `update_lock_file`, then mutated to set `target_platform` (`run.rs` lines 226-242). Contains lock file plus command dispatcher/caches used by prefix/activation paths.
- Task search state: `SearchEnvironments { project, explicit_environment, platform, disambiguate }` (`task_environment.rs` lines 43-98). If no explicit search platform, task lookup uses installed platform, best declared platform, or first declared platform (`task_environment.rs` lines 127-149, 166-173).
- Task graph: `TaskGraph { project, nodes }` (`task_graph.rs` lines 180-225), built from CLI argv. It parses single-string input with `shlex`, resolves named tasks vs executable fallback, merges typed/free args, expands `depends-on`, and returns topological order (`task_graph.rs` lines 226-310, 420-515, 644-670).
- `ExecutableTask`: materialized per graph node with workspace, task/name, run environment, args, and initial cwd (`executable_task.rs` lines 93-119). It renders command with platform/env/name/manifest/init cwd context and parses to deno shell AST before execution (`executable_task.rs` lines 140-217).
- Command env map: computed lazily per unique run environment and stored in `task_envs` (`run.rs` lines 402-445). Converted to `HashMap<OsString, OsString>` immediately before child execution (`run.rs` lines 447-450).

#### Env/config-sensitive
- Workspace discovery env: `WorkspaceLocator::for_cli()` considers environment overrides (`discovery.rs` lines 147-151). `PIXI_PROJECT_MANIFEST` can supply a manifest when no local workspace is found; `PIXI_IN_SHELL` triggers warning when it conflicts with local discovery (`discovery.rs` lines 313-353).
- Discovery flags: `--manifest-path`/`--workspace` determine `DiscoveryStart` (`cli_config.rs` lines 29-55). `--no-config` / `--config-file` select global config source and can also be env-driven via `PIXI_NO_CONFIG` / `PIXI_CONFIG_FILE` (`pixi_config/src/lib.rs` lines 170-205).
- Environment selection: `--environment` or `EnvironmentName::from_arg_or_env_var` (called at `workspace/mod.rs` lines 684-689) can pick the run environment.
- Lock/install flags: `--as-is`, `--no-install`, `--frozen`/`--locked` influence whether lock can update and whether prefix install/validation happens (`cli_config.rs` lines 162-195; `run.rs` lines 208-219, 409-427).
- Platform overrides: `--platform` is resolved via `resolve_install_platform` (`install_platform.rs` lines 13-26). `PIXI_OVERRIDE_PLATFORM` skips current-platform validation (`virtual_packages.rs` lines 113-117, 459-463). `CONDA_OVERRIDE_*` and `PIXI_OVERRIDE_PLATFORM` are honored in host platform calculations where `PlatformOverrides::EnvironmentVariableOverrides` is used (`run.rs` lines 189-192; `virtual_packages.rs` lines 398-417, 465-484).
- Activation behavior: `--clean-env` switches `CurrentEnvVarBehavior` to `Clean` in `get_task_env` (`executable_task.rs` lines 532-545). Task-level `clean_env()` is ORed with CLI (`run.rs` line 437). `--force-activate` and experimental activation cache config are passed from `workspace.config()` (`run.rs` lines 435-441; `pixi_config/src/lib.rs` lines 865-883, 2263-2269).
- Activation env content: `initialize_env_variables` includes current shell env, clean env, or excludes it, then chains activation env so activation overrides shell vars (`activation.rs` lines 506-545). `run_activation` captures `std::env::vars()` while running scripts (`activation.rs` lines 334-351).
- Reproducibility/env side note: prefix update checks `SOURCE_DATE_EPOCH` for mtime clamping after install (`lock_file/update.rs` lines 816-825), which may affect disk state before run if install occurs.

#### Process/terminal-owned
- Current working directory: workspace discovery defaults to `std::env::current_dir()` (`discovery.rs` lines 203-214). `init_cwd` is captured before iterating tasks (`run.rs` line 301) and passed into task render context (`executable_task.rs` lines 140-150). `INIT_CWD` env var is inserted from current dir at activation-env creation time (`executable_task.rs` lines 559-567). Task working dir defaults to workspace root or task `cwd` (`executable_task.rs` lines 219-234).
- Progress/terminal output: global progress draw target is hidden during early discovery and restored before install/task phases (`run.rs` lines 127-165); top-level progress can be cleared before activation (`run.rs` lines 223-224, 429-430).
- Interactive prompts: ambiguous task selection uses `dialoguer::Select` (`run.rs` lines 570-613). Prefix relocation repair uses `dialoguer::Confirm` (`environment/mod.rs` lines 71-90). These own terminal state and can block.
- Signals: child execution uses `KillSignal` with drop guard (`run.rs` lines 296-299). `execute_task` calls `run_future_forwarding_signals` around `deno_task_shell::execute` (`run.rs` lines 553-562). Unix listener forwards signals except SIGKILL/SIGSTOP and interactive SIGINT (`run.rs` lines 638-727). A ctrl-c task attempts cursor reset (`run.rs` lines 244-249, 622-625).

### Prefix validation/install gates before child
1. Early workspace sanity: `sanity_check_workspace(&workspace)` before lock update/task graph (`run.rs` lines 166-167; `environment/mod.rs` lines 431-448).
2. Runnability before announcement: for executable tasks, if installs are allowed and no explicit `--platform`, `classify_environment_runnability(..., lock_file)` can abort unsupported environments (`run.rs` lines 312-333; `virtual_packages.rs` lines 442-499).
3. Prefix quick validation/install: first time a run environment is needed, `lock_file.prefix(environment, UpdateMode::QuickValidate, ...)` validates hash/cached prefix or updates the prefix and writes marker (`run.rs` lines 405-419; `lock_file/update.rs` lines 770-814).
4. Post-prefix platform validation: `verify_run_platform(environment, user_platform.as_ref())` compares requested/host capabilities with installed `(resolved, minimum)` markers (`run.rs` lines 420-427; `virtual_packages.rs` lines 373-435).
5. Activation env computation happens only after prefix validation and before child (`run.rs` lines 435-443; `executable_task.rs` lines 532-571).

## Architecture

`pixi run` is a pipeline:

1. CLI args and config sources are parsed into `Args` (`run.rs` lines 55-122).
2. `WorkspaceLocator` discovers/loads manifests from disk or registry/environment and creates a `Workspace` (`discovery.rs` lines 203-310). CLI activation config is layered onto workspace config (`run.rs` lines 134-143).
3. The requested environment and run platform are selected from CLI/env/prefix marker/declared platform state (`run.rs` lines 145-204).
4. Workspace/prefix location is sanity-checked, lock file is loaded/updated, and run platform is pinned into lock-derived data (`run.rs` lines 166-242).
5. `SearchEnvironments` and `TaskGraph` resolve CLI input into a graph of tasks and dependency tasks across environments/platform-specific task tables (`run.rs` lines 251-280; `task_environment.rs`; `task_graph.rs`).
6. Each executable node is converted to `ExecutableTask`, checked for skip/cache/runnability, then its environment is prefix-validated/installed and activated exactly once per run environment (`run.rs` lines 301-445).
7. The child boundary is `execute_task`: deno shell AST + command env + task cwd + kill signal are passed to `deno_task_shell::execute` (`run.rs` lines 541-568). Everything before this is parent-owned state preparation.

## Start Here

Start with `crates/pixi_cli/src/run.rs` lines 127-483. It contains the exact sequencing of all state reads/derivations before any child process is spawned, and every secondary subsystem (workspace discovery, lock/prefix, task graph, activation env, terminal/signal handling) is called from there.
