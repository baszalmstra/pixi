//! Offline characterization tests that pin pixi's current freshness
//! guarantees ("no-op invariants").
//!
//! The design in `design/unified-freshness-fast-path.md` introduces an
//! "inputs unchanged" fast path across the lock-file / prefix / task-cache
//! layers. These tests pin the behavior that exists TODAY so that any fast
//! path introduced later must preserve it — a failure here after such a
//! change indicates a real regression, not a stale test:
//!
//! (a) a second `pixi install` with unchanged inputs is a no-op, observable
//!     through stable on-disk state,
//! (b) editing the manifest re-solves the lock file, and the follow-up
//!     install re-stamps the `conda-meta/pixi` marker,
//! (c) deleting or editing `pixi.lock` is detected and the lock is
//!     regenerated (while unreadable garbage is a hard error, not silently
//!     reused),
//! (d) repeated `pixi run` of the same task succeeds and reuses the
//!     installed prefix,
//! (e) the task cache round-trips: miss -> save -> hit -> input edit -> miss.
//!
//! Everything runs offline against a local mock channel whose packages are
//! materialized as real `.conda` archives, so installs actually populate a
//! prefix.

use std::path::PathBuf;

use pixi_cli::run;
use pixi_consts::consts;
use pixi_manifest::FeatureName;
use pixi_task::{CanSkip, ExecutableTask, PreferExecutable, SearchEnvironments, TaskGraph};
use pixi_test_utils::{LocalChannel, MockRepoData, Package};
use pixi_utils::EnvironmentFingerprint;
use rattler_conda_types::Platform;

use crate::common::{LockFileExt, PixiControl, string_from_iter};
use crate::setup_tracing;

/// A local channel with three materialized packages for the current platform:
/// `foo` (which depends on `bar`), `bar`, and the independent `baz`.
/// Materialization means real `.conda` archives exist in the channel, so
/// `pixi install` works without any network access.
async fn materialized_channel() -> LocalChannel {
    let platform = Platform::current();
    let mut db = MockRepoData::default();
    db.add_package(
        Package::build("bar", "1.0.0")
            .with_subdir(platform)
            .with_materialize(true)
            .finish(),
    );
    db.add_package(
        Package::build("foo", "1.0.0")
            .with_dependency("bar >=1")
            .with_subdir(platform)
            .with_materialize(true)
            .finish(),
    );
    db.add_package(
        Package::build("baz", "1.0.0")
            .with_subdir(platform)
            .with_materialize(true)
            .finish(),
    );
    db.into_channel().await.unwrap()
}

/// A minimal manifest that depends on `foo` from the mock channel. `extra` is
/// appended verbatim (e.g. a `[tasks]` section).
fn manifest(channel: &LocalChannel, extra: &str) -> String {
    format!(
        r#"
[workspace]
name = "freshness-invariants"
channels = ["{channel}"]
platforms = ["{platform}"]

[dependencies]
foo = "*"
{extra}"#,
        channel = channel.url(),
        platform = Platform::current(),
    )
}

/// Path to the `conda-meta/pixi` marker (the `EnvironmentFile`) of the
/// default environment.
fn environment_file_path(pixi: &PixiControl) -> PathBuf {
    pixi.default_env_path()
        .unwrap()
        .join("conda-meta")
        .join(consts::ENVIRONMENT_FILE_NAME)
}

/// Reads the `environment_lock_file_hash` field from `conda-meta/pixi`. The
/// `EnvironmentFile` struct is `pub(crate)` to `pixi_core`, so parse the JSON
/// untyped.
fn environment_lock_file_hash(pixi: &PixiControl) -> serde_json::Value {
    let contents = fs_err::read_to_string(environment_file_path(pixi)).unwrap();
    let value: serde_json::Value = serde_json::from_str(&contents).unwrap();
    value
        .get("environment_lock_file_hash")
        .cloned()
        .expect("conda-meta/pixi must contain the environment_lock_file_hash field")
}

