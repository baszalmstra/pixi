//! Module for reading and comparing PyPI package metadata from local source trees.
//!
//! This module provides functionality to:
//! 1. Read metadata from local pyproject.toml files
//! 2. Compare locked metadata against current source tree metadata
use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;

use indexmap::IndexMap;
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{PackageName, Requirement};
use pixi_install_pypi::LockedPypiRecord;
use uv_normalize::ExtraName;

/// Metadata extracted from a local package source tree.
#[derive(Debug, Clone)]
pub struct LocalPackageMetadata {
    /// The version of the package, if statically known.
    /// `None` for packages with dynamic versions.
    pub version: Option<Version>,
    /// The package dependencies.
    pub requires_dist: Vec<Requirement>,
    /// The Python version requirement.
    pub requires_python: Option<VersionSpecifiers>,
}

/// The result of comparing locked metadata against current metadata.
#[derive(Debug)]
pub enum MetadataMismatch {
    /// The requires_dist (dependencies) have changed.
    RequiresDist(RequiresDistDiff),
    /// The version has changed.
    Version { locked: Version, current: Version },
    /// The requires_python has changed.
    RequiresPython {
        locked: Option<VersionSpecifiers>,
        current: Option<VersionSpecifiers>,
    },
}

/// Describes the difference in requires_dist between locked and current metadata.
#[derive(Debug)]
pub struct RequiresDistDiff {
    /// Dependencies that were added.
    pub added: Vec<Requirement>,
    /// Dependencies that were removed.
    pub removed: Vec<Requirement>,
}

/// Compare locked metadata against current metadata from the source tree.
///
/// Returns `None` if the metadata matches, or `Some(MetadataMismatch)` describing
/// what changed.
pub fn compare_metadata(
    locked_record: &LockedPypiRecord,
    current: &LocalPackageMetadata,
) -> Option<MetadataMismatch> {
    let locked = &locked_record.data;

    // Compare requires_dist (as normalized sets)
    let locked_deps: BTreeSet<String> = locked
        .requires_dist()
        .iter()
        .map(normalize_requirement)
        .collect();

    let current_deps: BTreeSet<String> = current
        .requires_dist
        .iter()
        .map(normalize_requirement)
        .collect();

    if locked_deps != current_deps {
        // Calculate the diff
        let added: Vec<Requirement> = current
            .requires_dist
            .iter()
            .filter(|r| !locked_deps.contains(&normalize_requirement(r)))
            .cloned()
            .collect();

        let removed: Vec<Requirement> = locked
            .requires_dist()
            .iter()
            .filter(|r| !current_deps.contains(&normalize_requirement(r)))
            .cloned()
            .collect();

        return Some(MetadataMismatch::RequiresDist(RequiresDistDiff {
            added,
            removed,
        }));
    }

    // Compare the locked version (always present on LockedPypiRecord)
    // against the current version from the source tree.
    if let Some(current_version) = &current.version
        && &locked_record.locked_version != current_version
    {
        return Some(MetadataMismatch::Version {
            locked: locked_record.locked_version.clone(),
            current: current_version.clone(),
        });
    }

    // Compare requires_python
    if locked.requires_python() != current.requires_python.as_ref() {
        return Some(MetadataMismatch::RequiresPython {
            locked: locked.requires_python().cloned(),
            current: current.requires_python.clone(),
        });
    }

    None
}

/// Normalize a requirement for comparison purposes.
///
/// This ensures that semantically equivalent requirements compare equal,
/// regardless of formatting differences (e.g., whitespace, order of extras).
fn normalize_requirement(req: &Requirement) -> String {
    // Use the canonical string representation
    // The pep508_rs library already normalizes package names and versions
    req.to_string()
}

