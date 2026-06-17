# Pixi daemon: prototype-to-production planning frame

This frame expands `design/pixi-daemon.md` into a product/architecture planning scaffold. It intentionally stays above detailed implementation design: the goal is to decide what the daemon is for, where its boundary sits, how to stage it from useful prototype to production, and what decisions need user/team pressure-testing.

## Problem statement

Pixi currently rebuilds command state from disk for each invocation: workspace discovery, manifest/lockfile consistency, environment state, source package metadata, and build/source change checks. This is acceptable for small projects but becomes noticeable in large repositories and does not scale to workspaces with hundreds of packages. The daemon hypothesis is:

> Keep expensive, frequently reused workspace and dependency/build state warm in a local process; invalidate it when inputs change; let CLI invocations use that process as an accelerator while preserving daemonless correctness.

The daemon should not initially be a new source of truth. Disk remains authoritative: manifests, lockfiles, prefixes, caches, and file locks must remain valid if the daemon is killed or bypassed.

## Core use cases to optimize first

1. **Fast repeated workspace commands**
   - Typical loop: `pixi run test`, `pixi run lint`, `pixi install`, or editor-triggered checks in the same large workspace.
   - Value: avoid re-parsing/re-validating/reconstructing all state on every command.

2. **Large monorepo / many-package source workflows**
   - Reuse source package metadata, backend metadata, dependency graph knowledge, and file-change observations.
   - Value: make Pixi viable for much larger repositories without every CLI command paying the full scan/rebuild cost.

3. **Environment readiness as a service**
   - “Ensure env X for platform Y is ready” for CLI, editor, shell hook, or background tooling.
   - Value: coalesce identical work and serialize unsafe mutations around the same prefix/workspace.

4. **Workspace introspection for tools**
   - List environments, tasks, packages, platforms, lock status, active operations, and cached state.
   - Value: editors and integrations can ask Pixi for structured state without repeatedly invoking full CLI paths.

5. **Diagnostics/control**
   - Users need to answer: is the daemon running, what workspace state is cached, why did it invalidate, what is it doing, and how do I restart it?

## Daemon roles

### Roles that belong in early scope

- **Workspace state cache**: retain parsed/derived workspace state keyed by workspace root and relevant configuration/env fingerprints.
- **Incremental invalidation coordinator**: watch or cheaply re-check files that affect derived state: `pixi.toml`, lockfile, local/global config, source package inputs, and relevant environment variables.
- **Command dispatcher host**: keep warm the expensive resources Pixi already builds per command, especially compute-engine-backed dispatcher state, gateways, caches, source/build metadata, and resolver resources.
- **Operation coordinator**: de-duplicate identical in-flight work and serialize unsafe mutations such as prefix installs or lockfile writes.
- **Progress/status source**: expose active operations, recent invalidations, daemon health, version/protocol info, logs, and cache-hit/miss style diagnostics.

### Roles to defer or treat as non-goals initially

- **Privileged system service**: no need unless Pixi introduces a cross-user shared store requiring privilege separation.
- **Remote daemon**: initial daemon should be local-only and user-scoped.
- **Daemon-owned terminal/session manager**: keep `run`, interactive `shell`, prompts, signal handling, and child exit-code semantics in the CLI until there is a specific product reason to move them.
- **New global database as source of truth**: memory is an accelerator; disk remains authoritative.
- **Solver/build algorithm redesign**: the daemon should first reuse existing Pixi abstractions rather than become a parallel implementation.

## Architecture boundary

Recommended boundary:

> CLI owns user interaction and process execution. Daemon owns reusable workspace/dispatcher state and long-running Pixi operations behind a local RPC boundary.

### CLI should continue to own

- Argument parsing, help text, config flags, color/progress policy, terminal detection, and exact CLI output/exit-code contract.
- Spawning and attaching to user processes for `pixi run` and interactive `pixi shell`.
- Deciding whether a command is allowed to mutate files based on flags like locked/frozen/no-install modes.
- Fallback: if daemon is absent, incompatible, unhealthy, or disabled, the command should run through the existing in-process path.

### Daemon should own

- Per-workspace sessions keyed by canonical workspace root plus config/env/toolchain-relevant fingerprints.
- Warm command-dispatcher/compute-engine resources where safe.
- File/config/env invalidation policy and reasons.
- Operation lifecycle: start, progress, cancellation, completion, failure, and query by operation id.
- Safe concurrency: coalescing read/prepare requests, serializing lockfile/prefix mutations, and respecting existing file locks for mixed daemon/non-daemon Pixi processes.

### Process model frame

A practical production direction is **one user-scoped local daemon with per-workspace sessions**:

- One process avoids spawning many daemons for users with several workspaces.
- Internal workspace sessions preserve isolation and allow eviction/idle timeout.
- Local-only IPC avoids exposing credentials or filesystem operations over the network.
- Per-workspace daemon remains an option if isolation/lifecycle proves simpler, but it adds process management overhead.

External precedent points in this direction: Buck2 uses project daemon state for build speed, Bazel workers amortize process/tool state, LSP-style services use capability negotiation/lifecycle messages, and package managers like uv emphasize disk-backed caches plus explicit refresh controls.

