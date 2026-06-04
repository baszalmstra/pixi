//! Discover sibling ROS packages inside a workspace.
//!
//! Walks a workspace root looking for `package.xml` files, honoring the
//! standard colcon ignore markers (`COLCON_IGNORE`, `AMENT_IGNORE`,
//! `CATKIN_IGNORE`). Returns the discovered packages keyed by the `<name>`
//! element together with a glob list suitable for pixi's metadata
//! invalidation cache.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use fs_err as fs;
use miette::Diagnostic;
use pixi_build_types::InputGlobSet;
use pixi_glob::GlobSet;
use rattler_build_recipe::stage0::{ConditionalList, Item, SerializableMatchSpec, Value};
use thiserror::Error;
use url::Url;

use crate::package_map::item_package_name;
use crate::package_xml::{PackageXml, PackageXmlError};

const PACKAGE_XML: &str = "package.xml";
const IGNORE_MARKERS: &[&str] = &["COLCON_IGNORE", "AMENT_IGNORE", "CATKIN_IGNORE"];

/// Result of walking a workspace root for ROS packages.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceDiscovery {
    /// Map from the `<name>` of each discovered package to the absolute path of
    /// its `package.xml`.
    pub packages: HashMap<String, PathBuf>,
    /// Structured glob description of the discovery, suitable for pixi's
    /// metadata invalidation cache.  Pixi replays the same walk later
    /// using the marker semantics carried here; we deliberately don't
    /// emit a flat `Vec<String>` form because per-pruned-dir exclusions
    /// (`!build/**`, `!log/**`, ...) and the hidden-folder exclusion
    /// (`!**/.*/**`) can't be expressed losslessly without the markers
    /// the structured form carries.
    pub input_glob_set: InputGlobSet,

    /// Set listing the pixi manifests discovered next to a `package.xml`
    /// (the packages [`sibling_source_spec`] references by directory). The
    /// caller watches it so a manifest change or removal invalidates the
    /// cached metadata. Empty when no sibling is a pixi package.
    pub pixi_manifest_input_glob_set: InputGlobSet,
}

#[derive(Debug, Error, Diagnostic)]
pub enum WorkspaceDiscoveryError {
    #[error("duplicate ROS package name `{name}` declared by both:\n  - {first}\n  - {second}")]
    #[diagnostic(help(
        "Each ROS package in a workspace must have a unique <name> in its package.xml."
    ))]
    DuplicateName {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error("workspace walk failed at {path}")]
    Walk {
        path: PathBuf,
        #[source]
        source: Box<pixi_glob::GlobSetError>,
    },

    #[error("failed to read {path}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: PackageXmlError,
    },
}