/// The sorted file names directly under `<default env>/conda-meta`.
fn conda_meta_file_names(pixi: &PixiControl) -> Vec<String> {
    let conda_meta = pixi.default_env_path().unwrap().join("conda-meta");
    let mut names: Vec<String> = fs_err::read_dir(conda_meta)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Invariant (a): a second `pixi install` with unchanged inputs is a no-op.
///
/// After the second install the `conda-meta/pixi` marker still exists, its
/// `environment_lock_file_hash` and the install fingerprint are unchanged,
/// and `pixi.lock` is byte-for-byte identical (an up-to-date lock file is not
/// rewritten).
#[tokio::test]
async fn test_second_install_is_a_noop() {
    setup_tracing();

    let channel = materialized_channel().await;
    let pixi = PixiControl::from_manifest(&manifest(&channel, "")).unwrap();

    pixi.install().await.unwrap();

    let marker_path = environment_file_path(&pixi);
    assert!(
        marker_path.is_file(),
        "the first install must write conda-meta/pixi"
    );
    let hash_before = environment_lock_file_hash(&pixi);
    assert!(
        hash_before.is_string(),
        "the marker stores the lock file hash as a string"
    );
    let fingerprint_before = EnvironmentFingerprint::read(&pixi.default_env_path().unwrap())
        .expect("the first install must record a valid install fingerprint");
    let lock_path = pixi.workspace_path().join(consts::PROJECT_LOCK_FILE);
    let lock_before = fs_err::read(&lock_path).unwrap();

    // Second install: nothing changed in between.
    pixi.install().await.unwrap();

    assert!(
        marker_path.is_file(),
        "conda-meta/pixi must still exist after a no-op install"
    );
    assert_eq!(
        environment_lock_file_hash(&pixi),
        hash_before,
        "a no-op install must not change the recorded lock file hash"
    );
    assert_eq!(
        EnvironmentFingerprint::read(&pixi.default_env_path().unwrap()),
        Some(fingerprint_before),
        "a no-op install must not change the install fingerprint"
    );
    assert_eq!(
        fs_err::read(&lock_path).unwrap(),
        lock_before,
        "an up-to-date pixi.lock must not be rewritten"
    );
}

/// Invariant (b): editing the manifest (adding a dependency) re-solves the
/// lock file, and the follow-up install re-stamps `conda-meta/pixi` with a
/// different `environment_lock_file_hash`.
#[tokio::test]
async fn test_manifest_edit_triggers_resolve() {
    setup_tracing();

    let channel = materialized_channel().await;
    let pixi = PixiControl::from_manifest(&manifest(&channel, "")).unwrap();

    pixi.install().await.unwrap();
    let hash_before = environment_lock_file_hash(&pixi);

    let lock = pixi.lock_file().await.unwrap();
    assert!(
        !lock.contains_match_spec(consts::DEFAULT_ENVIRONMENT_NAME, Platform::current(), "baz"),
        "baz must not be locked before the manifest edit"
    );

    // Edit the manifest through `pixi add`. `baz` is independent of `foo`, so
    // the locked package set is guaranteed to change. (The add builder does
    // not install by default.)
    pixi.add("baz").await.unwrap();

    let lock = pixi.lock_file().await.unwrap();
    assert!(
        lock.contains_match_spec(
            consts::DEFAULT_ENVIRONMENT_NAME,
            Platform::current(),
            "baz ==1.0.0"
        ),
        "adding a dependency must re-solve the lock file"
    );
    // The prefix has not been re-installed yet, so the marker still holds the
    // hash of the previous install.
    assert_eq!(
        environment_lock_file_hash(&pixi),
        hash_before,
        "without an install the prefix marker is untouched"
    );

    pixi.install().await.unwrap();
    assert_ne!(
        environment_lock_file_hash(&pixi),
        hash_before,
        "the follow-up install must stamp the new lock file hash into conda-meta/pixi"
    );
}

/// Invariant (c1): deleting `pixi.lock` is detected; the next lock-file
/// update regenerates a valid, satisfiable lock containing the expected
/// packages, and persists it to disk.
#[tokio::test]
async fn test_deleted_lock_file_is_regenerated() {
    setup_tracing();

    let channel = materialized_channel().await;
    let pixi = PixiControl::from_manifest(&manifest(&channel, "")).unwrap();

    let lock = pixi.update_lock_file().await.unwrap();
    assert!(lock.contains_match_spec(
        consts::DEFAULT_ENVIRONMENT_NAME,
        Platform::current(),
        "foo ==1.0.0"
    ));
    assert!(lock.contains_match_spec(
        consts::DEFAULT_ENVIRONMENT_NAME,
        Platform::current(),
        "bar ==1.0.0"
    ));

    let lock_path = pixi.workspace_path().join(consts::PROJECT_LOCK_FILE);
    fs_err::remove_file(&lock_path).unwrap();

    pixi.update_lock_file().await.unwrap();

    assert!(
        lock_path.is_file(),
        "the regenerated lock file must be written back to disk"
    );
    // Read back from disk to pin persistence, not just the in-memory result.
    let lock = pixi.lock_file().await.unwrap();
    assert!(
        lock.contains_match_spec(
            consts::DEFAULT_ENVIRONMENT_NAME,
            Platform::current(),
            "foo ==1.0.0"
        ),
        "the regenerated lock must contain foo again"
    );
    assert!(
        lock.contains_match_spec(
            consts::DEFAULT_ENVIRONMENT_NAME,
            Platform::current(),
            "bar ==1.0.0"
        ),
        "the regenerated lock must contain foo's dependency bar again"
    );
}

/// Invariant (c2): editing the locked package list of an environment (here:
/// dropping `bar`, a dependency of `foo`) makes the lock unsatisfiable; the
/// next lock-file update detects this and re-solves, restoring `bar`.
#[tokio::test]
async fn test_edited_lock_file_is_detected() {
    setup_tracing();

    let channel = materialized_channel().await;
    let pixi = PixiControl::from_manifest(&manifest(&channel, "")).unwrap();
    pixi.update_lock_file().await.unwrap();

    // Remove `bar` from the default environment's package list while keeping
    // the YAML parseable: environment package entries are indented
    // `- conda: <url>` lines, unlike the top-level `packages:` metadata
    // entries which start at column 0 (those may remain as orphans).
    let lock_path = pixi.workspace_path().join(consts::PROJECT_LOCK_FILE);
    let original = fs_err::read_to_string(&lock_path).unwrap();
    let edited: String = original
        .lines()
        .filter(|line| {
            !(line.starts_with(char::is_whitespace)
                && line.trim_start().starts_with("- conda:")
                && line.contains("bar-1.0.0"))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(
        original.lines().count() - edited.lines().count(),
        1,
        "exactly one line (bar's environment package entry) must have been removed"
    );
    fs_err::write(&lock_path, &edited).unwrap();

    // Sanity check: the edited lock still parses but no longer contains bar.
    let lock = pixi.lock_file().await.unwrap();
    assert!(
        !lock.contains_conda_package(consts::DEFAULT_ENVIRONMENT_NAME, Platform::current(), "bar"),
        "the edited lock must no longer contain bar"
    );

    pixi.update_lock_file().await.unwrap();

    let lock = pixi.lock_file().await.unwrap();
    assert!(
        lock.contains_match_spec(
            consts::DEFAULT_ENVIRONMENT_NAME,
            Platform::current(),
            "foo ==1.0.0"
        ),
        "foo must still be locked after the re-solve"
    );
    assert!(
        lock.contains_match_spec(
            consts::DEFAULT_ENVIRONMENT_NAME,
            Platform::current(),
            "bar ==1.0.0"
        ),
        "the re-solve must restore foo's dependency bar"
    );
}

/// Invariant (c3): an unreadable `pixi.lock` is a hard error today — pixi
/// refuses to continue rather than silently regenerating or reusing state.
/// Any fast path must keep failing loudly here; it must never treat a corrupt
/// lock file as fresh.
#[tokio::test]
async fn test_corrupt_lock_file_is_an_error() {
    setup_tracing();

    let channel = materialized_channel().await;
    let pixi = PixiControl::from_manifest(&manifest(&channel, "")).unwrap();
    pixi.update_lock_file().await.unwrap();

    let lock_path = pixi.workspace_path().join(consts::PROJECT_LOCK_FILE);
    fs_err::write(&lock_path, "this is not a valid lock file").unwrap();

    assert!(
        pixi.update_lock_file().await.is_err(),
        "a corrupt pixi.lock is a hard error today, not silently regenerated"
    );
}

/// Invariant (d): repeated `pixi run` of the same task succeeds and reuses
/// the installed prefix: the set of files under `conda-meta/` and the
/// recorded lock-file hash are stable between runs.
///
/// Note: the test harness drives `prefix()` with `UpdateMode::Revalidate`
/// (like `pixi install`), so this pins marker/file stability rather than the
/// `UpdateMode::QuickValidate` short-circuit that `pixi run` proper uses.
#[tokio::test]
async fn test_repeated_run_reuses_prefix() {
    setup_tracing();

    let channel = materialized_channel().await;
    let pixi = PixiControl::from_manifest(&manifest(&channel, "")).unwrap();
    pixi.tasks()
        .add("hello".into(), None, FeatureName::default())
        .with_commands(["echo hello"])
        .execute()
        .await
        .unwrap();

    let run_args = || run::Args {
        task: string_from_iter(["hello"]),
        ..Default::default()
    };

    let output = pixi.run(run_args()).await.unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.contains("hello"));

    let files_before = conda_meta_file_names(&pixi);
    assert!(
        files_before
            .iter()
            .any(|name| name == consts::ENVIRONMENT_FILE_NAME),
        "the first run must have installed the prefix and written conda-meta/pixi"
    );
    let hash_before = environment_lock_file_hash(&pixi);

    let output = pixi.run(run_args()).await.unwrap();
    assert_eq!(output.exit_code, 0, "the second run must succeed");
    assert!(
        output.stdout.contains("hello"),
        "the second run must execute the task again"
    );

    assert_eq!(
        conda_meta_file_names(&pixi),
        files_before,
        "a second run must not add or remove files under conda-meta/"
    );
    assert_eq!(
        environment_lock_file_hash(&pixi),
        hash_before,
        "a second run must leave the recorded lock-file hash unchanged"
    );
}

/// Invariant (e): the task cache round-trips against today's lock-file-keyed
/// task hash: a task with declared `inputs` misses before any cache entry
/// exists, hits after `save_cache`, still hits after an mtime-only rewrite of
/// an input (input hashes are content-based), and misses again once an input
/// file's content changes.
///
/// Precondition pinned deliberately: the cache is exercised after a REAL
/// offline install, so the install fingerprint marker
/// (`conda-meta/.pixi-environment-fingerprint`) exists. The unified-freshness
/// design re-keys the task hash onto that marker; this precondition keeps the
/// scenario valid both before and after that change.
#[tokio::test]
async fn test_task_cache_round_trip() {
    setup_tracing();

    let channel = materialized_channel().await;
    let pixi = PixiControl::from_manifest(&manifest(
        &channel,
        r#"
[tasks]
process = { cmd = "echo processing", inputs = ["input.txt"] }
"#,
    ))
    .unwrap();
    let input_path = pixi.workspace_path().join("input.txt");
    fs_err::write(&input_path, "input v1").unwrap();

    pixi.install().await.unwrap();
    assert!(
        EnvironmentFingerprint::read(&pixi.default_env_path().unwrap()).is_some(),
        "a real install must record the install fingerprint marker"
    );

    // Mirror how `pixi_cli::run` builds the executable task: through the task
    // graph.
    let lock_file = pixi.lock_file().await.unwrap();
    let workspace = pixi.workspace().unwrap();
    let platform = pixi_manifest::PixiPlatform::from_subdir(Platform::current());
    let search_envs = SearchEnvironments::from_opt_env(&workspace, None, Some(&platform));
    let task_graph = TaskGraph::from_cmd_args(
        &workspace,
        &search_envs,
        vec!["process".to_string()],
        false,
        PreferExecutable::TaskFirst,
        false,
    )
    .unwrap();
    let order = task_graph.topological_order();
    assert_eq!(order.len(), 1, "expected exactly the `process` task");
    let task = ExecutableTask::from_task_graph(&task_graph, order[0], None);

    // 1. No cache entry exists yet -> the task cannot be skipped.
    let previous_hash = match task.can_skip(&lock_file).await.unwrap() {
        CanSkip::No(previous_hash) => previous_hash,
        CanSkip::Yes => panic!("the task must not be skippable before any cache entry exists"),
    };

    // 2. Mirror the post-run flow of `pixi run`: compute the post-run hash
    //    and persist it. (Actually executing `echo` is irrelevant to the
    //    cache; the task declares no outputs.)
    let post_run_hash = task
        .compute_post_run_hash(&lock_file, previous_hash)
        .await
        .unwrap()
        .expect("compute_post_run_hash always produces a hash today");
    assert!(
        post_run_hash.inputs.is_some(),
        "the declared input file must be matched and hashed into the post-run hash \
         (save_cache silently skips writing when declared inputs match nothing)"
    );
    task.save_cache(Some(post_run_hash)).await.unwrap();

    // 3. Unchanged inputs -> cache hit.
    assert!(
        matches!(task.can_skip(&lock_file).await.unwrap(), CanSkip::Yes),
        "with unchanged inputs the saved cache entry must produce a hit"
    );

    // 4. Rewriting identical content (an mtime-only change) -> still a hit:
    //    today's task-input hashes are content-based, not mtime-based.
    fs_err::write(&input_path, "input v1").unwrap();
    assert!(
        matches!(task.can_skip(&lock_file).await.unwrap(), CanSkip::Yes),
        "an mtime-only touch with identical content must still be a cache hit"
    );

    // 5. Changing an input's content -> miss again.
    fs_err::write(&input_path, "input v2").unwrap();
    match task.can_skip(&lock_file).await.unwrap() {
        CanSkip::No(hash) => assert!(
            hash.is_some(),
            "a stale entry returns the freshly computed hash for reuse"
        ),
        CanSkip::Yes => panic!("a content change in a declared input must invalidate the cache"),
    }
}
