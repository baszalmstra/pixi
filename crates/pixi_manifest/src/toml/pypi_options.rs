use std::{path::PathBuf, str::FromStr};

use indexmap::IndexSet;
use pixi_toml::{TomlEnum, TomlFromStr, TomlIndexMap, TomlWith};
use toml_span::{
    DeserError, ErrorKind, Value,
    de_helpers::{TableHelper, expected},
    value::ValueInner,
};
use url::Url;

use crate::pypi::pypi_options::{
    FindLinksUrlOrPath, NoBinary, NoBuild, NoBuildIsolation, PypiOptions,
};

/// A helper struct to deserialize a [`pep508_rs::PackageName`] from a TOML
/// string.
struct Pep508PackageName(pub pep508_rs::PackageName);

impl<'de> toml_span::Deserialize<'de> for Pep508PackageName {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let str = value.take_string(None)?;
        let package_name = pep508_rs::PackageName::from_str(&str).map_err(|e| {
            DeserError::from(toml_span::Error {
                kind: ErrorKind::Custom(e.to_string().into()),
                span: value.span,
                line_info: None,
            })
        })?;
        Ok(Self(package_name))
    }
}

impl<'de> toml_span::Deserialize<'de> for NoBuild {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        // It can be either `true` or `false` or an array of strings
        if value.as_bool().is_some() {
            if bool::deserialize(value)? {
                return Ok(NoBuild::All);
            } else {
                return Ok(NoBuild::None);
            }
        }
        // We assume it's an array of strings
        if value.as_array().is_some() {
            match value.take() {
                ValueInner::Array(array) => {
                    let mut packages = IndexSet::with_capacity(array.len());
                    for mut value in array {
                        packages.insert(Pep508PackageName::deserialize(&mut value)?.0);
                    }
                    Ok(NoBuild::Packages(packages))
                }
                _ => Err(expected(
                    "an array of packages e.g. [\"foo\", \"bar\"]",
                    value.take(),
                    value.span,
                )
                .into()),
            }
        } else {
            Err(expected(
                r#"either "all", "none" or an array of packages e.g. ["foo", "bar"] "#,
                value.take(),
                value.span,
            )
            .into())
        }
    }
}

impl<'de> toml_span::Deserialize<'de> for NoBinary {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        // It can be either `true` or `false` or an array of strings
        if value.as_bool().is_some() {
            if bool::deserialize(value)? {
                return Ok(NoBinary::All);
            } else {
                return Ok(NoBinary::None);
            }
        }
        // We assume it's an array of strings
        if value.as_array().is_some() {
            match value.take() {
                ValueInner::Array(array) => {
                    let mut packages = IndexSet::with_capacity(array.len());
                    for mut value in array {
                        packages.insert(Pep508PackageName::deserialize(&mut value)?.0);
                    }
                    Ok(NoBinary::Packages(packages))
                }
                _ => Err(expected(
                    "an array of packages e.g. [\"foo\", \"bar\"]",
                    value.take(),
                    value.span,
                )
                .into()),
            }
        } else {
            Err(expected(
                r#"either "all", "none" or an array of packages e.g. ["foo", "bar"] "#,
                value.take(),
                value.span,
            )
            .into())
        }
    }
}

impl<'de> toml_span::Deserialize<'de> for PypiOptions {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;

        let index_url = th
            .optional::<TomlFromStr<_>>("index-url")
            .map(TomlFromStr::into_inner);
        let extra_index_urls = th
            .optional::<TomlWith<_, Vec<TomlFromStr<_>>>>("extra-index-urls")
            .map(|x| x.into_inner());
        let find_links = th.optional("find-links");
        let no_build_isolation = th.optional("no-build-isolation").unwrap_or_default();
        let index_strategy = th
            .optional::<TomlEnum<_>>("index-strategy")
            .map(TomlEnum::into_inner);

        let no_build = th.optional::<NoBuild>("no-build");
        let dependency_overrides = th
            .optional::<TomlIndexMap<_, _>>("dependency-overrides")
            .map(TomlIndexMap::into_inner);

        let no_binary = th.optional::<NoBinary>("no-binary");

        th.finalize(None)?;

        Ok(Self {
            index_url,
            extra_index_urls,
            find_links,
            no_build_isolation,
            index_strategy,
            no_build,
            dependency_overrides,
            no_binary,
        })
    }
}

impl<'de> toml_span::Deserialize<'de> for FindLinksUrlOrPath {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut table = match value.take() {
            ValueInner::Table(table) => table,
            other => return Err(expected("a table", other, value.span).into()),
        };

