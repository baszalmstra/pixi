use std::fmt::{self, Display};

use itertools::Itertools;
use miette::Diagnostic;
use pixi_core::DependencyType;
use pixi_manifest::{FeatureName, WorkspaceManifest};
use pixi_pypi_spec::PypiPackageName;
use rattler_conda_types::PackageName;
use std::str::FromStr;

use pixi_consts::consts;

use super::locate::{
    Location, Slot, format_exact_location, is_exact_match, is_similar, walk_dependencies,
};

/// What was searched when a dependency could not be removed.
pub(super) enum Scope {
    /// A specific table + feature was targeted (the user passed a location flag
    /// such as `--pypi`, `--host`, `--build`, or `--feature`).
    Table {
        dependency_type: DependencyType,
        feature: FeatureName,
    },
    /// The whole workspace was searched (a bare `pixi remove <pkg>`).
    Anywhere,
}

/// Diagnostic for the "dependency not found" path of `pixi remove`. Carries
/// computed help text that points the user at the right dependency table,
/// feature, or a similar-looking package name.
#[derive(Debug)]
pub(super) struct DependencyRemovalError {
    name: String,
    location: NotFoundLocation,
    suggestions: Vec<String>,
}

/// The rendered description of where we looked, kept separate from [`Scope`] so
/// the error can stay `Debug` without requiring it of every field of `Scope`.
#[derive(Debug)]
enum NotFoundLocation {
    Table {
        table: &'static str,
        feature: FeatureName,
    },
    Anywhere,
}

impl DependencyRemovalError {
    pub(super) fn new(name: String, manifest: &WorkspaceManifest, scope: Scope) -> Self {
        let suggestions = collect_suggestions(manifest, &name, &scope);
        let location = match scope {
            Scope::Table {
                dependency_type,
                feature,
            } => NotFoundLocation::Table {
                table: Slot::from(dependency_type).table_name(),
                feature,
            },
            Scope::Anywhere => NotFoundLocation::Anywhere,
        };
        Self {
            name,
            location,
            suggestions,
        }
    }
}

impl Display for DependencyRemovalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.location {
            NotFoundLocation::Table { table, feature } => {
                write!(f, "dependency `{}` was not found in {table}", self.name)?;
                if !feature.is_default() {
                    write!(f, " of feature `{feature}`")?;
                }
                Ok(())
            }
            NotFoundLocation::Anywhere => {
                write!(
                    f,
                    "dependency `{}` was not found in the workspace",
                    self.name
                )
            }
        }
    }
}

impl std::error::Error for DependencyRemovalError {}

impl Diagnostic for DependencyRemovalError {
    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        if self.suggestions.is_empty() {
            None
        } else {
            Some(Box::new(self.suggestions.join("\n")))
        }
    }
}

/// Diagnostic for a bare `pixi remove <pkg>` that matches the same package in
/// more than one location. We refuse to guess and ask the user to disambiguate.
#[derive(Debug)]
pub(super) struct AmbiguousRemovalError {
    name: String,
    locations: Vec<(FeatureName, Slot)>,
}

impl AmbiguousRemovalError {
    pub(super) fn new(name: String, locations: &[Location]) -> Self {
        Self {
            name,
            locations: locations
                .iter()
                .map(|loc| (loc.feature.clone(), loc.slot))
                .collect(),
        }
    }
}

impl Display for AmbiguousRemovalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dependency `{}` is defined in multiple locations; specify which one to remove",
            self.name
        )
    }
}

impl std::error::Error for AmbiguousRemovalError {}

impl Diagnostic for AmbiguousRemovalError {
    fn help<'a>(&'a self) -> Option<Box<dyn Display + 'a>> {
        let hints = self
            .locations
            .iter()
            .map(|(feature, slot)| {
                format!("- {}", format_exact_location(&self.name, *slot, feature))
            })
            .join("\n");
        Some(Box::new(hints))
    }
}

fn collect_suggestions(manifest: &WorkspaceManifest, name: &str, scope: &Scope) -> Vec<String> {
    match scope {
        Scope::Table {
            dependency_type,
            feature,
        } => collect_table_suggestions(manifest, name, *dependency_type, feature),
        Scope::Anywhere => collect_similar_suggestions(manifest, name),
    }
}

