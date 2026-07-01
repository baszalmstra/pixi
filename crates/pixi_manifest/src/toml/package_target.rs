use std::collections::HashMap;

use indexmap::IndexMap;
use pixi_build_types::ExtraGroupName;
use pixi_spec::TomlSpec;
use pixi_spec_containers::DependencyMap;
use pixi_toml::{Same, TomlIndexMap, TomlWith};
use rattler_conda_types::PackageName;
use toml_span::{DeserError, Value, de_helpers::TableHelper};

use crate::{
    KnownPreviewFeature, PackageDependencySpec, Preview, SpecType, TomlError,
    error::GenericError,
    target::PackageTarget,
    utils::{PixiSpanned, inheritable_package_map::InheritablePackageMap},
};

#[derive(Debug, Default)]
pub struct TomlPackageTarget {
    pub run_dependencies: Option<PixiSpanned<InheritablePackageMap>>,
    pub run_constraints: Option<PixiSpanned<InheritablePackageMap>>,
    pub host_dependencies: Option<PixiSpanned<InheritablePackageMap>>,
    pub build_dependencies: Option<PixiSpanned<InheritablePackageMap>>,
    pub extra_dependencies: IndexMap<PixiSpanned<String>, PixiSpanned<InheritablePackageMap>>,
}

/// Combines different package target dependencies into a single map, mirroring
/// `toml::target::combine_target_dependencies` but operating on
/// [`InheritablePackageMap`]/[`PackageDependencySpec`] (package-level) instead
/// of `UniquePackageMap`/`PixiSpec` (workspace-level).
///
/// Every `pin-subpackage`/`pin-compatible` entry is validated against
/// `package_name` inside `ResolvedPackageMap::into_inner`.
fn combine_package_target_dependencies(
    iter: impl IntoIterator<Item = (SpecType, Option<PixiSpanned<InheritablePackageMap>>)>,
    workspace_dependencies: &IndexMap<PackageName, TomlSpec>,
    is_pixi_build_enabled: bool,
    package_name: &str,
) -> Result<HashMap<SpecType, DependencyMap<PackageName, PackageDependencySpec>>, TomlError> {
    iter.into_iter()
        .filter_map(|(ty, deps)| {
            deps.map(|deps| {
                deps.value
                    .resolve(workspace_dependencies, is_pixi_build_enabled)
                    .and_then(|resolved| resolved.into_inner(package_name, is_pixi_build_enabled))
                    .map(|index_map| {
                        let dep_map: DependencyMap<PackageName, PackageDependencySpec> =
                            index_map.into_iter().collect();
                        (ty, dep_map)
                    })
            })
        })
        .collect()
}

impl<'de> toml_span::Deserialize<'de> for TomlPackageTarget {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;
        let run_dependencies = th.optional("run-dependencies");
        let run_constraints = th.optional("run-constraints");
        let host_dependencies = th.optional("host-dependencies");
        let build_dependencies = th.optional("build-dependencies");
        let extra_dependencies = th
            .optional::<TomlWith<_, TomlIndexMap<_, Same>>>("extra-dependencies")
            .map(TomlWith::into_inner)
            .unwrap_or_default();
        th.finalize(None)?;
        Ok(TomlPackageTarget {
            run_dependencies,
            run_constraints,
            host_dependencies,
            build_dependencies,
            extra_dependencies,
        })
    }
}

