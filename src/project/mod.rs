mod dependencies;
mod environment;
pub mod errors;
mod grouped_environment;
pub mod manifest;
mod solve_group;
pub mod virtual_packages;

use indexmap::{Equivalent, IndexMap, IndexSet};
use miette::{IntoDiagnostic, NamedSource, WrapErr};
use once_cell::sync::OnceCell;
use rattler_conda_types::{Channel, GenericVirtualPackage, Platform, Version};
use rattler_networking::AuthenticationMiddleware;
use reqwest_middleware::ClientWithMiddleware;
use rip::index::PackageSources;
use rip::{index::PackageDb, normalize_index_url};
use std::hash::Hash;
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsStr,
    fmt::{Debug, Formatter},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    config,
    consts::{self, PROJECT_MANIFEST},
    task::Task,
};
use manifest::{EnvironmentName, Manifest, PyPiRequirement, SystemRequirements};
use url::Url;

use crate::task::TaskName;
pub use dependencies::Dependencies;
pub use environment::Environment;
pub use grouped_environment::{GroupedEnvironment, GroupedEnvironmentName};
pub use solve_group::SolveGroup;

/// The dependency types we support
#[derive(Debug, Copy, Clone)]
pub enum DependencyType {
    CondaDependency(SpecType),
    PypiDependency,
}

impl DependencyType {
    /// Convert to a name used in the manifest
    pub fn name(&self) -> &'static str {
        match self {
            DependencyType::CondaDependency(dep) => dep.name(),
            DependencyType::PypiDependency => consts::PYPI_DEPENDENCIES,
        }
    }
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
/// What kind of dependency spec do we have
pub enum SpecType {
    /// Host dependencies are used that are needed by the host environment when running the project
    Host,
    /// Build dependencies are used when we need to build the project, may not be required at runtime
    Build,
    /// Regular dependencies that are used when we need to run the project
    Run,
}

impl SpecType {
    /// Convert to a name used in the manifest
    pub fn name(&self) -> &'static str {
        match self {
            SpecType::Host => "host-dependencies",
            SpecType::Build => "build-dependencies",
            SpecType::Run => "dependencies",
        }
    }

    /// Returns all the variants of the enum
    pub fn all() -> impl Iterator<Item = SpecType> {
        [SpecType::Run, SpecType::Host, SpecType::Build].into_iter()
    }
}

/// The pixi project, this main struct to interact with the project. This struct holds the
/// `Manifest` and has functions to modify or request information from it.
/// This allows in the future to have multiple environments or manifests linked to a project.
#[derive(Clone)]
pub struct Project {
    /// Root folder of the project
    root: PathBuf,
    /// The PyPI package db for this project
    package_db: OnceCell<Arc<PackageDb>>,
    /// Reqwest client shared for this project
    client: reqwest::Client,
    /// Authenticated reqwest client shared for this project
    authenticated_client: ClientWithMiddleware,
    /// The manifest for the project
    pub(crate) manifest: Manifest,
}

impl Debug for Project {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Project")
            .field("root", &self.root)
            .field("manifest", &self.manifest)
            .finish()
    }
}

impl Project {
    /// Constructs a new instance from an internal manifest representation
    pub fn from_manifest(manifest: Manifest) -> Self {
        let client = reqwest::Client::new();
        let authenticated_client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
            .with_arc(Arc::new(AuthenticationMiddleware::default()))
            .build();
        Self {
            root: Default::default(),
            package_db: Default::default(),
            client,
            authenticated_client,
            manifest,
        }
    }

    /// Constructs a project from a manifest.
    pub fn from_str(root: &Path, content: &str) -> miette::Result<Self> {
        let manifest = Manifest::from_str(root, content)?;
        Ok(Self::from_manifest(manifest))
    }

    /// Discovers the project manifest file in the current directory or any of the parent
    /// directories.
    /// This will also set the current working directory to the project root.
    pub fn discover() -> miette::Result<Self> {
        let project_toml = match find_project_root() {
            Some(root) => root.join(PROJECT_MANIFEST),
            None => miette::bail!("could not find {}", PROJECT_MANIFEST),
        };
        Self::load(&project_toml)
    }

    /// Returns the source code of the project as [`NamedSource`].
    /// Used in error reporting.
    pub fn manifest_named_source(&self) -> NamedSource<String> {
        NamedSource::new(PROJECT_MANIFEST, self.manifest.contents.clone())
    }

