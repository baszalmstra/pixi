//! [`PackageDependencySpec`]: the dependency-value type for package-level
//! dependency tables (`[package.run-dependencies]`, `[package.host-dependencies]`,
//! `[package.build-dependencies]`, `[package.run-constraints]`, and all
//! `[package.run-exports.*]` tables).
//!
//! This type, and *not* [`PixiSpec`] itself, is what gates the `pin-subpackage`
//! spec value to package-level tables: workspace- and feature-level dependency
//! tables keep parsing plain [`PixiSpec`] values directly, so they structurally
//! cannot accept `pin-subpackage`.

use pixi_spec::{Pin, PixiSpec};
use toml_span::{DeserError, Value, value::ValueInner};

/// A single dependency value in a package-level dependency table: either a
/// regular [`PixiSpec`] (version, path, git, url, ...) or a self-referential
/// `pin-subpackage` pin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackageDependencySpec {
    /// A regular dependency spec, as used everywhere else in the manifest.
    Spec(PixiSpec),
    /// `{ pin-subpackage = true }` or `{ pin-subpackage = { ... } }`: a
    /// self-referential pin, resolved against the package's own
    /// `(name, version, build_string)` once it is known. Only valid when the
    /// entry's key equals the package's own name; that check happens later,
    /// in `TomlPackage::into_manifest`, once the package's resolved name is
    /// known (it may come from workspace inheritance).
    PinSubpackage(Pin),
}

impl PackageDependencySpec {
    /// Returns the inner [`PixiSpec`] if this is a regular spec.
    pub fn as_spec(&self) -> Option<&PixiSpec> {
        match self {
            PackageDependencySpec::Spec(spec) => Some(spec),
            PackageDependencySpec::PinSubpackage(_) => None,
        }
    }

    /// Returns the inner [`Pin`] if this is a `pin-subpackage` entry.
    pub fn as_pin_subpackage(&self) -> Option<&Pin> {
        match self {
            PackageDependencySpec::Spec(_) => None,
            PackageDependencySpec::PinSubpackage(pin) => Some(pin),
        }
    }

    /// Returns `true` if this is a source dependency (path, git, or url).
    /// `pin-subpackage` entries are never source dependencies.
    pub fn is_source(&self) -> bool {
        match self {
            PackageDependencySpec::Spec(spec) => spec.is_source(),
            PackageDependencySpec::PinSubpackage(_) => false,
        }
    }
}

impl From<PixiSpec> for PackageDependencySpec {
    fn from(value: PixiSpec) -> Self {
        PackageDependencySpec::Spec(value)
    }
}

/// `{ pin-subpackage = true }` is sugar for `{ pin-subpackage = { exact = true } }`.
fn exact_pin() -> Pin {
    Pin {
        lower_bound: None,
        upper_bound: None,
        exact: true,
        build: None,
    }
}

impl<'de> toml_span::Deserialize<'de> for PackageDependencySpec {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let outer_span = value.span;
        let inner = value.take();

        // Only tables can carry a `pin-subpackage` key; strings (plain version
        // specs) go straight to `PixiSpec`.
        let mut table = match inner {
            ValueInner::Table(table) => table,
            other => {
                // Put the value back and let `PixiSpec` handle it, including
                // producing the right "a string or a table" error for anything
                // else.
                let mut restored = Value::with_span(other, outer_span);
                return <PixiSpec as toml_span::Deserialize>::deserialize(&mut restored)
                    .map(PackageDependencySpec::Spec);
            }
        };

        let Some(mut pin_value) = table.remove("pin-subpackage") else {
            // No `pin-subpackage` key: delegate the whole table to `PixiSpec`.
            let mut table_value = Value::with_span(ValueInner::Table(table), outer_span);
            return <PixiSpec as toml_span::Deserialize>::deserialize(&mut table_value)
                .map(PackageDependencySpec::Spec);
        };

        // `pin-subpackage` cannot combine with any other matchspec field.
        if !table.is_empty() {
            let other_key = table.keys().next().map(|k| k.name.to_string());
            return Err(toml_span::Error {
                kind: toml_span::ErrorKind::Custom(
                    format!(
                        "`pin-subpackage` cannot be combined with other fields (found `{}`)",
                        other_key.unwrap_or_default()
                    )
                    .into(),
                ),
                span: outer_span,
                line_info: None,
            }
            .into());
        }

