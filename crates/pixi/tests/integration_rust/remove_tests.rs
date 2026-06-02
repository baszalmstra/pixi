//! End-to-end tests for `pixi remove`, driving the real `remove::execute`
//! against an on-disk manifest and asserting the resulting tables.
//!
//! These cover the user-facing decisions of the unified remove command:
//! searching every table by default, the default-feature preference, ambiguity
//! handling, atomicity, `--all`, the location flags, and the Python guard. Each
//! runs fully offline via `--frozen --no-install`.

use pixi_core::DependencyType;
use pixi_manifest::{FeatureName, HasWorkspaceManifest, SpecType, WorkspaceManifest};
use pixi_test_utils::format_diagnostic;

use crate::common::{
    PixiControl,
    builders::{HasDependencyConfig, HasLockFileUpdateConfig},
};
use crate::setup_tracing;

/// Wrap a manifest body in a minimal workspace header.
fn manifest(body: &str) -> String {
    format!(
        r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]
{body}
"#
    )
}

fn feature_label(feature: &FeatureName) -> String {
    if feature.is_default() {
        "default".to_string()
    } else {
        feature.as_str().to_string()
    }
}

fn table_label(spec_type: SpecType) -> &'static str {
    match spec_type {
        SpecType::Run => "dependencies",
        SpecType::Host => "host-dependencies",
        SpecType::Build => "build-dependencies",
        SpecType::RunConstraints => "run-constraints",
    }
}

/// Every distinct `"<feature>/<table>"` a dependency `name` appears in, sorted
/// and de-duplicated (platform variants of the same table collapse to one
/// entry). Reloads the manifest from disk so it reflects the saved state.
fn tables_of(pixi: &PixiControl, name: &str) -> Vec<String> {
    let workspace = pixi.workspace().unwrap();
    let manifest: &WorkspaceManifest = (&workspace).workspace_manifest();

    let mut tables = Vec::new();
    for dep in manifest.iter_conda_dependencies() {
        if dep.name.as_normalized() == name {
            tables.push(format!(
                "{}/{}",
                feature_label(dep.feature),
                table_label(dep.spec_type)
            ));
        }
    }
    for dep in manifest.iter_pypi_dependencies() {
        if dep.name.as_source() == name {
            tables.push(format!("{}/pypi-dependencies", feature_label(dep.feature)));
        }
    }
    tables.sort();
    tables.dedup();
    tables
}

/// A bare `pixi remove` of a default-feature dependency removes it and leaves
/// siblings in the same table untouched.
#[tokio::test]
async fn remove_from_default_dependencies() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"
ruff = "*"
"#,
    ))
    .unwrap();

    pixi.remove("numpy").with_frozen(true).await.unwrap();

    assert!(tables_of(&pixi, "numpy").is_empty());
    assert_eq!(tables_of(&pixi, "ruff"), ["default/dependencies"]);
}

/// A bare remove searches every table: it finds packages that live only in
/// `pypi-dependencies`, in a non-default feature, or in a platform target.
#[tokio::test]
async fn remove_finds_dependency_in_any_table() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[pypi-dependencies]
in-pypi = "*"

[feature.dev.dependencies]
in-feature = "*"

[target.linux-64.dependencies]
in-target = "*"
"#,
    ))
    .unwrap();

    pixi.remove("in-pypi").with_frozen(true).await.unwrap();
    pixi.remove("in-feature").with_frozen(true).await.unwrap();
    pixi.remove("in-target").with_frozen(true).await.unwrap();

    assert!(tables_of(&pixi, "in-pypi").is_empty());
    assert!(tables_of(&pixi, "in-feature").is_empty());
    assert!(tables_of(&pixi, "in-target").is_empty());
}

/// When a package is in the default `[dependencies]` *and* elsewhere, a bare
/// remove takes it from the default table and leaves the rest, instead of
/// erroring as ambiguous.
#[tokio::test]
async fn remove_prefers_default_feature_when_also_elsewhere() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"

[feature.dev.dependencies]
numpy = "*"
"#,
    ))
    .unwrap();

    pixi.remove("numpy").with_frozen(true).await.unwrap();

    assert_eq!(tables_of(&pixi, "numpy"), ["dev/dependencies"]);
}

/// A package present in the default table both platform-agnostically and under
/// a platform target is one logical location, so a bare remove clears both.
#[tokio::test]
async fn remove_collapses_platform_variants() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"

[target.linux-64.dependencies]
numpy = "*"
"#,
    ))
    .unwrap();

    pixi.remove("numpy").with_frozen(true).await.unwrap();

    assert!(tables_of(&pixi, "numpy").is_empty());
}

/// A package in several non-default tables is ambiguous: the command fails,
/// names each location, and leaves the manifest untouched.
#[tokio::test]
async fn remove_ambiguous_errors_and_keeps_manifest() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[pypi-dependencies]
requests = "*"