/// Discover every ROS package nested under `workspace_root`.
///
/// Directories containing any of `COLCON_IGNORE`, `AMENT_IGNORE`, or
/// `CATKIN_IGNORE` are pruned from the walk. Colcon writes these markers into
/// its own `build/`, `install/`, and `log/` directories, so honoring the
/// markers also handles those without any directory-name hardcoding.
///
/// Returns an error if two `package.xml` files declare the same `<name>`.
pub fn discover_ros_packages(
    workspace_root: &Path,
) -> Result<WorkspaceDiscovery, WorkspaceDiscoveryError> {
    let _span = tracing::info_span!(
        "ros_workspace_discovery",
        workspace_root = %workspace_root.display(),
    )
    .entered();
    let started = std::time::Instant::now();

    let mut packages: HashMap<String, PathBuf> = HashMap::new();

    if workspace_root.is_dir() {
        let mut marker_filenames = Vec::with_capacity(IGNORE_MARKERS.len() + 1);
        marker_filenames.push(PACKAGE_XML);
        marker_filenames.extend(IGNORE_MARKERS.iter().copied());

        // Hidden directories (`.git`, `.pixi`, `.vscode`, ...) are skipped
        // by the walker; matches colcon's behaviour and avoids descending
        // into the installed `.pixi/envs/.../share/...` tree whose
        // `package.xml`s are not workspace siblings.
        let matches = GlobSet::create([format!("**/{PACKAGE_XML}").as_str()])
            .with_exclude_hidden(true)
            .with_ignore_marker_filenames(marker_filenames)
            .collect_matching(workspace_root)
            .map_err(|source| WorkspaceDiscoveryError::Walk {
                path: workspace_root.to_path_buf(),
                source: Box::new(source),
            })?;

        for matched in matches {
            let package_xml = matched.into_path();
            let content = fs::read_to_string(&package_xml).map_err(|source| {
                WorkspaceDiscoveryError::ReadFile {
                    path: package_xml.clone(),
                    source,
                }
            })?;
            let parsed =
                PackageXml::parse(&content).map_err(|source| WorkspaceDiscoveryError::Parse {
                    path: package_xml.clone(),
                    source,
                })?;

            if let Some(existing) = packages.get(&parsed.name) {
                return Err(WorkspaceDiscoveryError::DuplicateName {
                    name: parsed.name,
                    first: existing.clone(),
                    second: package_xml,
                });
            }
            packages.insert(parsed.name, package_xml);
        }
    }

    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        packages = packages.len(),
        "ROS workspace discovery finished"
    );

    let mut markers: Vec<String> = Vec::with_capacity(IGNORE_MARKERS.len() + 1);
    markers.push(PACKAGE_XML.to_string());
    markers.extend(IGNORE_MARKERS.iter().map(|m| m.to_string()));
    let input_glob_set = InputGlobSet {
        patterns: vec![format!("**/{PACKAGE_XML}")],
        markers,
        exclude_hidden: true,
        // Caller (pixi-build-ros::main) fills in the workspace root
        // before emitting the recipe.
        root: None,
    };

    // Resolve the manifest of every already-discovered package that is also
    // a pixi package (cheap `is_file` probes, no second walk). They are
    // emitted as explicit anchored patterns rather than a `**/pixi.toml`
    // glob, so `ignore` only descends the relevant package dirs.
    let mut manifest_patterns: Vec<String> = packages
        .values()
        .filter_map(|package_xml| package_xml.parent())
        .filter_map(pixi_manifest_in_dir)
        .map(|manifest| {
            let rel = manifest
                .strip_prefix(workspace_root)
                .map(Path::to_path_buf)
                .unwrap_or(manifest);
            path_to_forward_slashes(&rel)
        })
        .collect();
    // Stable order for deterministic recipe output / cache identity.
    manifest_patterns.sort();
    let pixi_manifest_input_glob_set = InputGlobSet {
        patterns: manifest_patterns,
        markers: Vec::new(),
        exclude_hidden: true,
        root: None,
    };

    Ok(WorkspaceDiscovery {
        packages,
        input_glob_set,
        pixi_manifest_input_glob_set,
    })
}

fn path_to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Build a matchspec `ros-<distro>-<name>[url="source://?path=<rel>"]` that
/// parses into a source dependency on the sibling package.
///
/// The `path` targets the package *directory* when the sibling is itself a
/// pixi package, and the `package.xml` file otherwise. pixi identifies
/// source packages by their pinned path: a workspace dependency on a pixi
/// package resolves against the directory, so pointing at `package.xml`
/// here would resolve the same package twice under two locations, which the
/// solver rejects as duplicate records (prefix-dev/pixi#6277). Bare ROS
/// packages must keep the `package.xml` path (the ROS backend requires it).
pub fn sibling_source_spec(
    conda_name: &str,
    sibling_package_xml: &Path,
    manifest_root: &Path,
) -> String {
    let target = sibling_pixi_package_dir(sibling_package_xml)
        .unwrap_or_else(|| sibling_package_xml.to_path_buf());
    let rel = pathdiff::diff_paths(&target, manifest_root).unwrap_or(target);
    let rel = path_to_forward_slashes(&rel);

    let mut url = Url::from_str("source://").expect("static URL parses");
    url.query_pairs_mut().append_pair("path", &rel);
    format!("{conda_name}[url=\"{url}\"]")
}

/// If the directory containing `package_xml` is also a pixi package, return
/// that directory; otherwise `None` (callers keep the `package.xml` path).
fn sibling_pixi_package_dir(package_xml: &Path) -> Option<PathBuf> {
    let dir = package_xml.parent()?;
    pixi_manifest_in_dir(dir).map(|_| dir.to_path_buf())
}

