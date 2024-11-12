use std::{
    borrow::Cow,
    collections::HashSet,
    io::{stderr, Write},
};

use crate::Project;
use ahash::HashMap;
use indexmap::IndexMap;
use itertools::{Either, Itertools};
use pixi_consts::consts;
use pixi_manifest::FeaturesExt;
use rattler_conda_types::Platform;
use rattler_lock::{LockFile, Package};
use serde::Serialize;
use serde_json::Value;
use tabwriter::TabWriter;

// Represents the differences between two sets of packages.
#[derive(Default, Clone)]
pub struct PackagesDiff {
    pub added: Vec<rattler_lock::Package>,
    pub removed: Vec<rattler_lock::Package>,
    pub changed: Vec<(rattler_lock::Package, rattler_lock::Package)>,
}

impl PackagesDiff {
    /// Returns true if the diff is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Contains the changes between two lock-files.
pub struct LockFileDiff {
    pub environment: IndexMap<String, IndexMap<Platform, PackagesDiff>>,
}

impl LockFileDiff {
    /// Determine the difference between two lock-files.
    pub(crate) fn from_lock_files(previous: &LockFile, current: &LockFile) -> Self {
        let mut result = Self {
            environment: IndexMap::new(),
        };

        for (environment_name, environment) in current.environments() {
            let previous = previous.environment(environment_name);

            let mut environment_diff = IndexMap::new();

            for (platform, packages) in environment.packages_by_platform() {
                // Determine the packages that were previously there.
                let (mut previous_conda_packages, mut previous_pypi_packages): (
                    HashMap<_, _>,
                    HashMap<_, _>,
                ) = previous
                    .as_ref()
                    .and_then(|e| e.packages(platform))
                    .into_iter()
                    .flatten()
                    .partition_map(|p| match p {
                        rattler_lock::Package::Conda(p) => {
                            Either::Left((p.package_record().name.clone(), p))
                        }
                        rattler_lock::Package::Pypi(p) => {
                            Either::Right((p.package_data().name.clone(), p))
                        }
                    });

                let mut diff = PackagesDiff::default();

                // Find new and changed packages
                for package in packages {
                    match package {
                        Package::Conda(p) => {
                            let name = &p.package_record().name;
                            match previous_conda_packages.remove(name) {
                                Some(previous) if previous.location() != p.location() => {
                                    diff.changed
                                        .push((Package::Conda(previous), Package::Conda(p)));
                                }
                                None => {
                                    diff.added.push(Package::Conda(p));
                                }
                                _ => {}
                            }
                        }
                        Package::Pypi(p) => {
                            let name = &p.package_data().name;
                            match previous_pypi_packages.remove(name) {
                                Some(previous) if previous.location() != p.location() => {
                                    diff.changed
                                        .push((Package::Pypi(previous), Package::Pypi(p)));
                                }
                                None => {
                                    diff.added.push(Package::Pypi(p));
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Determine packages that were removed
                for (_, p) in previous_conda_packages {
                    diff.removed.push(Package::Conda(p));
                }
                for (_, p) in previous_pypi_packages {
                    diff.removed.push(Package::Pypi(p));
                }

                environment_diff.insert(platform, diff);
            }

            // Find platforms that were completely removed
            for (platform, packages) in previous
                .as_ref()
                .map(|e| e.packages_by_platform())
                .into_iter()
                .flatten()
                .filter(|(platform, _)| !environment_diff.contains_key(platform))
                .collect_vec()
            {
                let mut diff = PackagesDiff::default();
                for package in packages {
                    match package {
                        Package::Conda(p) => {
                            diff.removed.push(Package::Conda(p));
                        }
                        Package::Pypi(p) => {
                            diff.removed.push(Package::Pypi(p));
                        }
                    }
                }
                environment_diff.insert(platform, diff);
            }

            // Remove empty diffs
            environment_diff.retain(|_, diff| !diff.is_empty());

            result
                .environment
                .insert(environment_name.to_string(), environment_diff);
        }

        // Find environments that were completely removed
        for (environment_name, environment) in previous
            .environments()
            .filter(|(name, _)| !result.environment.contains_key(*name))
            .collect_vec()
        {
            let mut environment_diff = IndexMap::new();
            for (platform, packages) in environment.packages_by_platform() {
                let mut diff = PackagesDiff::default();
                for package in packages {
                    match package {
                        Package::Conda(p) => {
                            diff.removed.push(Package::Conda(p));
                        }
                        Package::Pypi(p) => {
                            diff.removed.push(Package::Pypi(p));
                        }
                    }
                }
                environment_diff.insert(platform, diff);
            }
            result
                .environment
                .insert(environment_name.to_string(), environment_diff);
        }

        // Remove empty environments
        result.environment.retain(|_, diff| !diff.is_empty());

        result
    }

    /// Returns true if the diff is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.environment.is_empty()
    }

    // Format the lock-file diff.
    pub(crate) fn print(&self) -> std::io::Result<()> {
        let mut writer = TabWriter::new(stderr());
        for (idx, (environment_name, environment)) in self
            .environment
            .iter()
            .sorted_by(|(a, _), (b, _)| a.cmp(b))
            .enumerate()
        {
            // Find the changes that happened in all platforms.
            let changes_by_platform = environment
                .into_iter()
                .map(|(platform, packages)| {
                    let changes = Self::format_changes(packages)
                        .into_iter()
                        .collect::<HashSet<_>>();
                    (platform, changes)
                })
                .collect::<Vec<_>>();

            // Find the changes that happened in all platforms.
            let common_changes = changes_by_platform
                .iter()
                .fold(None, |acc, (_, changes)| match acc {
                    None => Some(changes.clone()),
                    Some(acc) => Some(acc.intersection(changes).cloned().collect()),
                })
                .unwrap_or_default();

            // Add a new line between environments
            if idx > 0 {
                writeln!(writer, "\t\t\t",)?;
            }

            writeln!(
                writer,
                "{}: {}\t\t\t",
                console::style("Environment").underlined(),
                consts::ENVIRONMENT_STYLE.apply_to(environment_name)
            )?;

            // Print the common changes.
            for (_, line) in common_changes.iter().sorted_by_key(|(name, _)| name) {
                writeln!(writer, "  {}", line)?;
            }

            // Print the per-platform changes.
            for (platform, changes) in changes_by_platform {
                let mut changes = changes
                    .iter()
                    .filter(|change| !common_changes.contains(change))
                    .sorted_by_key(|(name, _)| name)
                    .peekable();
                if changes.peek().is_some() {
                    writeln!(
                        writer,
                        "{}: {}:{}\t\t\t",
                        console::style("Platform").underlined(),
                        consts::ENVIRONMENT_STYLE.apply_to(environment_name),
                        consts::PLATFORM_STYLE.apply_to(platform),
                    )?;
                    for (_, line) in changes {
                        writeln!(writer, "  {}", line)?;
                    }
                }
            }
        }

        writer.flush()?;

        Ok(())
    }

    fn format_changes(packages: &PackagesDiff) -> Vec<(Cow<'_, str>, String)> {
        enum Change<'i> {
            Added(&'i Package),
            Removed(&'i Package),
            Changed(&'i Package, &'i Package),
        }

        fn format_package_identifier(package: &Package) -> String {
            match package {
                Package::Conda(p) => format!(
                    "{} {}",
                    &p.package_record().version.as_str(),
                    &p.package_record().build
                ),
                Package::Pypi(p) => p.package_data().version.to_string(),
            }
        }

        itertools::chain!(
            packages.added.iter().map(Change::Added),
            packages.removed.iter().map(Change::Removed),
            packages.changed.iter().map(|a| Change::Changed(&a.0, &a.1))
        )
        .sorted_by_key(|c| match c {
            Change::Added(p) => p.name(),
            Change::Removed(p) => p.name(),
            Change::Changed(p, _) => p.name(),
        })
        .map(|p| match p {
            Change::Added(p) => (
                p.name(),
                format!(
                    "{} {} {}\t{}\t\t",
                    console::style("+").green(),
                    match p {
                        Package::Conda(_) => consts::CondaEmoji.to_string(),
                        Package::Pypi(_) => consts::PypiEmoji.to_string(),
                    },
                    p.name(),
                    format_package_identifier(p)
                ),
            ),
            Change::Removed(p) => (
                p.name(),
                format!(
                    "{} {} {}\t{}\t\t",
                    console::style("-").red(),
                    match p {
                        Package::Conda(_) => consts::CondaEmoji.to_string(),
                        Package::Pypi(_) => consts::PypiEmoji.to_string(),
                    },
                    p.name(),
                    format_package_identifier(p)
                ),
            ),
            Change::Changed(previous, current) => {
                fn choose_style<'a>(a: &'a str, b: &'a str) -> console::StyledObject<&'a str> {
                    if a == b {
                        console::style(a).dim()
                    } else {
                        console::style(a)
                    }
                }

                let name = previous.name();
                let line = match (previous, current) {
                    (Package::Conda(previous), Package::Conda(current)) => {
                        let previous = previous.package_record();
                        let current = current.package_record();

                        format!(
                            "{} {} {}\t{} {}\t->\t{} {}",
                            console::style("~").yellow(),
                            consts::CondaEmoji,
                            name,
                            choose_style(&previous.version.as_str(), &current.version.as_str()),
                            choose_style(previous.build.as_str(), current.build.as_str()),
                            choose_style(&current.version.as_str(), &previous.version.as_str()),
                            choose_style(current.build.as_str(), previous.build.as_str()),
                        )
                    }
                    (Package::Pypi(previous), Package::Pypi(current)) => {
                        let previous = previous.package_data();
                        let current = current.package_data();

                        format!(
                            "{} {} {}\t{}\t->\t{}",
                            console::style("~").yellow(),
                            consts::PypiEmoji,
                            name,
                            choose_style(
                                &previous.version.to_string(),
                                &current.version.to_string()
                            ),
                            choose_style(
                                &current.version.to_string(),
                                &previous.version.to_string()
                            ),
                        )
                    }
                    _ => unreachable!(),
                };

                (name, line)
            }
        })
        .collect()
    }
}

#[derive(Serialize, Clone)]
pub struct JsonPackageDiff {
    name: String,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
    #[serde(rename = "type")]
    ty: JsonPackageType,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    explicit: bool,
}

#[derive(Serialize, Copy, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum JsonPackageType {
    Conda,
    Pypi,
}

#[derive(Serialize, Clone)]
pub struct LockFileJsonDiff {
    pub version: usize,
    pub environment: IndexMap<String, IndexMap<Platform, Vec<JsonPackageDiff>>>,
}

impl LockFileJsonDiff {
    pub fn new(project: &Project, value: LockFileDiff) -> Self {
        let mut environment = IndexMap::new();

        for (environment_name, environment_diff) in value.environment {
            let mut environment_diff_json = IndexMap::new();

            for (platform, packages_diff) in environment_diff {
                let conda_dependencies = project
                    .environment(environment_name.as_str())
                    .map(|env| env.dependencies(pixi_manifest::SpecType::Run, Some(platform)))
                    .unwrap_or_default();

                let pypi_dependencies = project
                    .environment(environment_name.as_str())
                    .map(|env| env.pypi_dependencies(Some(platform)))
                    .unwrap_or_default();

                let add_diffs = packages_diff.added.into_iter().map(|new| match new {
                    Package::Conda(pkg) => JsonPackageDiff {
                        name: pkg.package_record().name.as_normalized().to_string(),
                        before: None,
                        after: Some(serde_json::to_value(&pkg).unwrap()),
                        ty: JsonPackageType::Conda,
                        explicit: conda_dependencies.contains_key(&pkg.package_record().name),
                    },
                    Package::Pypi(pkg) => JsonPackageDiff {
                        name: pkg.package_data().name.as_dist_info_name().into_owned(),
                        before: None,
                        after: Some(serde_json::to_value(&pkg).unwrap()),
                        ty: JsonPackageType::Pypi,
                        explicit: pypi_dependencies.contains_key(&pkg.package_data().name),
                    },
                });

                let removed_diffs = packages_diff.removed.into_iter().map(|old| match old {
                    Package::Conda(pkg) => JsonPackageDiff {
                        name: pkg.package_record().name.as_normalized().to_string(),
                        before: Some(serde_json::to_value(&pkg).unwrap()),
                        after: None,
                        ty: JsonPackageType::Conda,
                        explicit: conda_dependencies.contains_key(&pkg.package_record().name),
                    },

                    Package::Pypi(pkg) => JsonPackageDiff {
                        name: pkg.package_data().name.as_dist_info_name().into_owned(),
                        before: Some(serde_json::to_value(&pkg).unwrap()),
                        after: None,
                        ty: JsonPackageType::Pypi,
                        explicit: pypi_dependencies.contains_key(&pkg.package_data().name),
                    },
                });

                let changed_diffs = packages_diff.changed.into_iter().map(|(old, new)| match (old, new) {
                    (Package::Conda(old), Package::Conda(new)) =>
                        {
                            let before = serde_json::to_value(&old).unwrap();
                            let after = serde_json::to_value(&new).unwrap();
                            let (before, after) = compute_json_diff(before, after);
                            JsonPackageDiff {
                                name: old.package_record().name.as_normalized().to_string(),
                                before: Some(before),
                                after: Some(after),
                                ty: JsonPackageType::Conda,
                                explicit: conda_dependencies.contains_key(&old.package_record().name),
                            }
                        }
                    (Package::Pypi(old), Package::Pypi(new)) => {
                        let before = serde_json::to_value(&old).unwrap();
                        let after = serde_json::to_value(&new).unwrap();
                        let (before, after) = compute_json_diff(before, after);
                        JsonPackageDiff {
                            name: old.package_data().name.as_dist_info_name().into_owned(),
                            before: Some(before),
                            after: Some(after),
                            ty: JsonPackageType::Pypi,
                            explicit: pypi_dependencies.contains_key(&old.package_data().name),
                        }
                    }
                    _ => unreachable!("packages cannot change type, they are represented as removals and inserts instead"),
                });

                let packages_diff_json = add_diffs
                    .chain(removed_diffs)
                    .chain(changed_diffs)
                    .sorted_by_key(|diff| diff.name.clone())
                    .collect_vec();

                environment_diff_json.insert(platform, packages_diff_json);
            }

            environment.insert(environment_name, environment_diff_json);
        }

        Self {
            version: 1,
            environment,
        }
    }
}

fn compute_json_diff(
    mut a: serde_json::Value,
    mut b: serde_json::Value,
) -> (serde_json::Value, serde_json::Value) {
    if let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object_mut()) {
        a.retain(|key, value| {
            if let Some(other_value) = b.get(key) {
                if other_value == value {
                    b.remove(key);
                    return false;
                }
            } else {
                b.insert(key.to_string(), Value::Null);
            }
            true
        });
    }
    (a, b)
}