        let mut errors = DeserError { errors: vec![] };

        let path = table.remove("path");
        let url = table.remove("url");
        if path.is_some() && url.is_some() {
            errors.errors.push(toml_span::Error {
                kind: ErrorKind::Custom("cannot define both 'url' and 'path'".into()),
                span: value.span,
                line_info: None,
            });
        }

        let path = if let Some(mut path) = path {
            match path
                .take_string(None)
                .map(|str| PathBuf::from(str.into_owned()))
            {
                Err(error) => {
                    errors.errors.push(error);
                    None
                }
                Ok(path) => Some(path),
            }
        } else {
            None
        };

        let url = if let Some(mut url) = url {
            match url.take_string(None).map(|str| Url::parse(&str)) {
                Err(error) => {
                    errors.errors.push(error);
                    None
                }
                Ok(Err(error)) => {
                    errors.errors.push(toml_span::Error {
                        kind: ErrorKind::Custom(error.to_string().into()),
                        span: url.span,
                        line_info: None,
                    });
                    None
                }
                Ok(Ok(url)) => Some(url),
            }
        } else {
            None
        };

        if !errors.errors.is_empty() {
            return Err(errors);
        }

        if let Some(url) = url {
            Ok(Self::Url(url))
        } else if let Some(path) = path {
            Ok(Self::Path(path))
        } else {
            Err(DeserError::from(toml_span::Error {
                kind: ErrorKind::Custom("either 'url' or 'path' must be defined".into()),
                span: value.span,
                line_info: None,
            }))
        }
    }
}

