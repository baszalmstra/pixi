//! Conversion functions from `pixi_spec` types to `pixi_build_types` types.
//! these are used to convert the `pixi_spec` types to the `pixi_build_types`
//! types we want to keep the conversion here, as we do not want
//! `pixi_build_types` to depend on `pixi_spec`
//!
//! This will mostly be boilerplate conversions but some of these are a bit more
//! interesting

use ordermap::OrderMap;
// Namespace to pbt, *please use* exclusively so we do not get confused between the two
// different types
use pixi_build_types::{self as pbt};

use pixi_manifest::{
    ManifestRunExports, PackageDependencySpec, PackageManifest, PackageTarget, TargetSelector,
};
use pixi_spec::{GitReference, MatchspecFields, PixiSpec, SourceLocationSpec, SpecConversionError};
use rattler_conda_types::{ChannelConfig, NamelessMatchSpec, PackageName};

/// Conversion from a `PixiSpec` to a `pbt::PixiSpecV1`.
fn to_pixi_spec_v1(
    spec: &PixiSpec,
    channel_config: &ChannelConfig,
) -> Result<pbt::PackageSpec, SpecConversionError> {
    // Convert into source or binary
    let source_or_binary = spec.clone().into_source_or_binary();
    // Convert into correct type for pixi
    let pbt_spec = match source_or_binary {
        itertools::Either::Left(source) => {
            let MatchspecFields {
                version,
                build,
                build_number,
                extras: None,
                flags: None,
                subdir,
                license,
                condition: None,
                track_features: None,
            } = source.matchspec.clone()
            else {
                unimplemented!(
                    "a particular field is not implemented in the pixi to pbt conversion"
                );
            };
            let location = match source.location {
                SourceLocationSpec::Url(url_spec) => {
                    let pixi_spec::UrlSpec {
                        url,
                        md5,
                        sha256,
                        subdirectory,
                    } = url_spec;
                    pbt::SourcePackageLocationSpec::Url(pbt::UrlSpec {
                        url,
                        md5,
                        sha256,
                        subdirectory: subdirectory.to_option_string(),
                    })
                }
                SourceLocationSpec::Git(git_spec) => {
                    let pixi_spec::GitLocationSpec {
                        git,
                        rev,
                        subdirectory,
                    } = git_spec;
                    pbt::SourcePackageLocationSpec::Git(pbt::GitSpec {
                        git,
                        rev: rev.map(|r| match r {
                            GitReference::Branch(b) => pbt::GitReference::Branch(b),
                            GitReference::Tag(t) => pbt::GitReference::Tag(t),
                            GitReference::Rev(rev) => pbt::GitReference::Rev(rev),
                            GitReference::DefaultBranch => pbt::GitReference::DefaultBranch,
                        }),
                        subdirectory: subdirectory.to_option_string(),
                    })
                }
                SourceLocationSpec::Path(path_spec) => {
                    pbt::SourcePackageLocationSpec::Path(pbt::PathSpec {
                        path: path_spec.path.to_string(),
                    })
                }
            };
            pbt::PackageSpec::Source(pbt::SourcePackageSpec {
                location,
                version,
                build,
                build_number,
                subdir,
                license,
            })
        }
        itertools::Either::Right(binary) => {
            let NamelessMatchSpec {
                version,
                build,
                build_number,
                file_name,
                extras,
                flags,
                channel,
                subdir,
                md5,
                sha256,
                url,
                license,
                condition,
                // `license_family` and `track_features` are deprecated matchspec
                // fields and `namespace` is unused, so they are not propagated.
                license_family: _,
                track_features: _,
                namespace: _,
            } = binary.try_into_nameless_match_spec(channel_config)?;
            pbt::BinaryPackageSpec {
                version,
                build,
                build_number,
                file_name,
                extras,
                flags,
                channel: channel.map(|c| c.base_url.url().clone().into()),
                subdir,
                md5,
                sha256,
                url,
                license,
                condition,
            }
            .into()
        }
    };
    Ok(pbt_spec)
}

