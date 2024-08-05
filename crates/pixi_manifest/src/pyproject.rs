use std::{collections::HashMap, fs, path::PathBuf, str::FromStr};

use miette::{IntoDiagnostic, Report};
use pep440_rs::VersionSpecifiers;
use pixi_spec::Spec;
use pyproject_toml::{self, BuildSystem, Project};
use rattler_conda_types::{PackageName, ParseStrictness::Lenient, VersionSpec};
use serde::Deserialize;

use super::{
    error::{RequirementConversionError, TomlError},
    Feature, ParsedManifest, SpecType,
};
use crate::FeatureName;

#[derive(Deserialize, Debug, Clone)]
pub struct PyProjectManifest {
    #[serde(flatten)]
    inner: pyproject_toml::PyProjectToml,
    tool: Tool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Tool {
    pub pixi: Option<ParsedManifest>,
    #[allow(dead_code)]
    pub poetry: Option<ToolPoetry>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ToolPoetry {
    #[allow(dead_code)]
    pub name: Option<String>,
}

impl std::ops::Deref for PyProjectManifest {
    type Target = pyproject_toml::PyProjectToml;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl PyProjectManifest {
    /// Parses a toml string into a pyproject manifest.
    pub fn from_toml_str(source: &str) -> Result<Self, TomlError> {
        let manifest: PyProjectManifest =
            toml_edit::de::from_str(source).map_err(TomlError::from)?;

        // Make sure [project] exists in pyproject.toml,
        // This will ensure project.name is defined
        // TODO: do we want to Err if tool.pixi.name is defined?
        if manifest.project.is_none() {
            return Err(TomlError::NoProjectTable);
        }

        Ok(manifest)
    }
}

impl From<PyProjectManifest> for ParsedManifest {
    fn from(item: PyProjectManifest) -> Self {
        // Start by loading the data nested under "tool.pixi" as manifest,
        // and create a reference to the 'pyproject.toml' project table
        let mut manifest = item.tool.pixi.clone().expect("pixi should be present");
        let pyproject = item
            .project
            .as_ref()
            .expect("the [project] table should exist");

        // Overwrite the project name and version with the ones from the [project] table
        // of the pyproject.toml if they are not set.
        if manifest.project.name.is_none() {
            manifest.project.name = Some(pyproject.name.clone());
        }

        if manifest.project.version.is_none() {
            manifest.project.version = pyproject
                .version
                .clone()
                .and_then(|version| version.to_string().parse().ok())
        }

        if manifest.project.description.is_none() {
            manifest.project.description = pyproject.description.clone();
        }

        if manifest.project.authors.is_none() {
            manifest.project.authors = pyproject.authors.as_ref().map(|authors| {
                authors
                    .iter()
                    .filter_map(|contact| match (&contact.name, &contact.email) {
                        (Some(name), Some(email)) => Some(format!("{} <{}>", name, email)),
                        (Some(name), None) => Some(name.clone()),
                        (None, Some(email)) => Some(email.clone()),
                        (None, None) => None,
                    })
                    .collect()
            });
        }

        // TODO:  would be nice to add license, license-file, readme, homepage,
        // repository, documentation, regarding the above, the types are a bit
        // different than we expect, so the conversion is not straightforward we
        // could change these types or we can convert. Let's decide when we make it.
        // etc.

        // Add python as dependency based on the project.requires_python property (if
        // any)
        let python_spec = pyproject.requires_python.clone();

        let target = manifest.default_feature_mut().targets.default_mut();
        let python = PackageName::from_str("python").unwrap();
        // If the target doesn't have any python dependency, we add it from the
        // `requires-python`
        if !target.has_dependency(&python, Some(SpecType::Run), None) {
            target.add_dependency(
                &python,
                &version_or_url_to_spec(&python_spec).unwrap(),
                SpecType::Run,
            );
        } else if let Some(_spec) = python_spec {
            if target.has_dependency(&python, Some(SpecType::Run), None) {
                // TODO: implement some comparison or spec merging logic here
                tracing::info!(
                    "Overriding the requires-python with the one defined in pixi dependencies"
                )
            }
        }

        // Add pyproject dependencies as pypi dependencies
        if let Some(deps) = &pyproject.dependencies {
            for requirement in deps.iter() {
                target.add_pypi_dependency(requirement, None);
            }
        }

        // For each extra group, create a feature of the same name if it does not exist,
        // and add pypi dependencies from project.optional-dependencies,
        // filtering out self-references
        if let Some(extras) = pyproject.optional_dependencies.as_ref() {
            let project_name = pep508_rs::PackageName::new(pyproject.name.clone()).unwrap();
            for (extra, reqs) in extras {
                let feature_name = FeatureName::Named(extra.to_string());
                let target = manifest
                    .features
                    .entry(feature_name.clone())
                    .or_insert_with(move || Feature::new(feature_name))
                    .targets
                    .default_mut();
                for requirement in reqs.iter() {
                    // filter out any self references in groups of extra dependencies
                    if project_name != requirement.name {
                        target.add_pypi_dependency(requirement, None);
                    }
                }
            }
        }

        manifest
    }
}

/// Try to return a NamelessMatchSpec from a pep508_rs::VersionOrUrl
/// This will only work if it is not URL and the VersionSpecifier can
/// successfully be interpreted as a NamelessMatchSpec.version
fn version_or_url_to_spec(
    version: &Option<VersionSpecifiers>,
) -> Result<Spec, RequirementConversionError> {
    match version {
        // TODO: avoid going through string representation for conversion
        Some(v) => {
            let version_string = v.to_string();
            // Double equals works a bit different in conda vs. python
            let version_string = version_string.strip_prefix("==").unwrap_or(&version_string);
            Ok(VersionSpec::from_str(version_string, Lenient)?.into())
        }
        None => Ok(Spec::default()),
    }
}

/// A struct wrapping pyproject_toml::PyProjectToml
///
/// This is used during 'pixi init' to parse a potentially non-pixi
/// 'pyproject.toml'
#[derive(Deserialize, Clone)]
pub struct PyProjectToml {
    /// Build-related data
    pub build_system: Option<BuildSystem>,
    /// Project metadata
    pub project: Option<Project>,
    // Tool section
    pub tool: Option<Tool>,
}

impl PyProjectToml {
    /// Parses a non-pixi pyproject.toml string into a PyProjectToml struct
    /// making sure it contains a 'project' table
    pub fn from_toml_str(source: &str) -> Result<PyProjectToml, Report> {
        match toml_edit::de::from_str::<PyProjectToml>(source).map_err(TomlError::from) {
            Err(e) => e.to_fancy("pyproject.toml", source),
            Ok(pyproject) => {
                // Make sure [project] exists in pyproject.toml,
                // This will ensure project.name is defined
                if pyproject.project.is_none() {
                    TomlError::NoProjectTable.to_fancy("pyproject.toml", source)
                } else {
                    Ok(pyproject)
                }
            }
        }
    }

    /// Parses a non-pixi pyproject.toml string into a PyProjectToml struct
    /// making sure it contains a 'project' table
    pub fn from_path(path: &PathBuf) -> Result<PyProjectToml, Report> {
        let source = fs::read_to_string(path).into_diagnostic()?;
        PyProjectToml::from_toml_str(&source)
    }

    pub fn name(&self) -> String {
        self.project().name.clone()
    }

    pub fn project(&self) -> &Project {
        self.project.as_ref().unwrap()
    }

    /// Builds a list of pixi environments from pyproject groups of extra
    /// dependencies:
    ///  - one environment is created per group of extra, with the same name as
    ///    the group of extra
    ///  - each environment includes the feature of the same name as the group
    ///    of extra
    ///  - it will also include other features inferred from any self references
    ///    to other groups of extras
    pub fn environments_from_extras(&self) -> HashMap<String, Vec<String>> {
        let mut environments = HashMap::new();
        if let Some(extras) = &self.project().optional_dependencies {
            let pname = pep508_rs::PackageName::new(self.name()).unwrap();
            for (extra, reqs) in extras {
                let mut features = vec![extra.to_string()];
                // Add any references to other groups of extra dependencies
                for req in reqs.iter() {
                    if pname == req.name {
                        for extra in &req.extras {
                            features.push(extra.to_string())
                        }
                    }
                }
                // Environments can only contain number, strings and dashes
                environments.insert(extra.replace('_', "-").clone(), features);
            }
        }
        environments
    }

    /// Checks whether a path is a valid `pyproject.toml` for use with pixi by
    /// checking if it contains a `[tool.pixi.project]` item.
    pub fn is_pixi(&self) -> bool {
        self.tool
            .as_ref()
            .is_some_and(|project| project.pixi.is_some())
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, str::FromStr};

    use insta::assert_snapshot;
    use pep440_rs::VersionSpecifiers;
    use rattler_conda_types::{ParseStrictness, VersionSpec};

    use crate::{
        manifest::Manifest, pypi::PyPiPackageName, DependencyOverwriteBehavior, FeatureName,
    };

    const PYPROJECT_FULL: &str = r#"
        [project]
        name = "project"
        version = "0.1.0"
        description = "A project"
        authors = [
            { name = "Author", email = "author@bla.com" }
        ]

        [tool.pixi.project]
        channels = ["stable"]
        platforms = ["linux-64", "win-64", "osx-64", "osx-arm64"]
        license = "MIT"
        license-file = "../../LICENSE"
        readme = "../../README.md"
        homepage = "https://project.com"
        repository = "https://github.com/author/project"
        documentation = "https://docs.project.com"

        [tool.pixi.dependencies]
        test = "bla"
        test1 = "bli"
        pytorch-cpu = { version = "~=1.1", channel = "pytorch" }
        package1 = { version = ">=1.2.3", build="py34_0" }


        [tool.pixi.pypi-dependencies]
        testpypi = "*"
        testpypi1 = "*"
        requests = {version = ">= 2.8.1, ==2.8.*", extras=["security", "tests"]} # Using the map allows the user to add `extras`

        [tool.pixi.host-dependencies]
        test = "bla"
        test1 = "bli"
        pytorch-cpu = { version = "~=1.1", channel = "pytorch" }
        package1 = { version = ">=1.2.3", build="py34_0" }

        [tool.pixi.build-dependencies]
        test = "*"
        test1 = "*"
        pytorch-cpu = { version = "~=1.1", channel = "pytorch" }
        package1 = { version = ">=1.2.3", build="py34_0" }

        [tool.pixi.tasks]
        build = "conda build ."
        test = { cmd = "pytest", cwd = "tests", depends-on = ["build"] }
        test2 = { cmd = "pytest", cwd = "tests"}
        test3 = { cmd = "pytest", depends-on = ["test2"] }
        test5 = { cmd = "pytest" }
        test6 = { depends-on = ["test5"] }

        [tool.pixi.system-requirements]
        linux = "5.10"
        libc = { family="glibc", version="2.17" }
        cuda = "10.1"

        [tool.pixi.feature.test.dependencies]
        test = "bla"

        [tool.pixi.feature.test2.dependencies]
        test = "bla"

        [tool.pixi.environments]
        test = {features = ["test"], solve-group = "test"}
        prod = {features = ["test2"], solve-group = "test"}

        [tool.pixi.activation]
        scripts = ["activate.sh", "deactivate.sh"]

        [tool.pixi.target.win-64.activation]
        scripts = ["env_setup.bat"]

        [tool.pixi.target.linux-64.dependencies]
        test = "bla"
        test1 = "bli"
        pytorch-cpu = { version = "~=1.1", channel = "pytorch" }
        package1 = { version = ">=1.2.3", build="py34_0" }


        [tool.pixi.target.osx-arm64.pypi-dependencies]
        testpypi = "*"
        testpypi1 = "*"
        requests = {version = ">= 2.8.1, ==2.8.*", extras=["security", "tests"]} # Using the map allows the user to add `extras`

        [tool.pixi.target.osx-64.host-dependencies]
        test = "bla"
        test1 = "bli"
        pytorch-cpu = { version = "~=1.1", channel = "pytorch" }
        package1 = { version = ">=1.2.3", build="py34_0" }

        [tool.pixi.target.linux-64.build-dependencies]
        test = "bla"
        test1 = "bli"
        pytorch-cpu = { version = "~=1.1", channel = "pytorch" }
        package1 = { version = ">=1.2.3", build="py34_0" }

        [tool.pixi.target.linux-64.tasks]
        build = "conda build ."
        test = { cmd = "pytest", cwd = "tests", depends-on = ["build"] }
        test2 = { cmd = "pytest", cwd = "tests"}
        test3 = { cmd = "pytest", depends-on = ["test2"] }
        test5 = { cmd = "pytest" }
        test6 = { depends-on = ["test5"] }

        [tool.pixi.feature.test.target.linux-64.dependencies]
        test = "bla"

        [tool.pixi.feature.cuda]
        activation = {scripts = ["cuda_activation.sh"]}
        channels = ["nvidia"] # Results in:  ["nvidia", "conda-forge"] when the default is `conda-forge`
        dependencies = {cuda = "x.y.z", cudnn = "12.0"}
        pypi-dependencies = {torch = "==1.9.0"}
        platforms = ["linux-64", "osx-arm64"]
        system-requirements = {cuda = "12"}
        tasks = { warmup = "python warmup.py" }
        target.osx-arm64 = {dependencies = {mlx = "x.y.z"}}

        [tool.pixi.feature.cuda2.activation]
        scripts = ["cuda_activation.sh"]

        [tool.pixi.feature.cuda2.dependencies]
        cuda = "x.y.z"
        cudnn = "12.0"

        [tool.pixi.feature.cuda2.pypi-dependencies]
        torch = "==1.9.0"

        [tool.pixi.feature.cuda2.system-requirements]
        cuda = "12"

        [tool.pixi.feature.cuda2.tasks]
        warmup = "python warmup.py"

        [tool.pixi.feature.cuda2.target.osx-arm64.dependencies]
        mlx = "x.y.z"

        # Channels and Platforms are not available as separate tables as they are implemented as lists
        [tool.pixi.feature.cuda2]
        channels = ["nvidia"]
        platforms = ["linux-64", "osx-arm64"]
        "#;

    const PYPROJECT_BOILERPLATE: &str = r#"
        [project]
        name = "flask-hello-world-pyproject"
        version = "0.1.0"
        description = "Example how to get started with flask in a pixi environment."
        license = "MIT OR Apache-2.0"
        homepage = "https://github.com/prefix/pixi"
        readme = "README.md"
        requires-python = ">=3.11"
        dependencies = ["flask==2.*"]

        [tool.pixi.project]
        channels = ["conda-forge"]
        platforms = ["linux-64"]

        [tool.pixi.tasks]
        start = "python -m flask run --port=5050"
        "#;

    #[test]
    fn test_build_manifest() {
        let _manifest = Manifest::from_str(Path::new("pyproject.toml"), PYPROJECT_FULL).unwrap();
    }

    #[test]
    fn test_add_pypi_dependency() {
        let mut manifest =
            Manifest::from_str(Path::new("pyproject.toml"), PYPROJECT_BOILERPLATE).unwrap();

        // Add numpy to pyproject
        let requirement = pep508_rs::Requirement::from_str("numpy>=3.12").unwrap();
        manifest
            .add_pypi_dependency(
                &requirement,
                &[],
                &FeatureName::Default,
                None,
                DependencyOverwriteBehavior::Overwrite,
            )
            .unwrap();

        assert!(manifest
            .default_feature_mut()
            .targets
            .for_opt_target(None)
            .unwrap()
            .pypi_dependencies
            .as_ref()
            .unwrap()
            .get(&PyPiPackageName::from_normalized(requirement.name.clone()))
            .is_some());

        // Add numpy to feature in pyproject
        let requirement = pep508_rs::Requirement::from_str("pytest>=3.12").unwrap();
        manifest
            .add_pypi_dependency(
                &requirement,
                &[],
                &FeatureName::Named("test".to_string()),
                None,
                DependencyOverwriteBehavior::Overwrite,
            )
            .unwrap();
        assert!(manifest
            .feature(&FeatureName::Named("test".to_string()))
            .unwrap()
            .targets
            .for_opt_target(None)
            .unwrap()
            .pypi_dependencies
            .as_ref()
            .unwrap()
            .get(&PyPiPackageName::from_normalized(requirement.name.clone()))
            .is_some());

        assert_snapshot!(manifest.document.to_string());
    }

    #[test]
    fn test_remove_pypi_dependency() {
        let mut manifest =
            Manifest::from_str(Path::new("pyproject.toml"), PYPROJECT_BOILERPLATE).unwrap();

        // Remove flask from pyproject
        let name = PyPiPackageName::from_str("flask").unwrap();
        manifest
            .remove_pypi_dependency(&name, &[], &FeatureName::Default)
            .unwrap();

        assert!(manifest
            .default_feature_mut()
            .targets
            .for_opt_target(None)
            .unwrap()
            .pypi_dependencies
            .as_ref()
            .unwrap()
            .get(&name)
            .is_none());

        assert_snapshot!(manifest.document.to_string());
    }

    #[test]
    fn test_version_url_to_matchspec() {
        fn cmp(v1: &str, v2: &str) {
            let v = VersionSpecifiers::from_str(v1).unwrap();
            let matchspec = super::version_or_url_to_spec(&Some(v)).unwrap();
            let version_spec = matchspec.as_version().unwrap();
            let vspec = VersionSpec::from_str(v2, ParseStrictness::Strict).unwrap();
            assert_eq!(version_spec, &vspec);
        }

        // Check that we remove leading `==` for the conda version spec
        cmp("==3.12", "3.12");
        cmp("==3.12.*", "3.12.*");
        // rest should work just fine
        cmp(">=3.12", ">=3.12");
        cmp(">=3.10,<3.12", ">=3.10,<3.12");
        cmp("~=3.12", "~=3.12");
    }
}
