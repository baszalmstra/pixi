use std::path::Path;

use crate::common::{LockFileExt, PixiControl};
use rattler_conda_types::Platform;
use url::Url;

#[tokio::test]
#[cfg_attr(not(feature = "slow_integration_tests"), ignore)]
async fn test_flat_links_based_index_returns_path() {
    let pypi_indexes = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pypi-indexes");
    let pixi = PixiControl::from_manifest(&format!(
        r#"
        [project]
        name = "pypi-extra-index-url"
        platforms = ["{platform}"]
        channels = ["conda-forge"]

        [dependencies]
        python = "~=3.12.0"

        [pypi-dependencies]
        foo = "*"

        [pypi-options]
        find-links = [{{ path = "{pypi_indexes}/multiple-indexes-a/flat"}}]"#,
        platform = Platform::current(),
        pypi_indexes = pypi_indexes.display().to_string().replace("\\", "/"),
    ));
    let lock_file = pixi.unwrap().update_lock_file().await.unwrap();

    // This assertion is specifically to test that if we have a url-based *local* index
    // we will get a path back to the index and the corresponding file
    assert_eq!(
        lock_file
            .get_pypi_package_url("default", Platform::current(), "foo")
            .unwrap()
            .as_path()
            .unwrap()
            .as_str()
            .replace("\\", "/"),
        pypi_indexes
            .join("multiple-indexes-a")
            .join("flat")
            .join("foo-1.0.0-py2.py3-none-any.whl")
            .to_string_lossy()
            .replace("\\", "/"),
    );
}

#[tokio::test]
#[cfg_attr(not(feature = "slow_integration_tests"), ignore)]
async fn test_file_based_index_returns_path() {
    let pypi_indexes = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pypi-indexes");
    let pypi_indexes_url = Url::from_directory_path(pypi_indexes.clone()).unwrap();
    let pixi = PixiControl::from_manifest(&format!(
        r#"
        [project]
        name = "pypi-extra-index-url"
        platforms = ["{platform}"]
        channels = ["conda-forge"]

        [dependencies]
        python = "~=3.12.0"

        [pypi-dependencies]
        foo = "*"

        [pypi-options]
        extra-index-urls = [
            "{pypi_indexes}multiple-indexes-a/index"
        ]"#,
        platform = Platform::current(),
        pypi_indexes = pypi_indexes_url,
    ));
    let lock_file = pixi.unwrap().update_lock_file().await.unwrap();

    // This assertion is specifically to test that if we have a url-based *local* index
    // we will get a path back to the index and the corresponding file
    assert_eq!(
        lock_file
            .get_pypi_package_url("default", Platform::current(), "foo")
            .unwrap()
            .as_path()
            .unwrap()
            .as_str()
            .replace("\\", "/"),
        pypi_indexes
            .join("multiple-indexes-a/index/foo")
            .join("foo-1.0.0-py2.py3-none-any.whl")
            .to_string_lossy()
            .replace("\\", "/")
    );
}

#[tokio::test]
#[cfg_attr(not(feature = "slow_integration_tests"), ignore)]
async fn test_index_strategy() {
    let pypi_indexes = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pypi-indexes");
    let pypi_indexes_url = Url::from_directory_path(pypi_indexes.clone()).unwrap();

    let pixi = PixiControl::from_manifest(&format!(
        r#"
        [project]
        name = "pypi-extra-index-url"
        platforms = ["{platform}"]
        channels = ["conda-forge"]

        [dependencies]
        python = "~=3.12.0"

        [pypi-dependencies]
        foo = "*"

        [pypi-options]
        extra-index-urls = [
            "{pypi_indexes}multiple-indexes-a/index",
            "{pypi_indexes}multiple-indexes-b/index",
            "{pypi_indexes}multiple-indexes-c/index",
        ]

        [feature.first-index.pypi-options]
        index-strategy = "first-index"

        [feature.unsafe-first-match-unconstrained.pypi-options]
        index-strategy = "unsafe-first-match"

        [feature.unsafe-first-match-constrained.pypi-options]
        index-strategy = "unsafe-first-match"

        [feature.unsafe-first-match-constrained.pypi-dependencies]
        foo = "==3.0.0"

        [feature.unsafe-best-match.pypi-options]
        index-strategy = "unsafe-best-match"

        [environments]
        default = ["first-index"]
        unsafe-first-match-unconstrained = ["unsafe-first-match-unconstrained"]
        unsafe-first-match-constrained = ["unsafe-first-match-constrained"]
        unsafe-best-match = ["unsafe-best-match"]
        "#,
        platform = Platform::current(),
        pypi_indexes = pypi_indexes_url,
    ));

    let lock_file = pixi.unwrap().update_lock_file().await.unwrap();

    assert_eq!(
        lock_file.get_pypi_package_version("default", Platform::current(), "foo"),
        Some("1.0.0".into())
    );
    assert_eq!(
        lock_file.get_pypi_package_version(
            "unsafe-first-match-unconstrained",
            Platform::current(),
            "foo"
        ),
        Some("1.0.0".into())
    );

    assert_eq!(
        lock_file.get_pypi_package_version(
            "unsafe-first-match-constrained",
            Platform::current(),
            "foo"
        ),
        Some("3.0.0".into())
    );
    assert_eq!(
        lock_file.get_pypi_package_version("unsafe-best-match", Platform::current(), "foo"),
        Some("3.0.0".into())
    );
}