/// Suggestions for the strict path: point at the same package in other tables /
/// features, or at a similarly-named package in the requested table.
fn collect_table_suggestions(
    manifest: &WorkspaceManifest,
    name: &str,
    dependency_type: DependencyType,
    feature: &FeatureName,
) -> Vec<String> {
    let current_slot = Slot::from(dependency_type);
    let target_conda = PackageName::try_from(name).ok();
    let target_pypi = PypiPackageName::from_str(name).ok();

    let mut exact_locations: Vec<(FeatureName, Slot)> = Vec::new();
    let mut similar_in_target: Vec<(String, FeatureName)> = Vec::new();

    for entry in walk_dependencies(manifest) {
        if is_exact_match(&entry, target_conda.as_ref(), target_pypi.as_ref()) {
            let location = (entry.feature.clone(), entry.slot);
            if (entry.feature, entry.slot) != (feature, current_slot)
                && !exact_locations.contains(&location)
            {
                exact_locations.push(location);
            }
            continue;
        }

        let same_slot_and_feature = entry.slot == current_slot && entry.feature == feature;
        let candidate = (entry.name.clone(), entry.feature.clone());
        if same_slot_and_feature
            && is_similar(name, &entry.name)
            && !similar_in_target.contains(&candidate)
        {
            similar_in_target.push(candidate);
        }
    }

    let mut suggestions = Vec::new();
    for (feat_name, slot) in exact_locations {
        suggestions.push(format_exact_location(name, slot, &feat_name));
    }
    if !similar_in_target.is_empty() {
        suggestions.push(did_you_mean(&similar_in_target));
    }
    suggestions
}

/// Suggestions for the auto path: the package was nowhere to be found, so the
/// only useful hint is a similarly-named package anywhere in the workspace,
/// together with the feature it lives in.
fn collect_similar_suggestions(manifest: &WorkspaceManifest, name: &str) -> Vec<String> {
    let mut similar: Vec<(String, FeatureName)> = Vec::new();
    for entry in walk_dependencies(manifest) {
        let candidate = (entry.name.clone(), entry.feature.clone());
        if is_similar(name, &entry.name) && !similar.contains(&candidate) {
            similar.push(candidate);
        }
    }
    if similar.is_empty() {
        Vec::new()
    } else {
        vec![did_you_mean(&similar)]
    }
}

fn did_you_mean(candidates: &[(String, FeatureName)]) -> String {
    let rendered = candidates
        .iter()
        .map(|(name, feature)| similar_candidate(name, feature))
        .join(", ");
    format!("did you mean {rendered}?")
}