impl<'de> toml_span::Deserialize<'de> for NoBuildIsolation {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        match value.take() {
            ValueInner::Boolean(value) if value => Ok(NoBuildIsolation::All),
            ValueInner::Boolean(value) if !value => Ok(NoBuildIsolation::none()),
            ValueInner::Array(values) => {
                let mut packages = IndexSet::with_capacity(values.len());
                for mut value in values {
                    packages.insert(Pep508PackageName::deserialize(&mut value)?.0);
                }
                Ok(NoBuildIsolation::Packages(packages))
            }
            _ => Err(expected(
                "a boolean or an array of packages e.g. [\"foo\", \"bar\"]",
                value.take(),
                value.span,
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod test {
    use insta::{assert_debug_snapshot, assert_snapshot};
    use pixi_pypi_spec::PypiPackageName;
    use pixi_test_utils::format_parse_error;

    use super::*;
    use crate::toml::FromTomlStr;

    #[test]
    fn test_empty() {
        let input = "";
        let options = PypiOptions::from_toml_str(input).unwrap();
        assert_eq!(options, PypiOptions::default());
    }

    #[test]
    fn test_deserialize_pypi_options() {
        let toml_str = r#"
                 index-url = "https://example.com/pypi"
                 extra-index-urls = ["https://example.com/extra"]
                 no-build-isolation = ["pkg1", "pkg2"]

                 [[find-links]]
                 path = "/path/to/flat/index"

                 [[find-links]]
                 url = "https://flat.index"

                 [dependency-overrides]
                 numpy = ">=2.0.0"
             "#;
        let deserialized_options: PypiOptions = PypiOptions::from_toml_str(toml_str).unwrap();
        assert_eq!(
            deserialized_options,
            PypiOptions {
                index_url: Some(Url::parse("https://example.com/pypi").unwrap()),
                extra_index_urls: Some(vec![Url::parse("https://example.com/extra").unwrap()]),
                find_links: Some(vec![
                    FindLinksUrlOrPath::Path("/path/to/flat/index".into()),
                    FindLinksUrlOrPath::Url(Url::parse("https://flat.index").unwrap())
                ]),
                no_build_isolation: NoBuildIsolation::from_iter([
                    "pkg1".parse().unwrap(),
                    "pkg2".parse().unwrap()
                ]),
                index_strategy: None,
                no_build: Default::default(),
                dependency_overrides: Some(indexmap::IndexMap::from_iter([(
                    PypiPackageName::from_str("numpy").unwrap(),
                    pixi_pypi_spec::PixiPypiSpec::RawVersion(
                        pixi_pypi_spec::VersionOrStar::from_str(">=2.0.0").unwrap()
                    )
                )]),),
                no_binary: Default::default(),
            },
        );
    }

    #[test]
    fn test_full() {
        let input = r#"
        index-url = "https://pypi.org/simple"
        extra-index-urls = ["https://pypi.org/simple", "file:///path/to/simple"]
        find-links = [
            { path = "../" },
            { url = "https://google.com" }
        ]
        no-build-isolation = ["sigma"]
        index-strategy = "first-index"
        no-build = true
        no-binary = ["package1", "package2"]
        "#;
        let options = PypiOptions::from_toml_str(input).unwrap();
        assert_debug_snapshot!(options);
    }

    #[test]
    fn test_no_build_packages() {
        let input = r#"
        no-build = ["package1"]
        "#;
        let options = PypiOptions::from_toml_str(input).unwrap();
        assert_debug_snapshot!(options);
    }

    #[test]
    fn test_no_binary_packages() {
        let input = r#"
        no-binary = ["package1"]
        "#;
        let options = PypiOptions::from_toml_str(input).unwrap();
        assert_debug_snapshot!(options);
    }

    #[test]
    fn test_no_build_isolation_boolean() {
        let input = r#"
        no-build-isolation = true
        "#;
        let options = PypiOptions::from_toml_str(input).unwrap();
        assert_debug_snapshot!(options);
    }

    #[test]
    fn test_invalid_strategy_missing_dash() {
        let input = r#"
        index-strategy = "firstindex"
        "#;
        assert_snapshot!(
            format_parse_error(input, PypiOptions::from_toml_str(input).unwrap_err()),
            @r###"
         × Expected one of 'first-index', 'unsafe-first-match', 'unsafe-best-match'
          ╭─[pixi.toml:2:27]
        1 │
        2 │         index-strategy = "firstindex"
          ·                           ──────────
        3 │
          ╰────
         help: Did you mean 'first-index'?
        "###
        )
    }

    #[test]
    fn test_invalid_strategy_upper_case() {
        let input = r#"
        index-strategy = "UnsafeFirstMatch"
        "#;
        assert_snapshot!(
            format_parse_error(input, PypiOptions::from_toml_str(input).unwrap_err()),
            @r###"
         × Expected one of 'first-index', 'unsafe-first-match', 'unsafe-best-match'
          ╭─[pixi.toml:2:27]
        1 │
        2 │         index-strategy = "UnsafeFirstMatch"
          ·                           ────────────────
        3 │
          ╰────
         help: Did you mean 'unsafe-first-match'?
        "###
        )
    }

    #[test]
    fn test_invalid_strategy_far() {
        let input = r#"
        index-strategy = "foobar"
        "#;
        assert_snapshot!(
            format_parse_error(input, PypiOptions::from_toml_str(input).unwrap_err()),
            @r###"
         × Expected one of 'first-index', 'unsafe-first-match', 'unsafe-best-match'
          ╭─[pixi.toml:2:27]
        1 │
        2 │         index-strategy = "foobar"
          ·                           ──────
        3 │
          ╰────
        "###
        )
    }

    #[test]
    fn test_missing_url_or_path() {
        let input = "find-links = [{}]";
        assert_snapshot!(
            format_parse_error(input, PypiOptions::from_toml_str(input).unwrap_err()),
            @r###"
         × either 'url' or 'path' must be defined
          ╭─[pixi.toml:1:15]
        1 │ find-links = [{}]
          ·               ──
          ╰────
        "###
        )
    }

    #[test]
    fn test_both_url_or_path() {
        let input = r#"find-links = [{url = "", path = ""}]"#;
        assert_snapshot!(
            format_parse_error(input, PypiOptions::from_toml_str(input).unwrap_err()),
            @r###"
         × cannot define both 'url' and 'path'
          ╭─[pixi.toml:1:15]
        1 │ find-links = [{url = "", path = ""}]
          ·               ─────────────────────
          ╰────
        "###
        )
    }

    #[test]
    fn test_wrong_build_option_type() {
        let input = r#"no-build = 3"#;
        assert_snapshot!(format_parse_error(
            input,
            PypiOptions::from_toml_str(input).unwrap_err()
        ), @r###"
         × expected either "all", "none" or an array of packages e.g. ["foo", "bar"] , found integer
          ╭─[pixi.toml:1:12]
        1 │ no-build = 3
          ·            ─
          ╰────
        "###)
    }

    #[test]
    fn test_no_build_package_name() {
        let input = r#"no-build = ['$$$']"#;
        assert_snapshot!(format_parse_error(
            input,
            PypiOptions::from_toml_str(input).unwrap_err()
        ), @r###"
         × Not a valid package or extra name: "$$$". Names must start and end with a letter or digit and may only contain -, _, ., and alphanumeric characters.
          ╭─[pixi.toml:1:14]
        1 │ no-build = ['$$$']
          ·              ───
          ╰────
        "###)
    }
}