/// Converts a single [`PackageDependencySpec`] to its wire form.
///
/// `PackageDependencySpec::Spec(_)` goes through the existing `PixiSpec`
/// conversion (`Binary`/`Source`); `PackageDependencySpec::PinSubpackage(_)`
/// and `PackageDependencySpec::PinCompatible(_)` both become a
/// `pbt::PackageSpec::PinCompatible`, using the infallible `Pin ->
/// PinCompatibleSpec` conversion (`pixi_spec::pin`, behind the
/// `pixi_build_types` feature). The wire variant is shared; the backend
/// recovers the distinction by comparing the entry's name to the package's
/// own name (manifest validation guarantees `pin-subpackage` is only ever
/// recorded under the own name and `pin-compatible` never is).
fn to_package_dependency_spec_v1(
    spec: &PackageDependencySpec,
    channel_config: &ChannelConfig,
) -> Result<pbt::PackageSpec, SpecConversionError> {
    match spec {
        PackageDependencySpec::Spec(spec) => to_pixi_spec_v1(spec, channel_config),
        PackageDependencySpec::PinSubpackage(pin) | PackageDependencySpec::PinCompatible(pin) => {
            Ok(pbt::PackageSpec::PinCompatible(pin.into()))
        }
    }
}

/// Converts an iterator of `PackageName` and `PackageDependencySpec` (the
/// package-level dependency value type, which may carry a `pin-subpackage`
/// entry) to a `IndexMap<String, pbt::PackageSpec>`.
fn to_pbt_package_dependencies<'a>(
    iter: impl Iterator<Item = (&'a PackageName, &'a PackageDependencySpec)>,
    channel_config: &ChannelConfig,
) -> Result<OrderMap<pbt::SourcePackageName, pbt::PackageSpec>, SpecConversionError> {
    iter.map(|(name, spec)| {
        let converted = to_package_dependency_spec_v1(spec, channel_config)?;
        Ok((pbt::SourcePackageName::from(name.clone()), converted))
    })
    .collect()
}

/// Converts a [`ManifestRunExports`] to a [`pbt::RunExportsTarget`].
fn to_run_exports_v1(
    run_exports: &ManifestRunExports,
    channel_config: &ChannelConfig,
) -> Result<pbt::RunExportsTarget, SpecConversionError> {
    Ok(pbt::RunExportsTarget {
        noarch: Some(to_pbt_package_dependencies(
            run_exports.noarch.iter_specs(),
            channel_config,
        )?),
        strong: Some(to_pbt_package_dependencies(
            run_exports.strong.iter_specs(),
            channel_config,
        )?),
        weak: Some(to_pbt_package_dependencies(
            run_exports.weak.iter_specs(),
            channel_config,
        )?),
        strong_constrains: Some(to_pbt_package_dependencies(
            run_exports.strong_constrains.iter_specs(),
            channel_config,
        )?),
        weak_constrains: Some(to_pbt_package_dependencies(
            run_exports.weak_constrains.iter_specs(),
            channel_config,
        )?),
    })
}

/// Converts a [`PackageTarget`] to a [`pbt::Target`].
fn to_target_v1(
    target: &PackageTarget,
    channel_config: &ChannelConfig,
) -> Result<pbt::Target, SpecConversionError> {
    // Difference for us is that [`pbt::TargetV1`] has split the host, run and build
    // dependencies into separate fields, so we need to split them up here
    let extra_dependencies = if target.extra_dependencies.is_empty() {
        None
    } else {
        Some(
            target
                .extra_dependencies
                .iter()
                .map(|(name, deps)| {
                    to_pbt_package_dependencies(deps.iter_specs(), channel_config)
                        .map(|dependencies| (name.clone(), dependencies))
                })
                .collect::<Result<_, _>>()?,
        )
    };
    let run_exports = target
        .run_exports
        .as_ref()
        .map(|run_exports| to_run_exports_v1(run_exports, channel_config))
        .transpose()?;
    Ok(pbt::Target {
        host_dependencies: Some(
            target
                .host_dependencies()
                .map(|deps| to_pbt_package_dependencies(deps.iter_specs(), channel_config))
                .transpose()?
                .unwrap_or_default(),
        ),
        build_dependencies: Some(
            target
                .build_dependencies()
                .map(|deps| to_pbt_package_dependencies(deps.iter_specs(), channel_config))
                .transpose()?
                .unwrap_or_default(),
        ),
        run_dependencies: Some(
            target
                .run_dependencies()
                .map(|deps| to_pbt_package_dependencies(deps.iter_specs(), channel_config))
                .transpose()?
                .unwrap_or_default(),
        ),
        run_constraints: Some(
            target
                .run_constraints()
                .map(|deps| to_pbt_package_dependencies(deps.iter_specs(), channel_config))
                .transpose()?
                .unwrap_or_default(),
        ),
        extra_dependencies,
        run_exports,
    })
}