/// The manifest that makes `dir` a pixi package: a `pixi.toml`, or a
/// `pyproject.toml` carrying a `[tool.pixi]` table. `None` if neither.
fn pixi_manifest_in_dir(dir: &Path) -> Option<PathBuf> {
    let pixi_toml = dir.join("pixi.toml");
    if pixi_toml.is_file() {
        return Some(pixi_toml);
    }
    let pyproject = dir.join("pyproject.toml");
    let has_tool_pixi = fs::read_to_string(&pyproject)
        .map(|content| {
            content.lines().any(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("[tool.pixi]") || trimmed.starts_with("[tool.pixi.")
            })
        })
        .unwrap_or(false);
    has_tool_pixi.then_some(pyproject)
}

/// Build the conda-formatted package name for a ROS package.
pub fn conda_name_for(distro: &str, ros_name: &str) -> String {
    format!("ros-{}-{}", distro, ros_name.replace('_', "-"))
}

/// Compute the `conda-name -> source-spec-string` map for every sibling
/// package discovered under the workspace, excluding the current package.
pub fn sibling_source_spec_map(
    discovered: &HashMap<String, PathBuf>,
    current_package_name: &str,
    manifest_root: &Path,
    distro_name: &str,
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (ros_name, package_xml_path) in discovered {
        if ros_name == current_package_name {
            continue;
        }
        let conda_name = conda_name_for(distro_name, ros_name);
        let spec = sibling_source_spec(&conda_name, package_xml_path, manifest_root);
        map.insert(conda_name, spec);
    }
    map
}

/// Filter a sibling source-spec map down to entries whose conda name is **not**
/// already declared in `existing`. Used to honor the "manual entries in
/// pixi.toml always win over discovery" rule on a per-requirement-class basis.
pub fn filter_unspecified<'a>(
    overrides: &'a HashMap<String, String>,
    existing: &ConditionalList<SerializableMatchSpec>,
) -> HashMap<String, &'a str> {
    let declared: std::collections::HashSet<String> =
        existing.iter().filter_map(item_package_name).collect();
    overrides
        .iter()
        .filter(|(name, _)| !declared.contains(*name))
        .map(|(name, spec)| (name.clone(), spec.as_str()))
        .collect()
}

