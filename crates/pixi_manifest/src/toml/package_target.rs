use toml_span::{DeserError, Value, de_helpers::TableHelper};

use crate::{
    KnownPreviewFeature, Preview, SpecType, TomlError,
    target::PackageTarget,
    toml::{TomlRunExports, target::combine_target_dependencies},
    utils::{PixiSpanned, package_map::UniquePackageMap},
};

#[derive(Debug, Default)]
pub struct TomlPackageTarget {
    pub run_dependencies: Option<PixiSpanned<UniquePackageMap>>,
    pub run_constraints: Option<PixiSpanned<UniquePackageMap>>,
    pub host_dependencies: Option<PixiSpanned<UniquePackageMap>>,
    pub build_dependencies: Option<PixiSpanned<UniquePackageMap>>,
    pub run_exports: Option<TomlRunExports>,
}

impl<'de> toml_span::Deserialize<'de> for TomlPackageTarget {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;
        let run_dependencies = th.optional("run-dependencies");
        let run_constraints = th.optional("run-constraints");
        let host_dependencies = th.optional("host-dependencies");
        let build_dependencies = th.optional("build-dependencies");
        let run_exports = th.optional("run-exports");
        th.finalize(None)?;
        Ok(TomlPackageTarget {
            run_dependencies,
            run_constraints,
            host_dependencies,
            build_dependencies,
            run_exports,
        })
    }
}

impl TomlPackageTarget {
    pub fn into_package_target(self, preview: &Preview) -> Result<PackageTarget, TomlError> {
        let is_pixi_build_enabled = preview.is_enabled(KnownPreviewFeature::PixiBuild);
        let run_exports = self
            .run_exports
            .map(|r| r.into_package_run_exports(is_pixi_build_enabled))
            .transpose()?
            .unwrap_or_default();
        Ok(PackageTarget {
            dependencies: combine_target_dependencies(
                [
                    (SpecType::Run, self.run_dependencies),
                    (SpecType::Host, self.host_dependencies),
                    (SpecType::Build, self.build_dependencies),
                    (SpecType::RunConstraints, self.run_constraints),
                ],
                is_pixi_build_enabled,
            )?,
            run_exports,
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
            .into_package_target(&Preview::default())
            .unwrap();

        let lookup = |spec_type: SpecType, name: &str| -> String {
            package_target
                .dependencies
                .get(&spec_type)
                .and_then(|d| d.get(&PackageName::from_str(name).unwrap()))
                .and_then(|s| s.iter().next())
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
    fn test_package_target_run_exports_all_buckets() {
        // All five run-exports buckets must land in the matching
        // `PackageRunExports` field on the resulting `PackageTarget`.
        let input = r#"
        [run-exports.weak]
        libzma = "==1.0"

        [run-exports.strong]
        libgcc = ">=12"

        [run-exports.noarch]
        python = ">=3.8"

        [run-exports.weak-constraints]
        weak-c = "==2.0"

        [run-exports.strong-constraints]
        strong-c = "==3.0"
        "#;

        let target = TomlPackageTarget::from_toml_str(input)
            .unwrap()
            .into_package_target(&Preview::default())
            .unwrap();

        let lookup = |bucket: &pixi_spec_containers::DependencyMap<
            PackageName,
            pixi_spec::PixiSpec,
        >,
                      name: &str| {
            bucket
                .get(&PackageName::from_str(name).unwrap())
                .and_then(|s| s.iter().next())
                .and_then(|s| s.as_version_spec())
                .map(|v| v.to_string())
                .unwrap_or_else(|| panic!("missing {name}"))
        };

        let r = &target.run_exports;
        assert_eq!(lookup(&r.weak, "libzma"), "==1.0");
        assert_eq!(lookup(&r.strong, "libgcc"), ">=12");
        assert_eq!(lookup(&r.noarch, "python"), ">=3.8");
        assert_eq!(lookup(&r.weak_constraints, "weak-c"), "==2.0");
        assert_eq!(lookup(&r.strong_constraints, "strong-c"), "==3.0");
    }

    #[test]
    fn test_package_target_run_exports_unknown_bucket() {
        // A typo like `stong` must be flagged so users don't silently lose
        // their run-exports.
        let input = r#"
        [run-exports.stong]
        libgcc = ">=12"
        "#;
        let err = TomlPackageTarget::from_toml_str(input).unwrap_err();
        assert_snapshot!(format_parse_error(input, err));
    }

    #[test]
    fn test_package_target_run_exports_source_spec_requires_pixi_build() {
        // Source specs in run-exports must obey the same pixi-build gate as
        // the dependency tables.
        use crate::KnownPreviewFeature;

        let input = r#"
        [run-exports.weak]
        own-package = { path = "." }
        "#;

        let err = TomlPackageTarget::from_toml_str(input)
            .unwrap()
            .into_package_target(&Preview::default())
            .unwrap_err();
        let rendered = format_parse_error(input, err);
        assert!(
            rendered.contains("pixi-build"),
            "expected pixi-build gating error, got: {rendered}"
        );

        // With pixi-build enabled, the same input parses.
        let preview = Preview::from_iter([KnownPreviewFeature::PixiBuild]);
        TomlPackageTarget::from_toml_str(input)
            .unwrap()
            .into_package_target(&preview)
            .expect("source specs in run-exports must be allowed when pixi-build is enabled");
    }
}