/// Converts a manifest [`TargetSelector`] to its wire form. Only used for the
/// per-target backend configuration (`[package.build.target.<selector>]`);
/// dependencies carry conditional expressions instead.
pub fn to_target_selector_v1(
    selector: &TargetSelector,
) -> Result<pbt::TargetSelector, SpecConversionError> {
    Ok(match selector {
        TargetSelector::Platform(platform) => pbt::TargetSelector::Platform(platform.to_string()),
        TargetSelector::Subdir(subdir) => pbt::TargetSelector::Subdir(subdir.to_string()),
        TargetSelector::Unix => pbt::TargetSelector::Unix,
        TargetSelector::Linux => pbt::TargetSelector::Linux,
        TargetSelector::Win => pbt::TargetSelector::Win,
        TargetSelector::MacOs => pbt::TargetSelector::MacOs,
        // Package targets resolve by subdir through the build-types protocol,
        // which has no wildcard concept; glob keys are rejected upstream while
        // parsing the manifest, so this is an unreachable backstop.
        TargetSelector::PlatformGlob(glob) => {
            return Err(SpecConversionError::WildcardTargetSelector(
                glob.as_str().to_string(),
            ));
        }
    })
}

fn to_targets_v1(
    manifest: &PackageManifest,
    channel_config: &ChannelConfig,
) -> Result<pbt::Targets, SpecConversionError> {
    // Conditional `if(...)` dependencies are not platform selectors; they are
    // carried separately and passed through to rattler-build, which evaluates
    // the expression. The deprecated `[package.target.*]` tables are already
    // lowered into conditional dependencies at parse time.
    let conditional = manifest
        .conditional_dependencies
        .iter()
        .map(|(expression, target)| {
            to_target_v1(target, channel_config).map(|target| (expression.clone(), target))
        })
        .collect::<Result<OrderMap<pbt::ConditionalExpression, pbt::Target>, _>>()?;

    Ok(pbt::Targets {
        default_target: Some(to_target_v1(&manifest.dependencies, channel_config)?),
        conditional: (!conditional.is_empty()).then_some(conditional),
    })
}

