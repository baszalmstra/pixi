# Pixi daemon follow-up handoff

## Purpose

This document captures the next milestone after `pixi_compute_engine` invalidation and the `pixi_compute_fs` crate exist.

The daemon milestone builds on those foundations. It should not be implemented until the filesystem graph crate and benchmarks are in place.

## Agreed daemon direction

- **Per-workspace daemon first.**
- **Request-driven computation.**
- **Immediate invalidation from watcher events.**
- **Lazy recomputation on client request.**
- **Missed watcher events invalidate all filesystem graph state.**
- **IPC/API shape is intentionally deferred until this milestone starts.**

## Why per-workspace

The hot state is workspace-specific:

- manifest/package graph
- lockfile freshness/satisfiability
- source metadata
- backend metadata
- task/environment/platform state
- `.pixi` state
- source roots and watched file sets

Per-workspace bounds memory, simplifies invalidation, and matches the Buck2/Bazel-style mental model better than a global daemon for the first production path.

## First daemon scope

The daemon should initially serve only the filesystem graph use case:

- Own one compute engine for the workspace.
- Own `pixi_compute_fs` state.
- Own filesystem watcher.
- Invalidate filesystem leaf keys immediately on watcher events.
- Recompute glob snapshot/freshness lazily when requested.
- Provide local fallback in the Pixi process if daemon is unavailable.

It should **not** initially own:

- `pixi run` execution
- shell/pty/session management
- lockfile updates
- activation env computation
- prefix install/validation
- full source metadata state
- broad CLI command execution

## Watcher model

Use a watcher abstraction so backends can evolve:

- default likely Rust `notify`
- optional Watchman later if needed
- possible polling/full-scan fallback for correctness

Watcher events should map to low-level compute invalidations:

```text
file modified
  -> invalidate PathMetadataKey(file)

file created/deleted/renamed
  -> invalidate ReadDirKey(parent)
  -> invalidate PathMetadataKey(file)

directory created/deleted/renamed
  -> invalidate ReadDirKey(parent)
  -> invalidate PathMetadataKey(directory)
```

If the watcher reports missed events, overflow, fresh instance, or otherwise uncertain state:

```text
invalidate all filesystem graph state for the workspace
keep daemon alive
recompute lazily on next request
```

This mirrors the correctness-first Buck2/Bazel fallback pattern.

## Request-driven behavior

The daemon should not eagerly recompute globs after invalidation.

```text
watcher event
  -> invalidate graph keys
  -> no recomputation

client request
  -> compute requested snapshot/freshness
  -> reuse valid graph state
  -> recompute only invalidated dependencies
```

This keeps idle behavior cheap and avoids doing work nobody asked for.

## IPC/API decision deferred

Do not decide the exact IPC shape until daemon implementation starts.

Questions to answer then:

- transport: Unix socket / Windows named pipe / loopback HTTP for debugging
- protocol: JSON-RPC, custom framed JSON, postcard/messagepack, gRPC, etc.
- request shape: snapshot payload vs snapshot id/generation
- status/diagnostics shape
- version/capability handshake
- fallback behavior on incompatible daemon

The only current agreement is that the first API should be narrow and local-only.

## CLI/process responsibilities in daemon milestone

The Pixi process should continue to own:

- CLI parsing/help/version
- workspace discovery sufficient to locate the per-workspace daemon
- current cwd/env/terminal context
- progress rendering policy
- child process execution
- signal forwarding
- fallback to existing local behavior

For the first daemon milestone, the Pixi process should call the daemon only for filesystem graph/glob freshness operations.

## Production path after first daemon slice

Once daemon-backed glob freshness works, the next likely integration point is source metadata freshness:

```text
BuildBackendMetadata freshness
  -> backend input globs
  -> daemon-backed GlobSnapshot/Freshness
```

Only after that proves useful should we consider moving more state into the daemon:

- lockfile parsed state
- `LockFileResolver`
- source/backend metadata entries
- environment readiness checks
- activation env computation
- broader `pixi run` prepare path

## Acceptance criteria for first daemon slice

- Per-workspace daemon starts/stops reliably in development mode.
- Daemon owns a `pixi_compute_fs` compute engine.
- Watcher events invalidate graph keys immediately.
- Missed events invalidate all fs graph state.
- Repeated no-change glob freshness requests are served warm.
- Local fallback exists and is easy to force.
- No user command execution happens inside the daemon.
- IPC/API decisions are documented when made.
