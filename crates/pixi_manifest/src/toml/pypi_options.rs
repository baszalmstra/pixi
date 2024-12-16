use std::path::PathBuf;

use pixi_toml::{TomlEnum, TomlFromStr, TomlWith};
use toml_span::{
    de_helpers::{expected, TableHelper},
    value::ValueInner,
    DeserError, ErrorKind, Value,
};
use url::Url;

use crate::pypi::pypi_options::{FindLinksUrlOrPath, PypiOptions};

impl<'de> toml_span::Deserialize<'de> for PypiOptions {
    fn deserialize(value: &mut Value<'de>) -> Result<Self, DeserError> {
        let mut th = TableHelper::new(value)?;

        let index_url = th
            .optional::<TomlFromStr<_>>("index-url")
            .map(TomlFromStr::into_inner);
        let extra_index_urls = th
            .optional::<TomlWith<_, Vec<TomlFromStr<_>>>>("extra-index-url")
            .map(|x| x.into_inner());
        let find_links = th.optional("find-links");
        let no_build_isolation = th.optional("no-build-isolation");
        let index_strategy = th
            .optional::<TomlEnum<_>>("index-strategy")
            .map(TomlEnum::into_inner);

        th.finalize(None)?;

        Ok(Self {
            index_url,
            extra_index_urls,
            find_links,
            no_build_isolation,
            index_strategy,
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

#[cfg(test)]
mod test {
    use insta::{assert_debug_snapshot, assert_snapshot};

    use super::*;
    use crate::{toml::FromTomlStr, utils::test_utils::format_parse_error};

    #[test]
    fn test_empty() {
        let input = "";
        let options = PypiOptions::from_toml_str(input).unwrap();
        assert_eq!(options, PypiOptions::default());
    }

    #[test]
    fn test_full() {
        let input = r#"
        index-url = "https://pypi.org/simple"
        extra-index-url = ["https://pypi.org/simple", "file:///path/to/simple"]
        find-links = [
            { path = "../" },
            { url = "https://google.com" }
        ]
        no-build-isolation = ["sigma"]
        index-strategy = "first-index"
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
}