    /// Loads a project from manifest file.
    fn load(manifest_path: &Path) -> miette::Result<Self> {
        // Determine the parent directory of the manifest file
        let full_path = dunce::canonicalize(manifest_path).into_diagnostic()?;
        if full_path.file_name().and_then(OsStr::to_str) != Some(PROJECT_MANIFEST) {
            miette::bail!("the manifest-path must point to a {PROJECT_MANIFEST} file");
        }

        let root = full_path
            .parent()
            .ok_or_else(|| miette::miette!("can not find parent of {}", manifest_path.display()))?;

        // Load the TOML document
        let manifest = fs::read_to_string(manifest_path)
            .into_diagnostic()
            .and_then(|content| Manifest::from_str(root, content))
            .wrap_err_with(|| {
                format!(
                    "failed to parse {} from {}",
                    consts::PROJECT_MANIFEST,
                    root.display()
                )
            });

        Ok(Self {
            root: root.to_owned(),
            package_db: Default::default(),
            client: Default::default(),
            authenticated_client: reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
                .with_arc(Arc::new(AuthenticationMiddleware::default()))
                .build(),
            manifest: manifest?,
        })
    }

    /// Loads a project manifest file or discovers it in the current directory or any of the parent
    pub fn load_or_else_discover(manifest_path: Option<&Path>) -> miette::Result<Self> {
        let project = match manifest_path {
            Some(path) => Project::load(path)?,
            None => Project::discover()?,
        };
        Ok(project)
    }

    /// Returns the name of the project
    pub fn name(&self) -> &str {
        &self.manifest.parsed.project.name
    }

    /// Returns the version of the project
    pub fn version(&self) -> &Option<Version> {
        &self.manifest.parsed.project.version
    }

    /// Returns the description of the project
    pub fn description(&self) -> &Option<String> {
        &self.manifest.parsed.project.description
    }

    /// Returns the root directory of the project
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the pixi directory
    pub fn pixi_dir(&self) -> PathBuf {
        self.root.join(consts::PIXI_DIR)
    }

    /// Returns the environment directory
    pub fn environments_dir(&self) -> PathBuf {
        self.pixi_dir().join(consts::ENVIRONMENTS_DIR)
    }

    /// Returns the solve group directory
    pub fn solve_group_environments_dir(&self) -> PathBuf {
        self.pixi_dir().join(consts::SOLVE_GROUP_ENVIRONMENTS_DIR)
    }

    /// Returns the path to the manifest file.
    pub fn manifest_path(&self) -> PathBuf {
        self.manifest.path.clone()
    }

    /// Returns the path to the lock file of the project
    pub fn lock_file_path(&self) -> PathBuf {
        self.root.join(consts::PROJECT_LOCK_FILE)
    }

    /// Save back changes
    pub fn save(&mut self) -> miette::Result<()> {
        self.manifest.save()
    }