/// Replace every binary `Item` in `list` whose package name appears as a key
/// in `overrides` with a source `Item` built from the override's spec string.
/// Items that don't match are kept as-is.
pub fn apply_sibling_overrides(
    list: ConditionalList<SerializableMatchSpec>,
    overrides: &HashMap<String, &str>,
) -> ConditionalList<SerializableMatchSpec> {
    if overrides.is_empty() {
        return list;
    }
    let items: Vec<Item<SerializableMatchSpec>> = list
        .iter()
        .map(|item| {
            if let Some(name) = item_package_name(item)
                && let Some(source_spec) = overrides.get(&name)
            {
                Item::Value(Value::new_concrete(
                    SerializableMatchSpec::from(*source_spec),
                    None,
                ))
            } else {
                item.clone()
            }
        })
        .collect();
    ConditionalList::new(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_err as fs;
    use tempfile::tempdir;

    fn write_package_xml(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        let xml = format!(
            r#"<?xml version="1.0"?>
<package format="3">
  <name>{name}</name>
  <version>0.0.1</version>
  <description>test</description>
  <maintainer email="test@example.com">Tester</maintainer>
  <license>MIT</license>
</package>
"#
        );
        fs::write(dir.join("package.xml"), xml).unwrap();
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn discovers_top_level_packages() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("pkg_a"), "pkg_a");
        write_package_xml(&root.join("pkg_b"), "pkg_b");

        let result = discover_ros_packages(root).unwrap();

        assert_eq!(result.packages.len(), 2);
        assert_eq!(
            result.packages.get("pkg_a").unwrap(),
            &root.join("pkg_a").join("package.xml"),
        );
        assert_eq!(
            result.packages.get("pkg_b").unwrap(),
            &root.join("pkg_b").join("package.xml"),
        );
    }

    #[test]
    fn discovers_nested_packages() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("src").join("pkg_a"), "pkg_a");
        write_package_xml(&root.join("src").join("nested").join("pkg_b"), "pkg_b");

        let result = discover_ros_packages(root).unwrap();
        assert_eq!(result.packages.len(), 2);
        assert!(result.packages.contains_key("pkg_a"));
        assert!(result.packages.contains_key("pkg_b"));
    }

    #[test]
    fn colcon_ignore_prunes_subtree() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("pkg_a"), "pkg_a");
        // pkg_b is inside a directory with COLCON_IGNORE
        write_package_xml(&root.join("build").join("pkg_b"), "pkg_b");
        touch(&root.join("build").join("COLCON_IGNORE"));

        let result = discover_ros_packages(root).unwrap();
        assert_eq!(result.packages.len(), 1);
        assert!(result.packages.contains_key("pkg_a"));
        // Pixi re-runs the discovery with the same marker semantics to
        // learn that `build/` is pruned; the marker carries that intent.
        assert!(
            result
                .input_glob_set
                .markers
                .iter()
                .any(|m| m == "COLCON_IGNORE")
        );
    }

    #[test]
    fn ament_and_catkin_ignore_also_prune() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("a").join("pkg"), "in_ament");
        touch(&root.join("a").join("AMENT_IGNORE"));
        write_package_xml(&root.join("b").join("pkg"), "in_catkin");
        touch(&root.join("b").join("CATKIN_IGNORE"));
        write_package_xml(&root.join("c").join("pkg"), "visible");

        let result = discover_ros_packages(root).unwrap();
        assert_eq!(result.packages.len(), 1);
        assert!(result.packages.contains_key("visible"));
    }

    #[test]
    fn duplicate_name_is_an_error() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("a"), "same_name");
        write_package_xml(&root.join("b"), "same_name");

        let err = discover_ros_packages(root).unwrap_err();
        match err {
            WorkspaceDiscoveryError::DuplicateName { name, .. } => {
                assert_eq!(name, "same_name");
            }
            other => panic!("expected DuplicateName, got {other:?}"),
        }
    }

    #[test]
    fn empty_workspace_returns_no_packages_but_keeps_glob_set() {
        let tmp = tempdir().unwrap();
        let result = discover_ros_packages(tmp.path()).unwrap();
        assert!(result.packages.is_empty());
        assert_eq!(
            result.input_glob_set.patterns,
            vec!["**/package.xml".to_string()]
        );
        assert!(
            result
                .input_glob_set
                .markers
                .iter()
                .any(|m| m == "COLCON_IGNORE"),
            "expected COLCON_IGNORE marker, got: {:?}",
            result.input_glob_set.markers
        );
        assert!(result.input_glob_set.exclude_hidden);
    }

    #[test]
    fn nonexistent_workspace_root_returns_empty() {
        let result = discover_ros_packages(Path::new("/this/path/does/not/exist")).unwrap();
        assert!(result.packages.is_empty());
    }

    #[test]
    fn hidden_directories_are_pruned() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("src").join("pkg_a"), "pkg_a");

        // Simulate the contents of a built `.pixi/envs` tree: installed
        // packages bring their own package.xml files which must not be
        // mistaken for sibling sources.
        write_package_xml(
            &root
                .join(".pixi")
                .join("envs")
                .join("default")
                .join("share")
                .join("foo"),
            "foo",
        );
        // Hidden dirs other than .pixi should also be skipped (matches colcon).
        write_package_xml(&root.join(".git").join("worktree").join("pkg_b"), "pkg_b");

        let result = discover_ros_packages(root).unwrap();
        assert_eq!(result.packages.len(), 1);
        assert!(result.packages.contains_key("pkg_a"));
        assert!(result.input_glob_set.exclude_hidden);
    }

    /// Pull the decoded `path` query value out of a spec produced by
    /// [`sibling_source_spec`].
    fn extract_source_path(spec: &str) -> String {
        let start = spec.find("url=\"").expect("spec has a url") + "url=\"".len();
        let end = start + spec[start..].find('"').expect("url is quoted");
        let url = Url::parse(&spec[start..end]).expect("valid url");
        url.query_pairs()
            .find(|(k, _)| k == "path")
            .map(|(_, v)| v.into_owned())
            .expect("url carries a path")
    }

    #[test]
    fn sibling_source_spec_targets_package_xml_for_bare_ros() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("pkg_a"), "pkg_a");
        write_package_xml(&root.join("pkg_b"), "pkg_b");

        let spec = sibling_source_spec(
            "ros-jazzy-pkg-b",
            &root.join("pkg_b").join("package.xml"),
            &root.join("pkg_a"),
        );
        assert_eq!(extract_source_path(&spec), "../pkg_b/package.xml");
    }

    #[test]
    fn sibling_source_spec_targets_directory_for_pixi_package() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("pkg_a"), "pkg_a");
        write_package_xml(&root.join("pkg_b"), "pkg_b");
        // pkg_b is also a pixi package -> reference its directory.
        fs::write(root.join("pkg_b").join("pixi.toml"), "[package]\n").unwrap();

        let spec = sibling_source_spec(
            "ros-jazzy-pkg-b",
            &root.join("pkg_b").join("package.xml"),
            &root.join("pkg_a"),
        );
        assert_eq!(extract_source_path(&spec), "../pkg_b");
    }

    #[test]
    fn sibling_source_spec_targets_directory_for_pyproject_pixi_package() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("pkg_a"), "pkg_a");
        write_package_xml(&root.join("pkg_b"), "pkg_b");
        fs::write(
            root.join("pkg_b").join("pyproject.toml"),
            "[tool.pixi.package]\nname = \"pkg_b\"\n",
        )
        .unwrap();

        let spec = sibling_source_spec(
            "ros-jazzy-pkg-b",
            &root.join("pkg_b").join("package.xml"),
            &root.join("pkg_a"),
        );
        assert_eq!(extract_source_path(&spec), "../pkg_b");
    }

    #[test]
    fn sibling_source_spec_keeps_package_xml_for_non_pixi_pyproject() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        write_package_xml(&root.join("pkg_a"), "pkg_a");
        write_package_xml(&root.join("pkg_b"), "pkg_b");
        // A pyproject without a `[tool.pixi]` table is not a pixi package,
        // so pointing at the directory would break discovery -> keep the
        // `package.xml` path.
        fs::write(
            root.join("pkg_b").join("pyproject.toml"),
            "[build-system]\nrequires = [\"setuptools\"]\n",
        )
        .unwrap();

        let spec = sibling_source_spec(
            "ros-jazzy-pkg-b",
            &root.join("pkg_b").join("package.xml"),
            &root.join("pkg_a"),
        );
        assert_eq!(extract_source_path(&spec), "../pkg_b/package.xml");
    }

    #[test]
    fn pixi_manifest_set_lists_only_discovered_manifests() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        // Workspace-root manifest is not a discovered package -> not listed.
        fs::write(root.join("pixi.toml"), "[workspace]\n").unwrap();
        // Bare ROS package -> not listed.
        write_package_xml(&root.join("src").join("pkg_a"), "pkg_a");
        // pixi.toml package -> listed.
        write_package_xml(&root.join("src").join("pkg_b"), "pkg_b");
        fs::write(
            root.join("src").join("pkg_b").join("pixi.toml"),
            "[package]\n",
        )
        .unwrap();
        // pyproject with `[tool.pixi]` -> listed.
        write_package_xml(&root.join("src").join("pkg_c"), "pkg_c");
        fs::write(
            root.join("src").join("pkg_c").join("pyproject.toml"),
            "[tool.pixi.package]\nname = \"pkg_c\"\n",
        )
        .unwrap();

        let set = discover_ros_packages(root)
            .unwrap()
            .pixi_manifest_input_glob_set;
        assert_eq!(
            set.patterns,
            vec![
                "src/pkg_b/pixi.toml".to_string(),
                "src/pkg_c/pyproject.toml".to_string(),
            ],
            "only discovered pixi manifests, relative and sorted"
        );
        assert!(set.markers.is_empty());

        // Walked from the workspace root the set matches exactly those
        // manifests, and never the workspace-root manifest.
        let matches: Vec<PathBuf> = GlobSet::create(set.patterns.iter().map(String::as_str))
            .with_exclude_hidden(set.exclude_hidden)
            .collect_matching(root)
            .unwrap()
            .into_iter()
            .map(|m| m.into_path())
            .collect();
        let mut matches = matches;
        matches.sort();
        assert_eq!(
            matches,
            vec![
                root.join("src").join("pkg_b").join("pixi.toml"),
                root.join("src").join("pkg_c").join("pyproject.toml"),
            ],
        );
    }
}
