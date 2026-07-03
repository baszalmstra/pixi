//! Integration tests for the experimental workspace input fingerprint
//! (`[experimental] use-workspace-freshness-cache`) and the fast-path gate in
//! `Workspace::update_lock_file`.
//!
//! All tests run offline against a materialized `MockRepoData` channel.

use std::path::PathBuf;

use pixi_test_utils::{MockRepoData, Package};
use rattler_conda_types::Platform;

use crate::common::{LockFileExt, PixiControl};
use crate::setup_tracing;

/// The probe kinds a v1 fingerprint must record.
const EXPECTED_PROBE_KINDS: [&str; 5] = [
    "manifest-file-content",
    "resolved-config",
    "lock-file-content",
    "env-vars",
    "pixi-version",
];

/// Enables the experimental freshness cache in the workspace-local
/// `.pixi/config.toml` that `PixiControl::new` seeds (appending, not
/// clobbering, so the default local channel configuration survives).
fn enable_freshness_cache(pixi: &PixiControl) {
    let config_path = pixi.workspace_path().join(".pixi").join("config.toml");
    let mut contents = fs_err::read_to_string(&config_path).unwrap_or_default();
    contents.push_str("\n[experimental]\nuse-workspace-freshness-cache = true\n");
    fs_err::write(&config_path, contents).unwrap();
}

/// The path of the workspace fingerprint marker.
fn marker_path(pixi: &PixiControl) -> PathBuf {
    pixi.workspace_path()
        .join(".pixi")
        .join("freshness")
        .join("workspace.json")
}