    /// Returns the default environment of the project.
    pub fn default_environment(&self) -> Environment<'_> {
        Environment {
            project: self,
            environment: self.manifest.default_environment(),
        }
    }

    /// Returns the environment with the given name or `None` if no such environment exists.
    pub fn environment<Q: ?Sized>(&self, name: &Q) -> Option<Environment<'_>>
    where
        Q: Hash + Equivalent<EnvironmentName>,
    {
        Some(Environment {
            project: self,
            environment: self.manifest.environment(name)?,
        })
    }

    /// Returns the environments in this project.
    pub fn environments(&self) -> Vec<Environment> {
        self.manifest
            .parsed
            .environments
            .iter()
            .map(|env| Environment {
                project: self,
                environment: env,
            })
            .collect()
    }

    /// Returns all the solve groups in the project.
    pub fn solve_groups(&self) -> Vec<SolveGroup> {
        self.manifest
            .parsed
            .solve_groups
            .iter()
            .map(|group| SolveGroup {
                project: self,
                solve_group: group,
            })
            .collect()
    }

    /// Returns the solve group with the given name or `None` if no such group exists.
    pub fn solve_group(&self, name: &str) -> Option<SolveGroup> {
        self.manifest
            .parsed
            .solve_groups
            .find(name)
            .map(|group| SolveGroup {
                project: self,
                solve_group: group,
            })
    }

    /// Returns the channels used by this project.
    ///
    /// TODO: Remove this function and use the channels from the default environment instead.
    pub fn channels(&self) -> IndexSet<&Channel> {
        self.default_environment().channels()
    }

    /// Returns the platforms this project targets
    ///
    /// TODO: Remove this function and use the platforms from the default environment instead.
    pub fn platforms(&self) -> HashSet<Platform> {
        self.default_environment().platforms()
    }

    /// Get the tasks of this project
    ///
    /// TODO: Remove this function and use the tasks from the default environment instead.
    pub fn tasks(&self, platform: Option<Platform>) -> HashMap<&TaskName, &Task> {
        self.default_environment()
            .tasks(platform, true)
            .unwrap_or_default()
    }

    /// Get the task with the specified `name` or `None` if no such task exists. If `platform` is
    /// specified then the task will first be looked up in the target specific tasks for the given
    /// platform.
    ///
    /// TODO: Remove this function and use the `task` function from the default environment instead.
    pub fn task_opt(&self, name: &TaskName, platform: Option<Platform>) -> Option<&Task> {
        self.default_environment().task(name, platform).ok()
    }

    /// TODO: Remove this method and use the one from Environment instead.
    pub fn virtual_packages(&self, platform: Platform) -> Vec<GenericVirtualPackage> {
        self.default_environment().virtual_packages(platform)
    }

    /// Get the system requirements defined under the `system-requirements` section of the project manifest.
    /// They will act as the description of a reference machine which is minimally needed for this package to be run.
    ///
    /// TODO: Remove this function and use the `system_requirements` function from the default environment instead.
    pub fn system_requirements(&self) -> SystemRequirements {
        self.default_environment().system_requirements()
    }

    /// Returns the dependencies of the project.
    ///
    /// TODO: Remove this function and use the `dependencies` function from the default environment instead.
    pub fn dependencies(&self, kind: Option<SpecType>, platform: Option<Platform>) -> Dependencies {
        self.default_environment().dependencies(kind, platform)
    }

    /// Returns the PyPi dependencies of the project
    ///
    /// TODO: Remove this function and use the `dependencies` function from the default environment instead.
    pub fn pypi_dependencies(
        &self,
        platform: Option<Platform>,
    ) -> IndexMap<rip::types::PackageName, Vec<PyPiRequirement>> {
        self.default_environment().pypi_dependencies(platform)
    }

    /// Returns the all specified activation scripts that are used in the current platform.
    ///
    /// TODO: Remove this function and use the `activation_scripts function from the default environment instead.
    pub fn activation_scripts(&self, platform: Option<Platform>) -> Vec<String> {
        self.default_environment().activation_scripts(platform)
    }

    /// Returns true if the project contains any reference pypi dependencies. Even if just
    /// `[pypi-dependencies]` is specified without any requirements this will return true.
    pub fn has_pypi_dependencies(&self) -> bool {
        self.manifest.has_pypi_dependencies()
    }

    /// Returns the Python index URLs to use for this project.
    pub fn pypi_index_url(&self) -> Url {
        normalize_index_url(Url::parse("https://pypi.org/simple/").unwrap())
    }

    /// Returns the package database used for caching python metadata, wheels and more. See the
    /// documentation of [`rip::index::PackageDb`] for more information.
    pub fn pypi_package_db(&self) -> miette::Result<Arc<PackageDb>> {
        Ok(self
            .package_db
            .get_or_try_init(|| {
                PackageDb::new(
                    PackageSources::from(self.pypi_index_url()),
                    self.authenticated_client().clone(),
                    &config::get_cache_dir()?.join("pypi/"),
                )
                .map(Arc::new)
            })?
            .clone())
    }

    /// Returns the reqwest client used for http networking
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Create an authenticated reqwest client for this project
    /// use authentication from `rattler_networking`
    pub fn authenticated_client(&self) -> &ClientWithMiddleware {
        &self.authenticated_client
    }
}

