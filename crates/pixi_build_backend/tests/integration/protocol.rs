use std::sync::Arc;

use crate::common::model::{convert_test_model_to_project_model_v1, load_project_model_from_json};
use imp::TestGenerateRecipe;
use ordermap::OrderMap;
use pixi_build_backend::{
    intermediate_backend::IntermediateBackend, protocol::Protocol, tools::BackendIdentifier,
    utils::test::intermediate_conda_outputs,
};
use pixi_build_types::{
    BinaryPackageSpec, ConditionalExpression, ExtraGroupName, PackageSpec, PathSpec, ProjectModel,
    SourcePackageName, Target, Targets,
    procedures::conda_build_v1::{CondaBuildV1Output, CondaBuildV1Params},
};
use rattler_build_core::console_utils::LoggingOutputHandler;
use rattler_conda_types::{ChannelUrl, PackageName, Platform};
use serde_json::json;
use tempfile::TempDir;
use url::Url;

#[cfg(test)]
mod imp {
    use miette::IntoDiagnostic;
    use pixi_build_backend::generated_recipe::{
        BackendConfig, DefaultMetadataProvider, GenerateRecipe, GeneratedRecipe, PythonParams,
    };
    use rattler_conda_types::ChannelUrl;
    use serde::{Deserialize, Serialize};
    use std::{
        collections::HashSet,
        path::{Path, PathBuf},
    };

    #[derive(Debug, Default, Serialize, Deserialize, Clone)]
    #[serde(rename_all = "kebab-case")]
    pub struct TestBackendConfig {
        /// If set, internal state will be logged as files in that directory
        #[serde(alias = "debug_dir")]
        pub debug_dir: Option<PathBuf>,
    }

    #[cfg(test)]
    #[derive(Clone, Default)]
    pub(crate) struct TestGenerateRecipe {}

    impl BackendConfig for TestBackendConfig {
        fn debug_dir(&self) -> Option<&Path> {
            self.debug_dir.as_deref()
        }

        fn merge_with_target_config(&self, _target_config: &Self) -> miette::Result<Self> {
            Ok(Self {
                debug_dir: self.debug_dir.clone(),
            })
        }
    }

    #[async_trait::async_trait]
    impl GenerateRecipe for TestGenerateRecipe {
        type Config = TestBackendConfig;

        async fn generate_recipe(
            &self,
            model: &pixi_build_types::ProjectModel,
            _config: &Self::Config,
            _manifest_path: PathBuf,
            _host_platform: rattler_conda_types::Platform,
            _python_params: Option<PythonParams>,
            _variants: &HashSet<pixi_build_backend::variants::NormalizedKey>,
            _channels: Vec<ChannelUrl>,
            _cache_dir: Option<PathBuf>,
            _workspace_scratch_directory: Option<PathBuf>,
            _workspace_directory: Option<PathBuf>,
            _checkout_root: Option<PathBuf>,
        ) -> miette::Result<GeneratedRecipe> {
            GeneratedRecipe::from_model(model.clone(), &mut DefaultMetadataProvider)
                .into_diagnostic()
        }
    }
}

