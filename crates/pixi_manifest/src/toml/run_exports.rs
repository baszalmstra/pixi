use indexmap::IndexMap;
use pixi_spec::TomlSpec;
use rattler_conda_types::PackageName;
use toml_span::{DeserError, Value, de_helpers::TableHelper};

use crate::{
    KnownPreviewFeature, Preview, TomlError,
    run_exports::ManifestRunExports,
    target::key_looks_conditional,
    utils::{PixiSpanned, inheritable_package_map::InheritablePackageMap},
};

/// The TOML representation of `[package.run-exports]`.
///
/// Conditional `[package.run-exports.<bucket>."if(...)"]` sub-tables are not
/// supported in this version; [`reject_conditional_keys`] produces a clear
/// error instead of letting the unrecognized key fail as an "invalid package
/// name", or silently dropping it.
#[derive(Debug, Default)]
pub struct TomlRunExports {
    pub noarch: Option<PixiSpanned<InheritablePackageMap>>,
    pub strong: Option<PixiSpanned<InheritablePackageMap>>,
    pub weak: Option<PixiSpanned<InheritablePackageMap>>,
    pub strong_constrains: Option<PixiSpanned<InheritablePackageMap>>,
    pub weak_constrains: Option<PixiSpanned<InheritablePackageMap>>,
}

/// Parses one `[package.run-exports.<bucket>]` sub-table, rejecting any
/// `if(...)`-shaped key before delegating to [`InheritablePackageMap`] (which
/// would otherwise reject it with a confusing "invalid package name" error,
/// since it tries to parse every key as a [`PackageName`]).
fn parse_bucket<'de>(
    value: &mut Value<'de>,
) -> Result<PixiSpanned<InheritablePackageMap>, DeserError> {
    reject_conditional_keys(value)?;
    let span = value.span;
    let map = <InheritablePackageMap as toml_span::Deserialize>::deserialize(value)?;
    Ok(PixiSpanned {
        value: map,
        span: Some(span.start..span.end),
    })
}

fn reject_conditional_keys(value: &Value<'_>) -> Result<(), DeserError> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    for key in table.keys() {
        if key_looks_conditional(&key.name) {
            return Err(toml_span::Error {
                kind: toml_span::ErrorKind::Custom(
                    "conditional `if(...)` sub-tables are not yet supported in \
                     `[package.run-exports.*]`"
                        .into(),
                ),
                span: key.span,
                line_info: None,
            }
            .into());
        }
    }
    Ok(())
}

/// Takes `name` from the helper's table (if present) and parses it as a
/// run-exports bucket.
fn take_bucket<'de>(
    th: &mut TableHelper<'de>,
    name: &'static str,
) -> Result<Option<PixiSpanned<InheritablePackageMap>>, DeserError> {
    th.take(name)
        .map(|(_, mut value)| parse_bucket(&mut value))
        .transpose()
}

impl<'de> toml_span::Deserialize<'de> for TomlRunExports {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;

        let noarch = take_bucket(&mut th, "noarch")?;
        let strong = take_bucket(&mut th, "strong")?;
        let weak = take_bucket(&mut th, "weak")?;
        let strong_constrains = take_bucket(&mut th, "strong-constrains")?;
        let weak_constrains = take_bucket(&mut th, "weak-constrains")?;
        th.finalize(None)?;

        Ok(TomlRunExports {
            noarch,
            strong,
            weak,
            strong_constrains,
            weak_constrains,
        })
    }
}