impl TomlPackageTarget {
    /// Converts into a [`PackageTarget`], validating every
    /// `pin-subpackage`/`pin-compatible` entry against `package_name` (the
    /// package's resolved name, which may come from workspace inheritance).
    pub fn into_package_target_named(
        self,
        preview: &Preview,
        workspace_dependencies: &IndexMap<PackageName, TomlSpec>,
        package_name: &str,
    ) -> Result<PackageTarget, TomlError> {
        let pixi_build_enabled = preview.is_enabled(KnownPreviewFeature::PixiBuild);

        let extra_dependencies = self
            .extra_dependencies
            .into_iter()
            .map(|(name, dependencies)| {
                let PixiSpanned { value: name, span } = name;
                let group = ExtraGroupName::new(name).map_err(|err| {
                    TomlError::Generic(
                        GenericError::new(err.to_string())
                            .with_opt_span(span)
                            .with_span_label("invalid extra dependency group name"),
                    )
                })?;
                let resolved = dependencies
                    .value
                    .resolve(workspace_dependencies, pixi_build_enabled)?;
                let dep_map = resolved
                    .into_inner(package_name, pixi_build_enabled)?
                    .into_iter()
                    .collect();
                Ok::<_, TomlError>((group, dep_map))
            })
            .collect::<Result<_, _>>()?;

        Ok(PackageTarget {
            dependencies: combine_package_target_dependencies(
                [
                    (SpecType::Run, self.run_dependencies),
                    (SpecType::Host, self.host_dependencies),
                    (SpecType::Build, self.build_dependencies),
                    (SpecType::RunConstraints, self.run_constraints),
                ],
                workspace_dependencies,
                pixi_build_enabled,
                package_name,
            )?,
            extra_dependencies,
            run_exports: None,
        })
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use insta::assert_snapshot;
    use pixi_test_utils::format_parse_error;
    use rattler_conda_types::PackageName;

    use super::*;
    use crate::toml::FromTomlStr;

    #[test]
    fn test_package_target_all_dependency_types() {
        // All four dependency tables on a package target must end up in the
        // matching SpecType bucket.
        let input = r#"
        [run-dependencies]
        run-dep = "==1.0"

        [host-dependencies]
        host-dep = "==2.0"

        [build-dependencies]
        build-dep = "==3.0"

        [run-constraints]
        constrained = ">=4.0"
        "#;

        let package_target = TomlPackageTarget::from_toml_str(input)
            .unwrap()
            .into_package_target_named(&Preview::default(), &IndexMap::new(), "own-package")
            .unwrap();

        let lookup = |spec_type: SpecType, name: &str| -> String {
            package_target
                .dependencies
                .get(&spec_type)
                .and_then(|d| d.get(&PackageName::from_str(name).unwrap()))
                .and_then(|s| s.iter().next())
                .and_then(|s| s.as_spec())
                .and_then(|s| s.as_version_spec())
                .map(|v| v.to_string())
                .unwrap_or_else(|| panic!("missing {name} in {spec_type:?}"))
        };

        assert_eq!(lookup(SpecType::Run, "run-dep"), "==1.0");
        assert_eq!(lookup(SpecType::Host, "host-dep"), "==2.0");
        assert_eq!(lookup(SpecType::Build, "build-dep"), "==3.0");
        assert_eq!(lookup(SpecType::RunConstraints, "constrained"), ">=4.0");
    }

    #[test]
    fn test_package_target_unknown_key() {
        // A typo like `run-constraint` (singular) must be flagged so users
        // don't silently lose their constraints.
        let input = r#"
        [run-constraint]
        oops = "==1.0"
        "#;
        let err = TomlPackageTarget::from_toml_str(input).unwrap_err();
        assert_snapshot!(format_parse_error(input, err));
    }

    #[test]
    fn test_invalid_extra_group_name_is_rejected() {
        // Extra group names follow the extras naming
        // scheme `^[a-z0-9._+-]{1,64}$`; an uppercase name is rejected with a
        // spanned error rather than silently producing invalid v3 metadata.
        let input = r#"
        [extra-dependencies.Invalid]
        gtest = "*"
        "#;
        let err = TomlPackageTarget::from_toml_str(input)
            .unwrap()
            .into_package_target_named(&Preview::default(), &IndexMap::new(), "own-package")
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("extra") && message.contains("invalid character"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn test_pin_subpackage_in_package_target_dependency_tables() {
        // `pin-subpackage` must parse in every package-target dependency table
        // (run/host/build-dependencies; package targets don't have a plain
        // `[dependencies]` table of their own).
        let input = r#"
        [run-dependencies]
        own-package = { pin-subpackage = true }

        [host-dependencies]
        own-package = { pin-subpackage = { lower-bound = "x.x" } }

        [build-dependencies]
        own-package = { pin-subpackage = true }
        "#;

        let package_target = TomlPackageTarget::from_toml_str(input)
            .unwrap()
            .into_package_target_named(&Preview::default(), &IndexMap::new(), "own-package")
            .unwrap();

        let own_package = PackageName::from_str("own-package").unwrap();
        for spec_type in [SpecType::Run, SpecType::Host, SpecType::Build] {
            let spec = package_target
                .dependencies
                .get(&spec_type)
                .and_then(|d| d.get(&own_package))
                .and_then(|s| s.iter().next())
                .unwrap_or_else(|| panic!("missing own-package in {spec_type:?}"));
            assert!(
                spec.as_pin_subpackage().is_some(),
                "expected a pin-subpackage entry in {spec_type:?}, got {spec:?}"
            );
        }
    }

    #[test]
    fn test_pin_compatible_in_package_target_dependency_tables() {
        // `pin-compatible` must parse in package-target dependency tables and
        // must reference a name *other* than the package's own.
        let input = r#"
        [run-dependencies]
        numpy = { pin-compatible = { lower-bound = "x.x" } }

        [host-dependencies]
        numpy = { pin-compatible = true }
        "#;

        let package_target = TomlPackageTarget::from_toml_str(input)
            .unwrap()
            .into_package_target_named(&Preview::default(), &IndexMap::new(), "own-package")
            .unwrap();

        let numpy = PackageName::from_str("numpy").unwrap();
        for spec_type in [SpecType::Run, SpecType::Host] {
            let spec = package_target
                .dependencies
                .get(&spec_type)
                .and_then(|d| d.get(&numpy))
                .and_then(|s| s.iter().next())
                .unwrap_or_else(|| panic!("missing numpy in {spec_type:?}"));
            assert!(
                spec.as_pin_compatible().is_some(),
                "expected a pin-compatible entry in {spec_type:?}, got {spec:?}"
            );
        }
    }
}