#[tokio::test]
#[ignore] // TODO: this test since it sometimes hangs in CI
async fn test_conda_build_v1() {
    let tmp_dir = TempDir::new().unwrap();
    let tmp_dir_path = tmp_dir.path().to_path_buf();

    let pixi_manifest = tmp_dir_path.join("pixi.toml");
    let build_dir = tmp_dir_path.join("build");

    // Load a model from JSON
    let original_model = load_project_model_from_json("minimal_project_model_for_build.json");

    // Serialize it back to JSON
    let project_model_v1 = convert_test_model_to_project_model_v1(original_model);

    // save the pixi.toml file to a temporary location
    fs_err::write(&pixi_manifest, toml::to_string(&project_model_v1).unwrap()).unwrap();

    let channel_url = Url::parse("https://prefix.dev/conda-forge").unwrap();

    let channel_url = ChannelUrl::from(channel_url);

    let build_params = CondaBuildV1Params {
        channels: vec![channel_url],

        build_prefix: None,
        host_prefix: None,
        run_constraints: None,
        run_dependencies: None,
        run_exports: None,
        extra_dependencies: Default::default(),
        output: CondaBuildV1Output {
            name: "minimal-package".parse().unwrap(),
            version: None,
            build: None,
            subdir: Platform::current(),
            variant: Default::default(),
        },
        work_directory: build_dir.clone(),
        output_directory: None,
        editable: None,
        package_format: None,
    };

    let some_config = json!({
        "debug-dir": "some_debug_dir",
    });

    let target_config = Default::default();

    let intermediate_backend: IntermediateBackend<TestGenerateRecipe> = IntermediateBackend::new(
        BackendIdentifier::new("test-backend", env!("CARGO_PKG_VERSION")),
        pixi_manifest.clone(),
        Some(tmp_dir_path.clone()),
        project_model_v1,
        Arc::default(),
        some_config,
        target_config,
        LoggingOutputHandler::default(),
        None,
        None,
        None,
        None,
    )
    .unwrap();

    let conda_build_result = intermediate_backend
        .conda_build_v1(build_params)
        .await
        .unwrap();

    insta::assert_yaml_snapshot!(conda_build_result, {
        ".output_file" => "[redacted]",
        ".build" => "[redacted]",
        ".subdir" => "[redacted]",
    });

    assert!(build_dir.join("debug").join("recipe.yaml").exists());
}

#[tokio::test]
async fn test_conda_outputs_build_string_prefix() {
    let original_model = load_project_model_from_json("minimal_project_model_for_build.json");

    // Build without prefix
    let model_no_prefix = convert_test_model_to_project_model_v1(original_model.clone());
    let result_no_prefix = intermediate_conda_outputs::<TestGenerateRecipe>(
        Some(model_no_prefix),
        None,
        Platform::current(),
        None,
        None,
    )
    .await;

    assert!(
        !result_no_prefix.outputs.is_empty(),
        "should produce at least one output"
    );
    let default_build = &result_no_prefix.outputs[0].metadata.build;
    assert!(
        !default_build.is_empty(),
        "default build string should not be empty"
    );
    // Default build string should contain a hash (starts with 'h')
    assert!(
        default_build.contains('h'),
        "default build string should contain a hash, got: {default_build}"
    );

    // Build with prefix
    let mut model_with_prefix = convert_test_model_to_project_model_v1(original_model);
    model_with_prefix.build_string_prefix = Some("mypfx".to_string());
    let result_with_prefix = intermediate_conda_outputs::<TestGenerateRecipe>(
        Some(model_with_prefix),
        None,
        Platform::current(),
        None,
        None,
    )
    .await;

    assert!(
        !result_with_prefix.outputs.is_empty(),
        "should produce at least one output"
    );
    let prefixed_build = &result_with_prefix.outputs[0].metadata.build;

    // Prefixed build string should be "mypfx_" + the default build string
    assert_eq!(
        *prefixed_build,
        format!("mypfx_{default_build}"),
        "prefixed build string should be 'mypfx_' + default"
    );
}