impl TomlRunExports {
    /// `package_name` is the package's resolved name (it may come from
    /// workspace inheritance); every `pin-subpackage`/`pin-compatible` entry
    /// is validated against it inside `ResolvedPackageMap::into_inner`.
    pub fn into_manifest_run_exports(
        self,
        preview: &Preview,
        workspace_dependencies: &IndexMap<PackageName, TomlSpec>,
        package_name: &str,
    ) -> Result<ManifestRunExports, TomlError> {
        let pixi_build_enabled = preview.is_enabled(KnownPreviewFeature::PixiBuild);

        let resolve = |entry: Option<PixiSpanned<InheritablePackageMap>>| -> Result<
            pixi_spec_containers::DependencyMap<PackageName, crate::PackageDependencySpec>,
            TomlError,
        > {
            let Some(PixiSpanned { value, .. }) = entry else {
                return Ok(Default::default());
            };
            let resolved = value.resolve(workspace_dependencies, pixi_build_enabled)?;
            let resolved = resolved.into_inner(package_name, pixi_build_enabled)?;
            Ok(resolved.into_iter().collect())
        };

        Ok(ManifestRunExports {
            noarch: resolve(self.noarch)?,
            strong: resolve(self.strong)?,
            weak: resolve(self.weak)?,
            strong_constrains: resolve(self.strong_constrains)?,
            weak_constrains: resolve(self.weak_constrains)?,
        })
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use pixi_test_utils::format_parse_error;
    use rattler_conda_types::PackageName;

    use super::*;
    use crate::{Preview, toml::FromTomlStr};

    #[test]
    fn test_run_exports_all_buckets_parse() {
        let input = r#"
        [strong]
        libzma = "*"

        [weak]
        libcurl = ">=8"

        [noarch]
        python = "*"

        [strong-constrains]
        some-pkg = ">=1.0"

        [weak-constrains]
        other-pkg = "<2.0"
        "#;

        let toml = TomlRunExports::from_toml_str(input).unwrap();
        let run_exports = toml
            .into_manifest_run_exports(&Preview::default(), &IndexMap::new(), "own-package")
            .unwrap();

        let libzma = PackageName::from_str("libzma").unwrap();
        assert!(run_exports.strong.get(&libzma).is_some());
        let libcurl = PackageName::from_str("libcurl").unwrap();
        assert!(run_exports.weak.get(&libcurl).is_some());
        let python = PackageName::from_str("python").unwrap();
        assert!(run_exports.noarch.get(&python).is_some());
        let some_pkg = PackageName::from_str("some-pkg").unwrap();
        assert!(run_exports.strong_constrains.get(&some_pkg).is_some());
        let other_pkg = PackageName::from_str("other-pkg").unwrap();
        assert!(run_exports.weak_constrains.get(&other_pkg).is_some());
    }

    #[test]
    fn test_run_exports_unknown_bucket_rejected() {
        let input = r#"
        [extra-strong]
        foo = "*"
        "#;
        let err = TomlRunExports::from_toml_str(input).unwrap_err();
        let rendered = format_parse_error(input, err);
        assert!(
            rendered.contains("Unexpected keys") && rendered.contains("extra-strong"),
            "unexpected error message: {rendered}"
        );
    }

    #[test]
    fn test_run_exports_pin_subpackage_value() {
        let input = r#"
        [strong]
        own-package = { pin-subpackage = true }
        "#;
        let toml = TomlRunExports::from_toml_str(input).unwrap();
        let run_exports = toml
            .into_manifest_run_exports(&Preview::default(), &IndexMap::new(), "own-package")
            .unwrap();
        let own_package = PackageName::from_str("own-package").unwrap();
        let spec = run_exports
            .strong
            .get(&own_package)
            .and_then(|s| s.iter().next())
            .unwrap();
        assert!(spec.as_pin_subpackage().is_some());
    }

    #[test]
    fn test_run_exports_pin_subpackage_wrong_name_rejected() {
        let input = r#"
        [strong]
        other-package = { pin-subpackage = true }
        "#;
        let toml = TomlRunExports::from_toml_str(input).unwrap();
        let err = toml
            .into_manifest_run_exports(&Preview::default(), &IndexMap::new(), "own-package")
            .expect_err("must reject pin-subpackage referencing another package");
        let message = format!("{err}");
        assert!(
            message.contains("own-package") && message.contains("other-package"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn test_run_exports_pin_compatible_value() {
        let input = r#"
        [weak]
        numpy = { pin-compatible = { lower-bound = "x.x" } }
        "#;
        let toml = TomlRunExports::from_toml_str(input).unwrap();
        let run_exports = toml
            .into_manifest_run_exports(&Preview::default(), &IndexMap::new(), "own-package")
            .unwrap();
        let numpy = PackageName::from_str("numpy").unwrap();
        let spec = run_exports
            .weak
            .get(&numpy)
            .and_then(|s| s.iter().next())
            .unwrap();
        assert!(spec.as_pin_compatible().is_some());
    }

    #[test]
    fn test_run_exports_pin_compatible_own_name_rejected() {
        let input = r#"
        [weak]
        own-package = { pin-compatible = true }
        "#;
        let toml = TomlRunExports::from_toml_str(input).unwrap();
        let err = toml
            .into_manifest_run_exports(&Preview::default(), &IndexMap::new(), "own-package")
            .expect_err("must reject pin-compatible referencing the package itself");
        let message = format!("{err}");
        assert!(
            message.contains(
                "`pin-compatible` cannot reference the package's own name (`own-package`)"
            ),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn test_run_exports_conditional_subtable_rejected() {
        let input = r#"
        [strong."if(unix)"]
        foo = "*"
        "#;
        let err = TomlRunExports::from_toml_str(input).unwrap_err();
        let message = format!("{err:?}");
        assert!(
            message.contains("not yet supported"),
            "unexpected error: {message}"
        );
    }
}