## Prototype-to-production milestones

### Milestone 0: Meaningful prototype definition

Goal: prove the daemon can keep state warm and answer one real Pixi workflow faster without changing user-visible outcomes.

Prototype should include:
- local daemon process startup and shutdown;
- simple client handshake with version/protocol info;
- one workspace session;
- one high-value request, preferably repeated workspace readiness or read-only workspace summary;
- a measured before/after on a large workspace;
- daemonless fallback path still working.

Avoid overbuilding: no broad stable API, no remote support, no daemon-owned shell execution.

### Milestone 1: Control plane and lifecycle

Add the minimum production-shaped skeleton:
- `status`, `stop`/`restart`, health, pid, uptime, version/protocol, log location;
- startup readiness timeout and stale socket cleanup;
- incompatible daemon detection and restart/fallback behavior;
- local-only transport with user-private access expectations.

Exit criteria: users and developers can reliably see, stop, and debug the daemon.

### Milestone 2: Workspace read/introspection API

Move low-risk, request/response operations behind the daemon:
- resolve/open workspace;
- summarize environments, tasks, platforms, packages, lock status;
- expose invalidation reasons when manifest/config/lock inputs change.

Exit criteria: an editor-like client can query Pixi state repeatedly with warm-session benefit and correct refresh after edits.

### Milestone 3: Operation framework

Introduce long-running operations before moving install/solve work:
- operation ids;
- progress/event stream;
- cancellation;
- structured errors;
- per-workspace operation registry;
- client disconnect policy.

Exit criteria: long tasks can be observed, cancelled, and diagnosed without blocking the daemon globally.

### Milestone 4: Ensure environment / repeated run acceleration

Wrap the existing lock/update/install preparation path for one environment/platform.

Key principles:
- preserve current lockfile and prefix semantics;
- keep child process execution in CLI;
- coalesce identical environment-prepare requests;
- report lock contention and progress clearly;
- allow `pixi run` to ask daemon for “environment ready + activation data”, then execute locally.

Exit criteria: repeated `pixi run` or `pixi install` in a large workspace is measurably faster, with identical lock/prefix outcomes to daemonless Pixi.

### Milestone 5: Source/build metadata warming

Extend the daemon to the workflows most likely to hurt in huge workspaces:
- source package metadata freshness;
- backend metadata cache warming;
- file-change-triggered invalidation;
- optional precomputation after workspace edits.

Exit criteria: source-heavy workspaces see reduced repeated command latency and clear “why did this rebuild/recompute?” diagnostics.

### Milestone 6: Opt-in beta

Gate daemon usage behind an experimental config/env flag or explicit command option.

Beta requirements:
- docs for enabling/disabling;
- fallback and `--no-daemon` escape hatch;
- status/log troubleshooting;
- performance telemetry/benchmarks if acceptable;
- stress tests for concurrent clients and mixed daemon/non-daemon Pixi.

### Milestone 7: Production default decision

Only consider default-on after gates are met:
- correctness parity across representative install/run/shell-hook/list workflows;
- no prefix/cache corruption under concurrency;
- robust crash/restart/upgrade behavior;
- user-private transport and credential-safe logging;
- demonstrated cold-path non-regression and warm-path improvement;
- clear support story for Windows/macOS/Linux.

## Key decisions to grill the user/team on

1. **Primary success metric**: which command/workflow must become faster first: repeated `pixi run`, `install`, source builds, editor introspection, or `exec`?
2. **Daemon scope**: one user daemon with many workspace sessions, one daemon per workspace, or a hybrid?
3. **Freshness model**: exact file watching, per-request fingerprint checks, TTLs, manual refresh, or a combination?
4. **Fallback policy**: should daemon use be transparent and best-effort, or should users opt into daemon-only failure behavior for debugging?
5. **API consumers**: is the daemon only for the Pixi CLI initially, or must editor/tooling clients be supported from the start?
6. **Execution boundary**: should the daemon ever run user commands, or only prepare environments and return activation data?
7. **Mutation authority**: which operations may the daemon perform automatically: lockfile updates, prefix installs, dependency edits, source rebuilds?
8. **Auth/config refresh**: how should daemon react to `pixi auth`, config changes, proxy/cert changes, and env var changes?
9. **Transport choice**: Unix socket / Windows named pipe as production default, with localhost HTTP only for debugging?
10. **Upgrade behavior**: should a new Pixi client kill/restart an older daemon automatically, or fall back until user restarts?
11. **Observability bar**: what status/log/cache diagnostics are mandatory before beta?
12. **Privacy/security bar**: what credential redaction, socket permissions, and same-user checks are required before any default-on rollout?

## Planning guardrails

- Treat daemon memory as disposable. Killing the daemon must not corrupt or lose authoritative Pixi state.
- Preserve the current CLI contract first; acceleration is secondary to correctness.
- Reuse existing dispatcher/compute/cache/file-lock abstractions where possible.
- Be conservative with invalidation until there is evidence that precision is needed.
- Keep initial API narrow and versioned; expand only after real clients and performance data validate the boundary.