/// Extra groups must survive the round-trip through
/// `conda/outputs`: a binary dependency stays a binary spec, a source
/// dependency is preserved as a source spec rather than being stringified into
/// a meaningless match spec, and a target-specific group only applies on the
/// matching platform.
#[tokio::test]
async fn test_conda_outputs_extra_dependencies() {
    fn binary_spec() -> PackageSpec {
        BinaryPackageSpec {
            version: Some("*".parse().unwrap()),
            ..BinaryPackageSpec::default()
        }
        .into()
    }

    // Default-target `test` group with a binary and a source (path) dependency.
    let mut test_group: OrderMap<SourcePackageName, PackageSpec> = OrderMap::new();
    test_group.insert(
        SourcePackageName::from(PackageName::new_unchecked("gtest")),
        binary_spec(),
    );
    test_group.insert(
        SourcePackageName::from(PackageName::new_unchecked("mylib")),
        PackageSpec::Source(
            PathSpec {
                path: "../mylib".into(),
            }
            .into(),
        ),
    );
    let mut default_extras = OrderMap::new();
    default_extras.insert(ExtraGroupName::new("test").unwrap(), test_group);

    // Windows-specific `gpu` group.
    let mut gpu_group: OrderMap<SourcePackageName, PackageSpec> = OrderMap::new();
    gpu_group.insert(
        SourcePackageName::from(PackageName::new_unchecked("cudnn")),
        binary_spec(),
    );
    let mut win_extras = OrderMap::new();
    win_extras.insert(ExtraGroupName::new("gpu").unwrap(), gpu_group);

    let mut conditional_targets = OrderMap::new();
    conditional_targets.insert(
        ConditionalExpression::new("win"),
        Target {
            extra_dependencies: Some(win_extras),
            ..Target::default()
        },
    );

    let model = ProjectModel {
        name: Some("example".to_string()),
        version: Some("0.1.0".parse().unwrap()),
        targets: Some(Targets {
            default_target: Some(Target {
                extra_dependencies: Some(default_extras),
                ..Target::default()
            }),
            conditional: Some(conditional_targets),
        }),
        ..ProjectModel::default()
    };

    // Render for windows so the windows-specific group applies as well.
    let result = intermediate_conda_outputs::<TestGenerateRecipe>(
        Some(model),
        None,
        Platform::Win64,
        None,
        None,
    )
    .await;

    let output = result.outputs.first().expect("should produce one output");
    let extras = &output.extra_dependencies;

    let test = extras
        .get(&ExtraGroupName::new("test").unwrap())
        .expect("the `test` group should be present");
    assert!(
        test.iter()
            .any(|dep| dep.name.as_str() == "gtest" && matches!(dep.spec, PackageSpec::Binary(_))),
        "binary dependency in an extra group should stay a binary spec, got {test:?}"
    );
    assert!(
        test.iter()
            .any(|dep| dep.name.as_str() == "mylib" && matches!(dep.spec, PackageSpec::Source(_))),
        "source dependency in an extra group should round-trip as a source spec, got {test:?}"
    );

    let gpu = extras
        .get(&ExtraGroupName::new("gpu").unwrap())
        .expect("the windows-specific `gpu` group should apply when building for windows");
    assert!(
        gpu.iter().any(|dep| dep.name.as_str() == "cudnn"),
        "the `gpu` group should contain its dependency, got {gpu:?}"
    );
}

#[tokio::test]
async fn test_conda_outputs_build_number() {
    let original_model = load_project_model_from_json("minimal_project_model_for_build.json");

    // Without build number override -- should default to 0
    let model_default = convert_test_model_to_project_model_v1(original_model.clone());
    let result_default = intermediate_conda_outputs::<TestGenerateRecipe>(
        Some(model_default),
        None,
        Platform::current(),
        None,
        None,
    )
    .await;

    assert!(!result_default.outputs.is_empty());
    assert_eq!(
        result_default.outputs[0].metadata.build_number, 0,
        "default build number should be 0"
    );

    // With build number override
    let mut model_with_bn = convert_test_model_to_project_model_v1(original_model);
    model_with_bn.build_number = Some(42);
    let result_with_bn = intermediate_conda_outputs::<TestGenerateRecipe>(
        Some(model_with_bn),
        None,
        Platform::current(),
        None,
        None,
    )
    .await;

    assert!(!result_with_bn.outputs.is_empty());
    assert_eq!(
        result_with_bn.outputs[0].metadata.build_number, 42,
        "build number should be 42"
    );
}

