//! Integration tests for [`pixi_compute_sources::CheckoutGit`] and the
//! [`pixi_compute_sources::GitSourceCheckoutExt`] entry points.
//!
//! Tests use [`pixi_test_utils::GitRepoFixture`] to build a temporary
//! git repository (via the local `git` binary, no network).

mod common;

use common::{EngineConfig, LifecycleReporter, build_test_engine};
use pixi_compute_sources::GitSourceCheckoutExt;
use pixi_git::GitLfs;
use pixi_record::PinnedSourceSpec;
use pixi_spec::{GitSpec, Subdirectory};
use pixi_test_utils::GitRepoFixture;

/// `pin_and_checkout_git` against a fresh fixture clones the repo,
/// returns a checkout pointing at the head commit, and produces a
/// pinned spec that records the resolved commit.
#[tokio::test]
async fn pin_and_checkout_git_default_branch() {
    let repo = GitRepoFixture::new("minimal-pypi-package");
    let head = repo.latest_commit().to_string();

    let engine = build_test_engine(EngineConfig {
        sequential: true,
        ..Default::default()
    });

    let spec = GitSpec {
        git: repo.base_url.clone(),
        rev: None,
        subdirectory: Subdirectory::default(),
        lfs: GitLfs::Disabled,
    };

    let checkout = engine
        .with_ctx(async |ctx| ctx.pin_and_checkout_git(spec).await)
        .await
        .expect("engine scope")
        .expect("git checkout should succeed");

    assert!(
        checkout.path.as_std_path().is_dir(),
        "checkout path should be a directory: {:?}",
        checkout.path
    );

    match checkout.pinned {
        PinnedSourceSpec::Git(pinned) => {
            assert_eq!(
                pinned.source.commit.to_string(),
                head,
                "pinned commit should match the fixture head"
            );
            assert_eq!(pinned.git, repo.base_url);
        }
        other => panic!("expected git pinned spec, got {other:?}"),
    }
}

/// `lfs = Some(true)` on the spec flows through `pin_and_checkout_git` to
/// `GitSource`, which materialises LFS blobs and records the flag on the
/// pinned spec for the lock file.
#[tokio::test]
async fn pin_and_checkout_git_with_lfs() {
    let lfs_ok = std::process::Command::new("git")
        .args(["lfs", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !lfs_ok {
        eprintln!("skipping pin_and_checkout_git_with_lfs: git-lfs is not installed");
        return;
    }

    let repo = GitRepoFixture::new("lfs-sample");
    assert!(repo.uses_lfs, "fixture should auto-detect LFS");

    let engine = build_test_engine(EngineConfig {
        sequential: true,
        ..Default::default()
    });

    let spec = GitSpec {
        git: repo.base_url.clone(),
        rev: None,
        subdirectory: Subdirectory::default(),
        lfs: GitLfs::Enabled,
    };

    let checkout = engine
        .with_ctx(async |ctx| ctx.pin_and_checkout_git(spec).await)
        .await
        .expect("engine scope")
        .expect("git checkout should succeed");

    // The lock-file pin records the explicit LFS request so reinstalls
    // re-validate LFS objects (`db.contains_lfs_artifacts` in `GitSource`).
    match checkout.pinned {
        PinnedSourceSpec::Git(pinned) => {
            assert_eq!(pinned.source.lfs, GitLfs::Enabled);
        }
        other => panic!("expected git pinned spec, got {other:?}"),
    }

    // `data.bin` should be the real blob — not still a git-lfs pointer file.
    let data = checkout.path.join("data.bin");
    let contents = fs_err::read_to_string(data.as_std_path()).unwrap_or_default();
    assert!(
        !contents.starts_with("version https://git-lfs.github.com/spec/"),
        "data.bin should be materialised, not a pointer file"
    );
}

/// One git checkout fires the full reporter sequence. The
/// [`LifecycleReporter`] asserts ordering and exactly-once internally;
/// the test just asserts the run reached the terminal state.
#[tokio::test]
async fn git_checkout_fires_full_reporter_lifecycle() {
    let repo = GitRepoFixture::new("minimal-pypi-package");

    let reporter = LifecycleReporter::new();
    let engine = build_test_engine(EngineConfig {
        git_reporter: Some(reporter.clone()),
        sequential: true,
        ..Default::default()
    });

    engine
        .with_ctx(async |ctx| {
            ctx.pin_and_checkout_git(GitSpec {
                git: repo.base_url.clone(),
                rev: None,
                subdirectory: Subdirectory::default(),
                lfs: GitLfs::Disabled,
            })
            .await
        })
        .await
        .expect("engine scope")
        .expect("git checkout should succeed");

    reporter.assert_complete();
}