[feature.dev.dependencies]
requests = "*"
"#,
    ))
    .unwrap();

    let err = pixi.remove("requests").with_frozen(true).await.unwrap_err();
    let rendered = format_diagnostic(&*err);
    assert!(
        rendered.contains("defined in multiple locations"),
        "{rendered}"
    );
    assert!(
        rendered.contains("pixi remove --pypi requests"),
        "{rendered}"
    );
    assert!(
        rendered.contains("pixi remove --feature dev requests"),
        "{rendered}"
    );

    // Nothing was removed.
    assert_eq!(tables_of(&pixi, "requests").len(), 2);
}

/// A bare remove of a package that exists nowhere fails with a workspace-wide
/// "not found" and changes nothing.
#[tokio::test]
async fn remove_missing_errors() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"
"#,
    ))
    .unwrap();

    let err = pixi.remove("absent").with_frozen(true).await.unwrap_err();
    assert!(
        format_diagnostic(&*err).contains("was not found in the workspace"),
        "{:?}",
        err
    );
    assert_eq!(tables_of(&pixi, "numpy"), ["default/dependencies"]);
}

/// Resolution is atomic across specs: if any requested package can't be placed,
/// the whole command fails without removing the ones that could.
#[tokio::test]
async fn remove_is_atomic_across_specs() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"
"#,
    ))
    .unwrap();

    pixi.remove("numpy")
        .with_spec("absent")
        .with_frozen(true)
        .await
        .unwrap_err();

    // `numpy` resolved but must not have been removed.
    assert_eq!(tables_of(&pixi, "numpy"), ["default/dependencies"]);
}

/// `--all` removes every occurrence: conda, pypi, and feature copies all go.
#[tokio::test]
async fn remove_all_strips_every_occurrence() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"

[pypi-dependencies]
numpy = "*"

[feature.dev.dependencies]
numpy = "*"
"#,
    ))
    .unwrap();

    pixi.remove("numpy")
        .with_all(true)
        .with_frozen(true)
        .await
        .unwrap();

    assert!(tables_of(&pixi, "numpy").is_empty());
}

/// `--all` removes an otherwise-ambiguous package from every location instead
/// of erroring.
#[tokio::test]
async fn remove_all_resolves_ambiguous() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[pypi-dependencies]
requests = "*"

[feature.dev.dependencies]
requests = "*"
"#,
    ))
    .unwrap();

    pixi.remove("requests")
        .with_all(true)
        .with_frozen(true)
        .await
        .unwrap();

    assert!(tables_of(&pixi, "requests").is_empty());
}

/// `--all` of a missing package is still an error and changes nothing.
#[tokio::test]
async fn remove_all_missing_errors() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"
"#,
    ))
    .unwrap();

    pixi.remove("absent")
        .with_all(true)
        .with_frozen(true)
        .await
        .unwrap_err();

    assert_eq!(tables_of(&pixi, "numpy"), ["default/dependencies"]);
}

/// A location flag restricts the removal to that table: `--pypi` takes only the
/// pypi entry and leaves the conda one.
#[tokio::test]
async fn remove_pypi_flag_restricts_to_pypi() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
black = "*"

[pypi-dependencies]
black = "*"
"#,
    ))
    .unwrap();

    pixi.remove("black")
        .set_type(DependencyType::PypiDependency)
        .with_frozen(true)
        .await
        .unwrap();

    // The pypi entry is gone; the conda one is untouched.
    assert_eq!(tables_of(&pixi, "black"), ["default/dependencies"]);
}

/// `--pypi` on a package that is only a conda dependency fails, but points at
/// the command that would remove it, and changes nothing.
#[tokio::test]
async fn remove_pypi_flag_not_found_suggests_conda() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
ruff = "*"
"#,
    ))
    .unwrap();

    let err = pixi
        .remove("ruff")
        .set_type(DependencyType::PypiDependency)
        .with_frozen(true)
        .await
        .unwrap_err();
    let rendered = format_diagnostic(&*err);
    assert!(
        rendered.contains("was not found in pypi-dependencies"),
        "{rendered}"
    );
    assert!(rendered.contains("pixi remove ruff"), "{rendered}");

    assert_eq!(tables_of(&pixi, "ruff"), ["default/dependencies"]);
}

/// `--feature` restricts the removal to that feature, leaving the default copy.
#[tokio::test]
async fn remove_feature_flag_restricts() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
numpy = "*"

[feature.dev.dependencies]
numpy = "*"
"#,
    ))
    .unwrap();

    pixi.remove("numpy")
        .with_feature("dev")
        .with_frozen(true)
        .await
        .unwrap();

    assert_eq!(tables_of(&pixi, "numpy"), ["default/dependencies"]);
}

/// Removing Python while PyPI dependencies still exist is refused, and the
/// manifest is left untouched.
#[tokio::test]
async fn remove_python_guarded_by_pypi_deps() {
    setup_tracing();
    let pixi = PixiControl::from_manifest(&manifest(
        r#"
[dependencies]
python = "*"

[pypi-dependencies]
requests = "*"
"#,
    ))
    .unwrap();

    let err = pixi.remove("python").with_frozen(true).await.unwrap_err();
    assert!(
        format_diagnostic(&*err).contains("Cannot remove Python while PyPI dependencies exist"),
        "{:?}",
        err
    );
    assert_eq!(tables_of(&pixi, "python"), ["default/dependencies"]);
}