/// End-to-end round trip for a self-referential `pin-subpackage` run-export,
/// asserting the full chain described by the implementation plan: manifest
/// run-export -> wire `PackageSpec::PinCompatible` -> Jinja
/// `${{ pin_subpackage(...) }}` template -> rendered stage1
/// `Dependency::PinSubpackage` -> resolved `CondaOutputRunExports` with the
/// concrete version/build pin of the *same* output (since the manifest
/// describes exactly one output, "subpackage" can only mean "myself").
///
/// Also confirms `info/index.json`'s own `depends` (here, `run_dependencies`)
/// is unaffected: run-exports affect consumers, not the exporter itself.
#[tokio::test]
async fn test_conda_outputs_run_exports_self_pin_exact() {
    let mut weak_run_exports: OrderMap<SourcePackageName, PackageSpec> = OrderMap::new();
    weak_run_exports.insert(
        SourcePackageName::from(PackageName::new_unchecked("self-pin-pkg")),
        PackageSpec::PinCompatible(pixi_build_types::PinCompatibleSpec {
            lower_bound: None,
            upper_bound: None,
            exact: true,
            build: None,
        }),
    );

    let model = ProjectModel {
        name: Some("self-pin-pkg".to_string()),
        version: Some("1.2.3".parse().unwrap()),
        targets: Some(Targets {
            default_target: Some(Target {
                run_exports: Some(pixi_build_types::RunExportsTarget {
                    weak: Some(weak_run_exports),
                    ..Default::default()
                }),
                ..Target::default()
            }),
            conditional: None,
        }),
        ..ProjectModel::default()
    };

    let result = intermediate_conda_outputs::<TestGenerateRecipe>(
        Some(model),
        None,
        Platform::current(),
        None,
        None,
    )
    .await;

    let output = result.outputs.first().expect("should produce one output");

    // The self-pin resolves against this very output's own (name, version,
    // build_string), since a native manifest describes exactly one output.
    // Because the pinned package is itself a local source package (built from
    // `.`), the resolved pin comes back as a `Source` spec carrying the
    // source location alongside the exact version and build string.
    let weak = &output.run_exports.weak;
    assert_eq!(weak.len(), 1, "expected exactly one weak run-export");
    let pin = &weak[0];
    assert_eq!(pin.name.as_str(), "self-pin-pkg");
    match &pin.spec {
        PackageSpec::Source(source) => {
            assert_eq!(
                source.version.as_ref().map(|v| v.to_string()).as_deref(),
                Some("==1.2.3"),
                "exact pin should resolve to the output's own version"
            );
            assert!(
                source.build.is_some(),
                "exact pin should also carry the output's own build string matcher"
            );
        }
        other => panic!("expected a resolved Source pin, got {other:?}"),
    }

    // Other run-export buckets stay empty.
    assert!(output.run_exports.strong.is_empty());
    assert!(output.run_exports.noarch.is_empty());
    assert!(output.run_exports.weak_constrains.is_empty());
    assert!(output.run_exports.strong_constrains.is_empty());

    // The exporter's own `run_dependencies` (which feed `info/index.json`'s
    // `depends`) are unaffected by its run-exports.
    assert!(
        output.run_dependencies.depends.is_empty(),
        "run-exports must not leak into the exporting package's own dependencies"
    );
}

/// A `PinCompatible` wire entry whose name does *not* match the project's own
/// name is a `pin-compatible` dependency pin: the backend renders it as a
/// `${{ pin_compatible('<name>', ...) }}` Jinja item (manifest validation
/// guarantees `pin-subpackage` only ever appears under the own name), and the
/// conda_outputs path passes it through unresolved — resolution against the
/// host environment happens later, at build time, when that environment
/// exists.
#[tokio::test]
async fn test_conda_outputs_run_exports_pin_compatible_passthrough() {
    let mut weak_run_exports: OrderMap<SourcePackageName, PackageSpec> = OrderMap::new();
    weak_run_exports.insert(
        SourcePackageName::from(PackageName::new_unchecked("some-other-package")),
        PackageSpec::PinCompatible(pixi_build_types::PinCompatibleSpec {
            lower_bound: None,
            upper_bound: None,
            exact: true,
            build: None,
        }),
    );

    let model = ProjectModel {
        name: Some("self-pin-pkg".to_string()),
        version: Some("1.2.3".parse().unwrap()),
        targets: Some(Targets {
            default_target: Some(Target {
                run_exports: Some(pixi_build_types::RunExportsTarget {
                    weak: Some(weak_run_exports),
                    ..Default::default()
                }),
                ..Target::default()
            }),
            conditional: None,
        }),
        ..ProjectModel::default()
    };

    let result = intermediate_conda_outputs::<TestGenerateRecipe>(
        Some(model),
        None,
        Platform::current(),
        None,
        None,
    )
    .await;

    let output = result.outputs.first().expect("should produce one output");

    let weak = &output.run_exports.weak;
    assert_eq!(weak.len(), 1, "expected exactly one weak run-export");
    let pin = &weak[0];
    assert_eq!(pin.name.as_str(), "some-other-package");
    match &pin.spec {
        PackageSpec::PinCompatible(spec) => {
            assert!(
                spec.exact,
                "the pin's arguments must survive the round trip"
            );
        }
        other => panic!("expected an unresolved PinCompatible passthrough, got {other:?}"),
    }
}