/// Iterates over the current directory and all its parent directories and returns the first
/// directory path that contains the [`consts::PROJECT_MANIFEST`].
pub fn find_project_root() -> Option<PathBuf> {
    let current_dir = env::current_dir().ok()?;
    std::iter::successors(Some(current_dir.as_path()), |prev| prev.parent())
        .find(|dir| dir.join(consts::PROJECT_MANIFEST).is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::manifest::FeatureName;
    use insta::{assert_debug_snapshot, assert_display_snapshot};
    use itertools::Itertools;
    use rattler_virtual_packages::{LibC, VirtualPackage};
    use std::str::FromStr;

    const PROJECT_BOILERPLATE: &str = r#"
        [project]
        name = "foo"
        version = "0.1.0"
        channels = []
        platforms = ["linux-64", "win-64"]
        "#;

    #[test]
    fn test_system_requirements_edge_cases() {
        let file_contents = [
            r#"
        [system-requirements]
        libc = { version = "2.12" }
        "#,
            r#"
        [system-requirements]
        libc = "2.12"
        "#,
            r#"
        [system-requirements.libc]
        version = "2.12"
        "#,
            r#"
        [system-requirements.libc]
        version = "2.12"
        family = "glibc"
        "#,
        ];

        for file_content in file_contents {
            let file_content = format!("{PROJECT_BOILERPLATE}\n{file_content}");

            let manifest = Manifest::from_str(Path::new(""), &file_content).unwrap();
            let project = Project::from_manifest(manifest);
            let expected_result = vec![VirtualPackage::LibC(LibC {
                family: "glibc".to_string(),
                version: Version::from_str("2.12").unwrap(),
            })];

            let virtual_packages = project.system_requirements().virtual_packages();

            assert_eq!(virtual_packages, expected_result);
        }
    }

    fn format_dependencies(deps: Dependencies) -> String {
        deps.iter_specs()
            .map(|(name, spec)| format!("{} = \"{}\"", name.as_source(), spec))
            .join("\n")
    }

    #[test]
    fn test_dependency_sets() {
        let file_contents = r#"
        [dependencies]
        foo = "1.0"

        [host-dependencies]
        libc = "2.12"

        [build-dependencies]
        bar = "1.0"
        "#;

        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();
        let project = Project::from_manifest(manifest);

        assert_display_snapshot!(format_dependencies(
            project.dependencies(None, Some(Platform::Linux64))
        ));
    }

    #[test]
    fn test_dependency_target_sets() {
        let file_contents = r#"
        [dependencies]
        foo = "1.0"

        [host-dependencies]
        libc = "2.12"

        [build-dependencies]
        bar = "1.0"

        [target.linux-64.build-dependencies]
        baz = "1.0"

        [target.linux-64.host-dependencies]
        banksy = "1.0"

        [target.linux-64.dependencies]
        wolflib = "1.0"
        "#;
        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();
        let project = Project::from_manifest(manifest);

        assert_display_snapshot!(format_dependencies(
            project.dependencies(None, Some(Platform::Linux64))
        ));
    }

    #[test]
    fn test_activation_scripts() {
        fn fmt_activation_scripts(scripts: Vec<String>) -> String {
            scripts.iter().join("\n")
        }

        // Using known files in the project so the test succeed including the file check.
        let file_contents = r#"
            [target.linux-64.activation]
            scripts = ["Cargo.toml"]

            [target.win-64.activation]
            scripts = ["Cargo.lock"]

            [activation]
            scripts = ["pixi.toml", "pixi.lock"]
            "#;
        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();
        let project = Project::from_manifest(manifest);

        assert_display_snapshot!(format!(
            "= Linux64\n{}\n\n= Win64\n{}\n\n= OsxArm64\n{}",
            fmt_activation_scripts(project.activation_scripts(Some(Platform::Linux64))),
            fmt_activation_scripts(project.activation_scripts(Some(Platform::Win64))),
            fmt_activation_scripts(project.activation_scripts(Some(Platform::OsxArm64)))
        ));
    }

    #[test]
    fn test_target_specific_tasks() {
        // Using known files in the project so the test succeed including the file check.
        let file_contents = r#"
            [tasks]
            test = "test multi"

            [target.win-64.tasks]
            test = "test win"

            [target.linux-64.tasks]
            test = "test linux"
            "#;
        let manifest = Manifest::from_str(
            Path::new(""),
            format!("{PROJECT_BOILERPLATE}\n{file_contents}").as_str(),
        )
        .unwrap();

        let project = Project::from_manifest(manifest);

        assert_debug_snapshot!(project
            .manifest
            .tasks(Some(Platform::Osx64), &FeatureName::Default)
            .unwrap());
        assert_debug_snapshot!(project
            .manifest
            .tasks(Some(Platform::Win64), &FeatureName::Default)
            .unwrap());
        assert_debug_snapshot!(project
            .manifest
            .tasks(Some(Platform::Linux64), &FeatureName::Default)
            .unwrap());
    }
}