        let pin = match pin_value.take() {
            ValueInner::Boolean(true) => exact_pin(),
            ValueInner::Boolean(false) => {
                return Err(toml_span::Error {
                    kind: toml_span::ErrorKind::Custom(
                        "`pin-subpackage = false` has no meaning; remove the key or use a table to configure the pin".into(),
                    ),
                    span: pin_value.span,
                    line_info: None,
                }
                .into());
            }
            inner @ ValueInner::Table(_) => {
                let mut tmp = Value::with_span(inner, pin_value.span);
                <Pin as toml_span::Deserialize>::deserialize(&mut tmp)?
            }
            other => {
                return Err(toml_span::de_helpers::expected(
                    "a boolean or a table",
                    other,
                    pin_value.span,
                )
                .into());
            }
        };

        Ok(PackageDependencySpec::PinSubpackage(pin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PackageDependencySpec` deserializes a *value*, but `toml_span::parse`
    /// always returns a table at the top level (a document). Wrap the fixture
    /// in `v = ...` and pull out the inner value to test arbitrary value
    /// shapes (string, table, ...).
    fn entry_value(input: &str) -> Value<'_> {
        let mut value = toml_span::parse(input.trim()).expect("expected valid TOML");
        let ValueInner::Table(mut table) = value.take() else {
            panic!("expected a table");
        };
        table.remove("v").expect("expected a `v` key")
    }

    fn parse(input: &str) -> PackageDependencySpec {
        let mut entry = entry_value(input);
        <PackageDependencySpec as toml_span::Deserialize>::deserialize(&mut entry)
            .expect("expected parse to succeed")
    }

    fn parse_err(input: &str) -> String {
        let mut entry = entry_value(input);
        let err = <PackageDependencySpec as toml_span::Deserialize>::deserialize(&mut entry)
            .expect_err("expected a parse failure");
        format!("{err:?}")
    }

    #[test]
    fn parses_plain_string_as_spec() {
        let spec = parse(r#"v = "1.0""#);
        assert!(matches!(spec, PackageDependencySpec::Spec(_)));
        assert_eq!(
            spec.as_spec()
                .unwrap()
                .as_version_spec()
                .unwrap()
                .to_string(),
            "==1.0"
        );
    }

    #[test]
    fn parses_detailed_table_as_spec() {
        let spec = parse(r#"v = { version = ">=1.0", channel = "conda-forge" }"#);
        assert!(matches!(spec, PackageDependencySpec::Spec(_)));
    }

    #[test]
    fn parses_path_table_as_spec() {
        let spec = parse(r#"v = { path = "." }"#);
        assert!(matches!(spec, PackageDependencySpec::Spec(_)));
        assert!(spec.is_source());
    }

    #[test]
    fn parses_pin_subpackage_shorthand_as_exact() {
        let spec = parse("v = { pin-subpackage = true }");
        let pin = spec.as_pin_subpackage().expect("expected a pin");
        assert!(pin.exact);
        assert_eq!(pin.lower_bound, None);
        assert_eq!(pin.upper_bound, None);
        assert_eq!(pin.build, None);
    }

    #[test]
    fn parses_detailed_pin_subpackage_table() {
        let spec = parse(
            r#"v = { pin-subpackage = { lower-bound = "x.x", upper-bound = "x.x.x", build = "py*" } }"#,
        );
        let pin = spec.as_pin_subpackage().expect("expected a pin");
        assert!(!pin.exact);
        assert!(pin.lower_bound.is_some());
        assert!(pin.upper_bound.is_some());
        assert_eq!(pin.build.as_deref(), Some("py*"));
    }

    #[test]
    fn rejects_pin_subpackage_combined_with_other_fields() {
        let err = parse_err(r#"v = { pin-subpackage = true, channel = "conda-forge" }"#);
        assert!(
            err.contains("pin-subpackage") && err.contains("combined"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_pin_subpackage_false() {
        let err = parse_err("v = { pin-subpackage = false }");
        assert!(err.contains("pin-subpackage"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_exact_and_build_combination_in_detailed_pin() {
        let err = parse_err(r#"v = { pin-subpackage = { exact = true, build = "py*" } }"#);
        assert!(
            err.contains("exact") && err.contains("build"),
            "unexpected error: {err}"
        );
    }
}