/// Converts a [`PackageManifest`] to a [`pbt::ProjectModel`].
pub fn to_project_model_v1(
    manifest: &PackageManifest,
    channel_config: &ChannelConfig,
) -> Result<pbt::ProjectModel, SpecConversionError> {
    let project = pbt::ProjectModel {
        name: manifest.package.name.clone(),
        build_string_prefix: manifest.build.build_string_prefix.clone(),
        build_number: manifest.build.build_number,
        version: manifest.package.version.clone(),
        description: manifest.package.description.clone(),
        build_flags: (!manifest.build.flags.is_empty()).then(|| manifest.build.flags.clone()),
        authors: manifest.package.authors.clone(),
        license: manifest.package.license.clone(),
        license_file: manifest.package.license_file.clone(),
        readme: manifest.package.readme.clone(),
        homepage: manifest.package.homepage.clone(),
        repository: manifest.package.repository.clone(),
        documentation: manifest.package.documentation.clone(),
        targets: Some(to_targets_v1(manifest, channel_config)?),
        secrets: manifest.build.secrets.clone(),
    };
    Ok(project)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pixi_manifest::Preview;
    use pixi_manifest::toml::{
        FromTomlStr, PackageDefaults, TomlPackage, WorkspacePackageProperties,
    };
    use rattler_conda_types::ChannelConfig;
    use rstest::rstest;

    use super::pbt;

    fn some_channel_config() -> ChannelConfig {
        ChannelConfig {
            channel_alias: "http://prefix.dev".parse().unwrap(),
            root_dir: PathBuf::from("/tmp"),
        }
    }

    /// Use a macro so that the snapshot test is inlined into the function
    /// this makes insta use the name of the function as the snapshot name
    /// instead of this generic name
    macro_rules! snapshot_test {
        ($manifest_path:expr) => {{
            use std::ffi::OsStr;

            let manifest = pixi_manifest::Manifests::from_workspace_manifest_path($manifest_path)
                .expect("could not load manifest")
                .value;
            if let Some(package_manifest) = manifest.package {
                // To create different snapshot files for the same function
                let name = package_manifest
                    .provenance
                    .path
                    .parent()
                    .unwrap()
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap();

                // Convert the manifest to the project model
                let project_model =
                    super::to_project_model_v1(&package_manifest.value, &some_channel_config())
                        .unwrap();
                let mut settings = insta::Settings::clone_current();
                settings.set_snapshot_suffix(name);
                settings.bind(|| {
                    insta::assert_json_snapshot!(project_model);
                });
            }
        }};
    }

    #[rstest]
    #[test]
    fn test_conversions_v1_examples(
        #[files("../../examples/pixi-build/*/pixi.toml")] manifest_path: PathBuf,
    ) {
        snapshot_test!(manifest_path);
    }

    #[rstest]
    #[test]
    fn test_conversions_v1_docs(
        #[files("../../docs/source_files/pixi_workspaces/pixi_build/*/pixi.toml")]
        manifest_path: PathBuf,
    ) {
        snapshot_test!(manifest_path);
    }

    #[test]
    fn test_package_extras_are_converted_to_project_model() {
        let input = r#"
        name = "example"
        version = "0.1.0"

        [build]
        backend = { name = "pixi-build-rattler-build", version = "0.3.*" }

        [extra-dependencies.test]
        gtest = "*"
        "#;

        let manifest = TomlPackage::from_toml_str(input)
            .unwrap()
            .into_manifest(
                WorkspacePackageProperties::default(),
                PackageDefaults::default(),
                &Preview::default(),
                std::path::Path::new(""),
            )
            .unwrap()
            .value;

        let project_model = super::to_project_model_v1(&manifest, &some_channel_config()).unwrap();
        let extras = project_model
            .targets
            .expect("targets are forwarded")
            .default_target
            .expect("default target is forwarded")
            .extra_dependencies
            .expect("extras are forwarded");
        let test_extra = extras.get("test").expect("test extra exists");

        assert!(test_extra.keys().any(|name| name.as_str() == "gtest"));
    }

    #[test]
    fn test_package_build_flags_are_converted_to_project_model() {
        let input = r#"
        name = "example"
        version = "0.1.0"

        [build]
        backend = { name = "pixi-build-rattler-build", version = "0.3.*" }
        flags = ["cuda", "blas_openblas"]
        "#;

        let manifest = TomlPackage::from_toml_str(input)
            .unwrap()
            .into_manifest(
                WorkspacePackageProperties::default(),
                PackageDefaults::default(),
                &Preview::default(),
                std::path::Path::new(""),
            )
            .unwrap()
            .value;

        let project_model = super::to_project_model_v1(&manifest, &some_channel_config()).unwrap();
        let flags = project_model
            .build_flags
            .expect("build flags are forwarded");
        let flags = flags.iter().map(|flag| flag.as_str()).collect::<Vec<_>>();

        assert_eq!(flags, vec!["cuda", "blas_openblas"]);
    }

    /// Regression test: `to_target_v1` must propagate `[package.run-constraints]`
    /// (the `SpecType::RunConstraints` bucket on `PackageTarget`) into the
    /// `pbt::Target.run_constraints` field. A previous version dropped them
    /// silently because `to_target_v1` only mapped run/host/build.
    #[test]
    fn test_to_target_v1_run_constraints() {
        use std::str::FromStr;

        use pixi_manifest::{DependencyOverwriteBehavior, PackageTarget, SpecType};
        use pixi_spec::PixiSpec;
        use rattler_conda_types::{PackageName, ParseStrictness, VersionSpec};

        let mut package_target = PackageTarget::default();
        let constrained = PackageName::from_str("constrained").unwrap();
        let spec = PixiSpec::from(VersionSpec::from_str(">=1.0", ParseStrictness::Strict).unwrap());
        package_target
            .try_add_dependency(
                &constrained,
                &spec,
                SpecType::RunConstraints,
                DependencyOverwriteBehavior::Error,
            )
            .unwrap();

        let target = super::to_target_v1(&package_target, &some_channel_config()).unwrap();

        let constraints = target
            .run_constraints
            .expect("run_constraints should be Some");
        assert_eq!(constraints.len(), 1);
        let (name, converted) = constraints.iter().next().unwrap();
        assert_eq!(name.as_str(), "constrained");
        match converted {
            pbt::PackageSpec::Binary(binary) => assert_eq!(
                binary.version.as_ref().unwrap().to_string(),
                ">=1.0",
                "expected version spec to round-trip",
            ),
            other => panic!("expected Binary spec, got {other:?}"),
        }

        // Confirm the other buckets stay empty so we know we routed only to constraints.
        assert!(target.run_dependencies.unwrap().is_empty());
        assert!(target.host_dependencies.unwrap().is_empty());
        assert!(target.build_dependencies.unwrap().is_empty());
    }

    /// `to_target_v1` must convert a `pin-subpackage` entry in a regular
    /// dependency table into a `pbt::PackageSpec::PinCompatible`, and an
    /// ordinary spec alongside it must still come through as `Binary`
    /// (regression guard: the shared conversion function must not affect the
    /// non-pin path).
    #[test]
    fn test_to_target_v1_pin_subpackage_and_ordinary_spec() {
        let input = r#"
        name = "mypkg"
        version = "1.0"

        [build]
        backend = { name = "bla", version = "1.0" }

        [run-dependencies]
        mypkg = { pin-subpackage = { lower-bound = "x.x", build = "py*" } }
        numpy = ">=1.0"
        "#;

        let manifest = TomlPackage::from_toml_str(input)
            .unwrap()
            .into_manifest(
                WorkspacePackageProperties::default(),
                PackageDefaults::default(),
                &Preview::default(),
                std::path::Path::new(""),
            )
            .unwrap()
            .value;

        let target = super::to_target_v1(&manifest.dependencies, &some_channel_config()).unwrap();
        let run_deps = target.run_dependencies.expect("run_dependencies present");

        let mypkg = run_deps
            .iter()
            .find(|(name, _)| name.as_str() == "mypkg")
            .map(|(_, spec)| spec)
            .expect("mypkg dependency present");
        match mypkg {
            pbt::PackageSpec::PinCompatible(pin) => {
                assert!(!pin.exact);
                assert_eq!(pin.build.as_deref(), Some("py*"));
                assert!(pin.lower_bound.is_some());
            }
            other => panic!("expected PinCompatible spec, got {other:?}"),
        }

        let numpy = run_deps
            .iter()
            .find(|(name, _)| name.as_str() == "numpy")
            .map(|(_, spec)| spec)
            .expect("numpy dependency present");
        match numpy {
            pbt::PackageSpec::Binary(binary) => {
                assert_eq!(binary.version.as_ref().unwrap().to_string(), ">=1.0");
            }
            other => panic!("expected Binary spec, got {other:?}"),
        }
    }

    /// A `pin-compatible` entry maps to the same shared wire variant
    /// (`pbt::PackageSpec::PinCompatible`) as `pin-subpackage`; the backend
    /// tells them apart by comparing the entry name to the package's own name.
    #[test]
    fn test_to_target_v1_pin_compatible() {
        let input = r#"
        name = "mypkg"
        version = "1.0"

        [build]
        backend = { name = "bla", version = "1.0" }

        [run-dependencies]
        numpy = { pin-compatible = { lower-bound = "x.x" } }
        "#;

        let manifest = TomlPackage::from_toml_str(input)
            .unwrap()
            .into_manifest(
                WorkspacePackageProperties::default(),
                PackageDefaults::default(),
                &Preview::default(),
                std::path::Path::new(""),
            )
            .unwrap()
            .value;

        let target = super::to_target_v1(&manifest.dependencies, &some_channel_config()).unwrap();
        let run_deps = target.run_dependencies.expect("run_dependencies present");
        let numpy = run_deps
            .iter()
            .find(|(name, _)| name.as_str() == "numpy")
            .map(|(_, spec)| spec)
            .expect("numpy dependency present");
        match numpy {
            pbt::PackageSpec::PinCompatible(pin) => {
                assert!(!pin.exact);
                assert!(pin.lower_bound.is_some());
                assert_eq!(pin.upper_bound, None);
            }
            other => panic!("expected PinCompatible spec, got {other:?}"),
        }
    }

    /// `to_run_exports_v1` must propagate every bucket of
    /// `[package.run-exports.*]` into the matching `pbt::RunExportsTarget`
    /// field, including a self-referential `pin-subpackage` shorthand.
    #[test]
    fn test_to_run_exports_v1_all_buckets() {
        let input = r#"
        name = "mypkg"
        version = "1.0"

        [build]
        backend = { name = "bla", version = "1.0" }

        [run-exports.weak]
        mypkg = { pin-subpackage = true }

        [run-exports.strong]
        libzma = "*"
        "#;

        let manifest = TomlPackage::from_toml_str(input)
            .unwrap()
            .into_manifest(
                WorkspacePackageProperties::default(),
                PackageDefaults::default(),
                &Preview::default(),
                std::path::Path::new(""),
            )
            .unwrap()
            .value;

        let target = super::to_target_v1(&manifest.dependencies, &some_channel_config()).unwrap();
        let run_exports = target.run_exports.expect("run_exports present");

        let weak = run_exports.weak.expect("weak bucket present");
        let (_, mypkg_spec) = weak
            .iter()
            .find(|(name, _)| name.as_str() == "mypkg")
            .expect("mypkg present in weak run-exports");
        match mypkg_spec {
            pbt::PackageSpec::PinCompatible(pin) => assert!(pin.exact),
            other => panic!("expected PinCompatible spec, got {other:?}"),
        }

        let strong = run_exports.strong.expect("strong bucket present");
        assert!(strong.iter().any(|(name, _)| name.as_str() == "libzma"));
    }
}