/// Render a single "did you mean" candidate, naming the feature it lives in
/// unless that is the default feature (where the location is unambiguous).
fn similar_candidate(name: &str, feature: &FeatureName) -> String {
    if feature.is_default() {
        format!("`{name}`")
    } else {
        format!(
            "`{name}` from feature `{}`",
            consts::FEATURE_STYLE.apply_to(feature)
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pixi_manifest::SpecType;
    use pixi_test_utils::format_diagnostic;

    use super::*;

    fn parse(toml: &str) -> WorkspaceManifest {
        WorkspaceManifest::from_toml_str_with_base_dir(toml, Path::new(""))
            .expect("failed to parse manifest")
    }

    fn render(
        manifest: &WorkspaceManifest,
        name: &str,
        dep_type: DependencyType,
        feature: &FeatureName,
    ) -> String {
        let err = DependencyRemovalError::new(
            name.to_string(),
            manifest,
            Scope::Table {
                dependency_type: dep_type,
                feature: feature.clone(),
            },
        );
        format_diagnostic(&err)
    }

    const MIXED_MANIFEST: &str = r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[dependencies]
ruff = "*"

[pypi-dependencies]
polars = "*"

[host-dependencies]
openssl = "*"

[build-dependencies]
cmake = "*"

[feature.dev.dependencies]
numpy = "*"

[feature.dev.pypi-dependencies]
pandas = "*"
"#;

    #[test]
    fn missing_dep_lives_in_pypi() {
        // `pixi remove polars` (no flag) when polars is a pypi-dependency.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "polars",
                DependencyType::CondaDependency(SpecType::Run),
                &FeatureName::DEFAULT,
            ),
            @r"
          × dependency `polars` was not found in dependencies
          help: `polars` is a pypi-dependencies entry in the default feature; try `pixi remove --pypi polars`
        "
        );
    }

    #[test]
    fn missing_dep_lives_in_conda() {
        // `pixi remove --pypi ruff` when ruff is a conda dependency.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "ruff",
                DependencyType::PypiDependency,
                &FeatureName::DEFAULT,
            ),
            @r"
          × dependency `ruff` was not found in pypi-dependencies
          help: `ruff` is a dependencies entry in the default feature; try `pixi remove ruff`
        "
        );
    }

    #[test]
    fn missing_dep_lives_in_host_deps() {
        // `pixi remove openssl` when openssl is in host-dependencies.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "openssl",
                DependencyType::CondaDependency(SpecType::Run),
                &FeatureName::DEFAULT,
            ),
            @r"
          × dependency `openssl` was not found in dependencies
          help: `openssl` is a host-dependencies entry in the default feature; try `pixi remove --host openssl`
        "
        );
    }

    #[test]
    fn missing_dep_lives_in_build_deps() {
        // `pixi remove cmake` when cmake is in build-dependencies.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "cmake",
                DependencyType::CondaDependency(SpecType::Run),
                &FeatureName::DEFAULT,
            ),
            @r"
          × dependency `cmake` was not found in dependencies
          help: `cmake` is a build-dependencies entry in the default feature; try `pixi remove --build cmake`
        "
        );
    }

    #[test]
    fn missing_dep_lives_in_other_feature() {
        // `pixi remove numpy` from the default feature when numpy is only in
        // feature `dev`.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "numpy",
                DependencyType::CondaDependency(SpecType::Run),
                &FeatureName::DEFAULT,
            ),
            @r"
          × dependency `numpy` was not found in dependencies
          help: `numpy` is a dependencies entry in feature `dev`; try `pixi remove --feature dev numpy`
        "
        );
    }

    #[test]
    fn missing_dep_typo_suggests_similar_name() {
        // `pixi remove --pypi polrs` when polars exists. Jaro similarity
        // catches the typo.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "polrs",
                DependencyType::PypiDependency,
                &FeatureName::DEFAULT,
            ),
            @r"
          × dependency `polrs` was not found in pypi-dependencies
          help: did you mean `polars`?
        "
        );
    }

    #[test]
    fn missing_dep_truly_absent() {
        // `pixi remove fizzbuzz` with nothing matching. No help text.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "fizzbuzz",
                DependencyType::CondaDependency(SpecType::Run),
                &FeatureName::DEFAULT,
            ),
            @"  × dependency `fizzbuzz` was not found in dependencies"
        );
    }

    #[test]
    fn missing_dep_wrong_dep_type_in_non_default_feature() {
        // `pixi remove --pypi numpy --feature dev`: numpy exists in feature
        // dev but as a conda dep, not a pypi dep.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "numpy",
                DependencyType::PypiDependency,
                &FeatureName::from("dev"),
            ),
            @r"
          × dependency `numpy` was not found in pypi-dependencies of feature `dev`
          help: `numpy` is a dependencies entry in feature `dev`; try `pixi remove --feature dev numpy`
        "
        );
    }

    #[test]
    fn missing_dep_pypi_in_non_default_feature() {
        // `pixi remove pandas --feature dev`: pandas exists in feature dev
        // but as a pypi dep.
        let manifest = parse(MIXED_MANIFEST);
        insta::assert_snapshot!(
            render(
                &manifest,
                "pandas",
                DependencyType::CondaDependency(SpecType::Run),
                &FeatureName::from("dev"),
            ),
            @r"
          × dependency `pandas` was not found in dependencies of feature `dev`
          help: `pandas` is a pypi-dependencies entry in feature `dev`; try `pixi remove --pypi --feature dev pandas`
        "
        );
    }

    #[test]
    fn auto_not_found_searches_whole_workspace() {
        // A bare `pixi remove fizzbuzz` that matches nothing anywhere.
        let manifest = parse(MIXED_MANIFEST);
        let err = DependencyRemovalError::new("fizzbuzz".to_string(), &manifest, Scope::Anywhere);
        insta::assert_snapshot!(
            format_diagnostic(&err),
            @"  × dependency `fizzbuzz` was not found in the workspace"
        );
    }

    #[test]
    fn auto_not_found_suggests_similar_name_anywhere() {
        // A bare `pixi remove pollars` should suggest the pypi `polars`.
        let manifest = parse(MIXED_MANIFEST);
        let err = DependencyRemovalError::new("pollars".to_string(), &manifest, Scope::Anywhere);
        insta::assert_snapshot!(
            format_diagnostic(&err),
            @r"
          × dependency `pollars` was not found in the workspace
          help: did you mean `polars`?
        "
        );
    }

    #[test]
    fn auto_not_found_similar_name_names_the_feature() {
        // `pandas` only lives in feature `dev`, so the suggestion should say so.
        let manifest = parse(MIXED_MANIFEST);
        let err = DependencyRemovalError::new("pandes".to_string(), &manifest, Scope::Anywhere);
        insta::assert_snapshot!(
            format_diagnostic(&err),
            @r"
          × dependency `pandes` was not found in the workspace
          help: did you mean `pandas` from feature `dev`?
        "
        );
    }

    const DUPLICATE_MANIFEST: &str = r#"
[workspace]
name = "test"
channels = []
platforms = ["linux-64"]

[pypi-dependencies]
requests = "*"

[feature.dev.dependencies]
requests = "*"
"#;

    #[test]
    fn ambiguous_removal_lists_each_location() {
        // `requests` is a pypi dep and a conda dep in feature `dev`: with no
        // default `[dependencies]` match to fall back on, the removal is
        // ambiguous and each location's command is spelled out.
        let manifest = parse(DUPLICATE_MANIFEST);
        let locations = crate::remove::locate::locate(&manifest, "requests");
        let err = AmbiguousRemovalError::new("requests".to_string(), &locations);
        insta::assert_snapshot!(
            format_diagnostic(&err),
            @r"
          × dependency `requests` is defined in multiple locations; specify which one to remove
          help: - `requests` is a pypi-dependencies entry in the default feature; try `pixi remove --pypi requests`
                - `requests` is a dependencies entry in feature `dev`; try `pixi remove --feature dev requests`
        "
        );
    }
}