/// Replace each `pkg[group]` self-reference with the raw entries of
/// `optional_dependencies[group]`, carrying the outer marker. Matches
/// what build backends bake into wheel METADATA; UV's static parse
/// leaves the self-references intact. Cycles in the optional-deps
/// graph are broken on the path that closes them.
pub fn expand_self_extras(
    requires_dist: Vec<Requirement>,
    package_name: &PackageName,
    optional_dependencies: &IndexMap<ExtraName, Vec<String>>,
) -> Vec<Requirement> {
    let parsed: HashMap<&str, Vec<Requirement>> = optional_dependencies
        .iter()
        .map(|(extra, raws)| {
            let reqs = raws
                .iter()
                .filter_map(|s| Requirement::from_str(s).ok())
                .collect::<Vec<_>>();
            (extra.as_ref(), reqs)
        })
        .collect();

    let mut result: Vec<Requirement> = Vec::new();
    let mut path: HashSet<&str> = HashSet::new();
    for req in &requires_dist {
        expand_into(req, package_name, &parsed, &mut path, &mut result);
    }
    result
}

fn expand_into<'a>(
    req: &Requirement,
    package_name: &PackageName,
    parsed: &'a HashMap<&'a str, Vec<Requirement>>,
    path: &mut HashSet<&'a str>,
    result: &mut Vec<Requirement>,
) {
    if req.name != *package_name || req.extras.is_empty() {
        result.push(req.clone());
        return;
    }
    for extra in &req.extras {
        let Some((&key, group_reqs)) = parsed.get_key_value(extra.as_ref()) else {
            continue;
        };
        if !path.insert(key) {
            continue;
        }
        for child in group_reqs {
            let mut expanded = child.clone();
            expanded.marker.and(req.marker.clone());
            expand_into(&expanded, package_name, parsed, path, result);
        }
        path.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::lock_file::tests::make_wheel_package_with;
    use pixi_install_pypi::UnresolvedPypiRecord;
    use rattler_lock::{PypiDistributionData, PypiPackageData};

    fn lock_for_test(data: PypiPackageData) -> LockedPypiRecord {
        let version = data
            .version()
            .cloned()
            .unwrap_or_else(|| Version::from_str("42.23").unwrap());
        UnresolvedPypiRecord::from(data).lock(version)
    }

    #[test]
    fn test_normalize_requirement() {
        let req1: Requirement = "numpy>=1.0".parse().unwrap();
        let req2: Requirement = "numpy >= 1.0".parse().unwrap();
        // Note: These may or may not be equal depending on pep508_rs normalization
        // The important thing is we consistently compare them
        assert_eq!(normalize_requirement(&req1), normalize_requirement(&req1));
        let _ = req2; // silence unused warning
    }

    #[test]
    fn git_url_with_full_sha_ref_roundtrips_through_pep508() {
        // Sanity check for the hypothesis in pixi#6062 that pep508_rs +
        // url::Url rewrite `@<40-hex>` into `#<40-hex>` on Display round-trip.
        // It doesn't: across schemes (ssh, ssh-with-userinfo, https) and
        // across ref shapes (full sha, tag, branch), Requirement::from_str
        // followed by Display preserves the original string verbatim.
        //
        // Whatever flips `@<sha>` → `#<sha>` on the lock-write side is NOT
        // this pep508_rs round-trip, so investigation needs to focus on the
        // uv producer (e.g. how `database.get_or_build_wheel_metadata` and
        // `database.requires_dist` materialise the git URL on their
        // respective sides) and on `rattler_lock`'s YAML serializer for
        // `requires_dist`.
        let cases = [
            "pkg @ git+ssh://host/repo@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pkg @ git+ssh://git@host/repo@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pkg @ git+https://github.com/foo/bar@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "pkg @ git+https://github.com/foo/bar@v1.0",
            "pkg @ git+https://github.com/foo/bar@main",
        ];
        for input in cases {
            let req: Requirement = input.parse().unwrap();
            assert_eq!(req.to_string(), input, "round-trip changed `{input}`");
        }
    }

    #[test]
    fn given_url_with_hash_separator_survives_to_string() {
        // The rattler_lock v7 writer at requires_dist time picks the
        // verbatim `given` string of a VerbatimUrl over the parsed URL's
        // Display:
        //
        //   if let Some(g) = url.given() { format!(" @ {g}") }
        //   else                          { format!(" @ {url}") }
        //
        // So if anything constructs a `pep508_rs::Requirement` whose
        // verbatim URL has `given = "git+url#<sha>"`, that exact `#<sha>`
        // form lands in the lock file regardless of what `Display` would
        // have produced from the parsed URL. The live read then re-derives
        // the requirement via `uv → req.to_string() → pep508_rs::from_str`
        // — no `given` carried — and rattler's write path emits `@<sha>`
        // form on the next round. That is exactly the comparison mismatch
        // in pixi#6062.
        let sha = "84063a63c794e0ec1f37e5d895523b5bd31c3406";
        let parsed_url: url::Url =
            format!("git+ssh://github.com/Z3ZEL/dep-test@{sha}")
                .parse()
                .unwrap();
        let verbatim_with_hash_given = pep508_rs::VerbatimUrl::from_url(parsed_url)
            .with_given(format!("git+ssh://github.com/Z3ZEL/dep-test#{sha}"));

        // Display (used by `to_requirements`-side path) reads parsed url:
        assert_eq!(
            format!("{}", verbatim_with_hash_given),
            format!("git+ssh://github.com/Z3ZEL/dep-test@{sha}")
        );
        // The `given()` accessor (used by rattler_lock's
        // `requirement_to_string`) reads the verbatim string:
        assert_eq!(
            verbatim_with_hash_given.given(),
            Some(format!("git+ssh://github.com/Z3ZEL/dep-test#{sha}").as_str())
        );
    }

    #[test]
    fn pep508_from_str_preserves_given_for_hash_form() {
        // When rattler_lock reads the lock file and parses each
        // `requires_dist` line via `pep508_rs::Requirement::from_str`, does
        // it preserve the raw input as `given`? If yes, then a lock file
        // that contains `dep-a @ git+url#<sha>` deserialises into a
        // Requirement whose `given` is `git+url#<sha>` — which then writes
        // back to the lock file in the same `#<sha>` form on every save.
        // That's the self-perpetuating channel for pixi#6062.
        let sha = "84063a63c794e0ec1f37e5d895523b5bd31c3406";
        let input = format!("dep-a @ git+ssh://github.com/Z3ZEL/dep-test#{sha}");
        let req: pep508_rs::Requirement = input.parse().unwrap();
        let Some(pep508_rs::VersionOrUrl::Url(url)) = req.version_or_url.as_ref() else {
            panic!("expected URL");
        };
        eprintln!("parsed given:    {:?}", url.given());
        eprintln!("parsed Display:  {}", url);
        eprintln!("rattler form:    {}", req);
    }

    #[test]
    fn ssh_full_sha_paths_preserve_at_ref_form() {
        // The reporter's pyproject.toml uses
        //   git = "ssh://github.com/Z3ZEL/dep-test", rev = "<full-sha>"
        // and the issue says the lock-file writer ends up with `#<sha>`
        // form. This test pins down that with the ssh scheme — both with
        // and without a `git@` userinfo — every conversion path pixi has
        // produces `@<sha>` form, never `#<sha>`. The `#<sha>` form
        // therefore cannot originate inside pixi's lock-write path on
        // HEAD via `to_requirements`; it must enter `requires_dist` via
        // the `given` channel exercised by `convert_uv_requirements_to_pep508`
        // and persisted by rattler_lock's `requirement_to_string`.
        use pixi_pypi_spec::PixiPypiSpec;
        use pixi_uv_conversions::{as_uv_req, to_requirements};

        let sha = "84063a63c794e0ec1f37e5d895523b5bd31c3406";
        for input in [
            format!("dep-a @ git+ssh://github.com/Z3ZEL/dep-test@{sha}"),
            format!("dep-a @ git+ssh://git@github.com/Z3ZEL/dep-test@{sha}"),
        ] {
            let pep_req = pep508_rs::Requirement::from_str(&input).unwrap();
            let pixi_spec = PixiPypiSpec::try_from(pep_req).unwrap();
            let uv_req = as_uv_req(&pixi_spec, "dep-a", std::path::Path::new("/")).unwrap();
            let pep_reqs = to_requirements(std::iter::once(&uv_req)).unwrap();
            let live = uv_req.to_string();
            let lock = pep_reqs[0].to_string();

            assert!(
                live.contains(&format!("@{sha}")) && !live.contains(&format!("#{sha}")),
                "uv Display produced `#<sha>` for `{input}`: {live}"
            );
            assert!(
                lock.contains(&format!("@{sha}")) && !lock.contains(&format!("#{sha}")),
                "pixi to_requirements produced `#<sha>` for `{input}`: {lock}"
            );
        }
    }

    #[test]
    fn pixi_to_requirements_and_uv_display_match_for_full_sha() {
        // Two paths produce the `requires_dist` strings compared in
        // `compare_metadata`:
        //   • lock side: pixi's `to_requirements()` over the uv requirement,
        //     stored as `pep508_rs::Requirement`, serialised to YAML via its
        //     `Display`, and parsed back on read.
        //   • live side: `req.to_string()` on the uv requirement returned by
        //     `database.requires_dist`, then parsed into `pep508_rs::Requirement`.
        //
        // For pixi#6062 to manifest the way the reporter describes, these
        // two paths would have to diverge on `rev = <full-sha>` git deps.
        // This test pins down that with the uv requirement pixi itself
        // builds via `as_uv_req`, both paths produce the same string and
        // the round-trip survives pep508 re-parse. The bug therefore must
        // originate in a different uv-side requirement (e.g. one produced
        // by `database.requires_dist` over a wheel METADATA file), not in
        // the conversion layer.
        use pixi_pypi_spec::PixiPypiSpec;
        use pixi_uv_conversions::{as_uv_req, to_requirements};

        let pep_req = pep508_rs::Requirement::from_str(
            "dacite @ git+ssh://git@github.com/konradhalas/dacite.git@9898ccbb783e7e6a35ae165e7deb9fa84edfe21c",
        )
        .unwrap();
        let pixi_spec = PixiPypiSpec::try_from(pep_req).unwrap();
        let uv_req = as_uv_req(&pixi_spec, "dacite", std::path::Path::new("/")).unwrap();

        let live_string = uv_req.to_string();
        let pep_reqs = to_requirements(std::iter::once(&uv_req)).unwrap();
        let lock_string = pep_reqs[0].to_string();
        let reparsed: pep508_rs::Requirement = lock_string.parse().unwrap();

        assert_eq!(live_string, lock_string);
        assert_eq!(lock_string, reparsed.to_string());
        assert!(
            live_string.contains("@9898ccbb783e7e6a35ae165e7deb9fa84edfe21c"),
            "expected `@<sha>` form, got `{live_string}`"
        );
        assert!(
            !live_string.contains("#9898ccbb783e7e6a35ae165e7deb9fa84edfe21c"),
            "expected no `#<sha>` form, got `{live_string}`"
        );
    }

    #[test]
    fn test_compare_metadata_same() {
        let locked = lock_for_test(PypiPackageData::Distribution(Box::new(
            PypiDistributionData {
                name: "test-package".parse().unwrap(),
                version: Version::from_str("1.0.0").unwrap(),
                requires_dist: vec!["numpy>=1.0".parse().unwrap()],
                requires_python: Some(VersionSpecifiers::from_str(">=3.8").unwrap()),
                location: rattler_lock::UrlOrPath::Url(url::Url::parse("file:///test").unwrap())
                    .into(),
                hash: None,
                index_url: None,
            },
        )));

        let current = LocalPackageMetadata {
            version: Some(Version::from_str("1.0.0").unwrap()),
            requires_dist: vec!["numpy>=1.0".parse().unwrap()],
            requires_python: Some(VersionSpecifiers::from_str(">=3.8").unwrap()),
        };

        assert!(compare_metadata(&locked, &current).is_none());
    }

    #[test]
    fn test_compare_metadata_different_deps() {
        let locked = lock_for_test(make_wheel_package_with(
            "test-package",
            "1.0.0",
            rattler_lock::UrlOrPath::Url(url::Url::parse("file:///test").unwrap()).into(),
            None,
            None,
            vec!["numpy>=1.0".parse().unwrap()],
            None,
        ));

        let current = LocalPackageMetadata {
            version: Some(Version::from_str("1.0.0").unwrap()),
            requires_dist: vec![
                "numpy>=1.0".parse().unwrap(),
                "pandas>=2.0".parse().unwrap(), // Added
            ],
            requires_python: None,
        };

        let mismatch = compare_metadata(&locked, &current);
        assert!(matches!(mismatch, Some(MetadataMismatch::RequiresDist(_))));
    }

    fn pkg_name(s: &str) -> PackageName {
        PackageName::from_str(s).unwrap()
    }

    fn extra(s: &str) -> ExtraName {
        ExtraName::from_str(s).unwrap()
    }

    fn req(s: &str) -> Requirement {
        Requirement::from_str(s).unwrap()
    }

    fn render(reqs: &[Requirement]) -> String {
        let mut lines: Vec<String> = reqs.iter().map(|r| r.to_string()).collect();
        lines.sort();
        lines.join("\n")
    }

    #[test]
    fn expand_self_extras_replaces_ribasim_style_self_refs() {
        // Mirrors Deltares/Ribasim: `delwaq` references `ribasim[netcdf]`
        // and `all` composes the other groups.
        let mut optional: IndexMap<ExtraName, Vec<String>> = IndexMap::new();
        optional.insert(extra("tests"), vec!["pytest".into()]);
        optional.insert(extra("netcdf"), vec!["xugrid".into()]);
        optional.insert(
            extra("delwaq"),
            vec!["jinja2".into(), "networkx".into(), "ribasim[netcdf]".into()],
        );
        optional.insert(
            extra("all"),
            vec![
                "ribasim[tests]".into(),
                "ribasim[netcdf]".into(),
                "ribasim[delwaq]".into(),
            ],
        );

        let static_parsed = vec![
            req("pandas"),
            req("pytest ; extra == 'tests'"),
            req("xugrid ; extra == 'netcdf'"),
            req("jinja2 ; extra == 'delwaq'"),
            req("networkx ; extra == 'delwaq'"),
            req("ribasim[netcdf] ; extra == 'delwaq'"),
            req("ribasim[tests] ; extra == 'all'"),
            req("ribasim[netcdf] ; extra == 'all'"),
            req("ribasim[delwaq] ; extra == 'all'"),
        ];

        let expanded = expand_self_extras(static_parsed, &pkg_name("ribasim"), &optional);
        insta::assert_snapshot!(render(&expanded), @r"
        jinja2 ; extra == 'all'
        jinja2 ; extra == 'delwaq'
        networkx ; extra == 'all'
        networkx ; extra == 'delwaq'
        pandas
        pytest ; extra == 'all'
        pytest ; extra == 'tests'
        xugrid ; extra == 'all'
        xugrid ; extra == 'all'
        xugrid ; extra == 'delwaq'
        xugrid ; extra == 'netcdf'
        ");
    }

    #[test]
    fn expand_self_extras_preserves_non_self_references() {
        let optional: IndexMap<ExtraName, Vec<String>> = IndexMap::new();
        let input = vec![req("requests"), req("other[gpu] ; extra == 'all'")];
        let expanded = expand_self_extras(input, &pkg_name("mypkg"), &optional);
        insta::assert_snapshot!(render(&expanded), @r"
        other[gpu] ; extra == 'all'
        requests
        ");
    }

    #[test]
    fn expand_self_extras_drops_unknown_extras_silently() {
        // A self-reference to an extra that doesn't exist (typo, stale
        // metadata) is dropped.
        let optional: IndexMap<ExtraName, Vec<String>> = IndexMap::new();
        let input = vec![req("pandas"), req("mypkg[missing] ; extra == 'all'")];
        let expanded = expand_self_extras(input, &pkg_name("mypkg"), &optional);
        insta::assert_snapshot!(render(&expanded), @"pandas");
    }

    #[test]
    fn expand_self_extras_breaks_cycles() {
        // a -> b -> a; expansion must terminate. The non-cyclic dep
        // `actual` is still emitted with the outer marker.
        let mut optional: IndexMap<ExtraName, Vec<String>> = IndexMap::new();
        optional.insert(extra("a"), vec!["actual".into(), "mypkg[b]".into()]);
        optional.insert(extra("b"), vec!["mypkg[a]".into()]);

        let input = vec![req("mypkg[a] ; extra == 'X'")];
        let expanded = expand_self_extras(input, &pkg_name("mypkg"), &optional);
        insta::assert_snapshot!(render(&expanded), @"actual ; extra == 'x'");
    }
}