/// Parses the marker and returns the digest recorded for the probe with the
/// given kind.
fn probe_digest(marker: &serde_json::Value, kind: &str) -> String {
    marker["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|probe| probe["kind"] == kind)
        .unwrap_or_else(|| panic!("marker should contain a `{kind}` probe"))["digest"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Creates an offline channel with materialized `foo` and `bar` packages for
/// the current platform.
async fn materialized_channel() -> pixi_test_utils::LocalChannel {
    let mut database = MockRepoData::default();
    database.add_package(
        Package::build("foo", "1.0.0")
            .with_subdir(Platform::current())
            .with_materialize(true)
            .finish(),
    );
    database.add_package(
        Package::build("bar", "1.0.0")
            .with_subdir(Platform::current())
            .with_materialize(true)
            .finish(),
    );
    database.into_channel().await.unwrap()
}

/// A `pixi install` with the flag enabled records the fingerprint marker with
/// the expected probes.
#[tokio::test(flavor = "multi_thread")]
async fn install_records_input_fingerprint() {
    setup_tracing();
    let channel = materialized_channel().await;

    let pixi = PixiControl::from_manifest(&format!(
        r#"
        [workspace]
        name = "fingerprint-install"
        channels = ["{channel}"]
        platforms = ["{platform}"]

        [dependencies]
        foo = "*"
        "#,
        channel = channel.url(),
        platform = Platform::current(),
    ))
    .unwrap();
    enable_freshness_cache(&pixi);

    pixi.install().await.unwrap();

    // The marker exists and records exactly the v1 probes.
    let marker_bytes = fs_err::read(marker_path(&pixi)).unwrap();
    let marker: serde_json::Value = serde_json::from_slice(&marker_bytes).unwrap();
    assert_eq!(marker["schema_version"], 1);
    let kinds: Vec<&str> = marker["probes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|probe| probe["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, EXPECTED_PROBE_KINDS);

    // The lock file exists, so its digest must be a real digest, not the
    // absent sentinel.
    let lock_digest = probe_digest(&marker, "lock-file-content");
    assert_eq!(lock_digest.len(), 16);
    assert!(lock_digest.chars().all(|c| c.is_ascii_hexdigit()));

    // The env-vars probe always pins PIXI_OVERRIDE_PLATFORM.
    let env_probe = marker["probes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|probe| probe["kind"] == "env-vars")
        .unwrap();
    assert!(
        env_probe["vars"]
            .as_object()
            .unwrap()
            .contains_key("PIXI_OVERRIDE_PLATFORM")
    );
}

/// With a fresh fingerprint, a second `update_lock_file` succeeds and leaves
/// the lock file untouched (the gate skips the outdated walk; behavior stays
/// identical to the slow path).
#[tokio::test(flavor = "multi_thread")]
async fn second_update_hits_the_gate_and_keeps_the_lock_file() {
    setup_tracing();
    let channel = materialized_channel().await;

    let pixi = PixiControl::new().unwrap();
    enable_freshness_cache(&pixi);
    pixi.init()
        .with_local_channel(channel.url().to_file_path().unwrap())
        .await
        .unwrap();
    pixi.add("foo").await.unwrap();

    // First update reaches steady state and records the fingerprint.
    let lock = pixi.update_lock_file().await.unwrap();
    assert!(lock.contains_conda_package("default", Platform::current(), "foo"));
    assert!(marker_path(&pixi).is_file());
    let lock_bytes_before = fs_err::read(pixi.workspace_path().join("pixi.lock")).unwrap();
    let marker_before = fs_err::read(marker_path(&pixi)).unwrap();

    // Second update: inputs unchanged, must succeed and not touch the lock.
    let lock = pixi.update_lock_file().await.unwrap();
    assert!(lock.contains_conda_package("default", Platform::current(), "foo"));
    let lock_bytes_after = fs_err::read(pixi.workspace_path().join("pixi.lock")).unwrap();
    assert_eq!(
        lock_bytes_before, lock_bytes_after,
        "a no-op update must not rewrite the lock file"
    );

    // The marker still validates against the same inputs (content equal).
    let marker_after = fs_err::read(marker_path(&pixi)).unwrap();
    assert_eq!(marker_before, marker_after);
}

/// Editing the manifest makes the fingerprint stale: the next update
/// re-solves correctly and re-records the marker with a new manifest digest.
#[tokio::test(flavor = "multi_thread")]
async fn manifest_edit_invalidates_and_rerecords_the_fingerprint() {
    setup_tracing();
    let channel = materialized_channel().await;

    let pixi = PixiControl::new().unwrap();
    enable_freshness_cache(&pixi);
    pixi.init()
        .with_local_channel(channel.url().to_file_path().unwrap())
        .await
        .unwrap();
    pixi.add("foo").await.unwrap();
    pixi.update_lock_file().await.unwrap();

    let marker: serde_json::Value =
        serde_json::from_slice(&fs_err::read(marker_path(&pixi)).unwrap()).unwrap();
    let manifest_digest_before = probe_digest(&marker, "manifest-file-content");
    let lock_digest_before = probe_digest(&marker, "lock-file-content");

    // Edit the manifest (pixi add rewrites pixi.toml and re-solves).
    pixi.add("bar").await.unwrap();

    // The re-solve happened even though a (stale) fingerprint existed.
    let lock = pixi.update_lock_file().await.unwrap();
    assert!(
        lock.contains_conda_package("default", Platform::current(), "bar"),
        "the manifest edit must lead to a re-solve that locks `bar`"
    );

    // The marker was re-recorded with new digests.
    let marker: serde_json::Value =
        serde_json::from_slice(&fs_err::read(marker_path(&pixi)).unwrap()).unwrap();
    assert_eq!(marker["schema_version"], 1);
    let manifest_digest_after = probe_digest(&marker, "manifest-file-content");
    let lock_digest_after = probe_digest(&marker, "lock-file-content");
    assert_ne!(
        manifest_digest_before, manifest_digest_after,
        "the manifest digest must change after editing the manifest"
    );
    assert_ne!(
        lock_digest_before, lock_digest_after,
        "the lock digest must change after a re-solve"
    );
}

/// A corrupt marker never breaks an update: pixi falls through to the normal
/// path and rewrites a valid marker.
#[tokio::test(flavor = "multi_thread")]
async fn corrupt_marker_falls_back_and_is_rewritten() {
    setup_tracing();
    let channel = materialized_channel().await;

    let pixi = PixiControl::new().unwrap();
    enable_freshness_cache(&pixi);
    pixi.init()
        .with_local_channel(channel.url().to_file_path().unwrap())
        .await
        .unwrap();
    pixi.add("foo").await.unwrap();
    pixi.update_lock_file().await.unwrap();
    assert!(marker_path(&pixi).is_file());

    // Corrupt the marker.
    fs_err::write(marker_path(&pixi), b"{ this is not json").unwrap();
    let lock_bytes_before = fs_err::read(pixi.workspace_path().join("pixi.lock")).unwrap();

    // The update still works and does not touch the lock file.
    let lock = pixi.update_lock_file().await.unwrap();
    assert!(lock.contains_conda_package("default", Platform::current(), "foo"));
    let lock_bytes_after = fs_err::read(pixi.workspace_path().join("pixi.lock")).unwrap();
    assert_eq!(lock_bytes_before, lock_bytes_after);

    // The marker was rewritten and is valid JSON again.
    let marker: serde_json::Value =
        serde_json::from_slice(&fs_err::read(marker_path(&pixi)).unwrap()).unwrap();
    assert_eq!(marker["schema_version"], 1);
}

/// With the flag off (the default), no marker is ever written.
#[tokio::test(flavor = "multi_thread")]
async fn flag_off_never_writes_a_marker() {
    setup_tracing();
    let channel = materialized_channel().await;

    let pixi = PixiControl::new().unwrap();
    // Note: deliberately NOT enabling the flag.
    pixi.init()
        .with_local_channel(channel.url().to_file_path().unwrap())
        .await
        .unwrap();
    pixi.add("foo").await.unwrap();
    pixi.update_lock_file().await.unwrap();
    pixi.update_lock_file().await.unwrap();

    assert!(
        !marker_path(&pixi).exists(),
        "no fingerprint marker may be written when the flag is off"
    );
    assert!(
        !pixi
            .workspace_path()
            .join(".pixi")
            .join("freshness")
            .exists(),
        "the freshness directory must not be created when the flag is off"
    );
}
