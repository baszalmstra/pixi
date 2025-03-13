use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::{
    cli::cli_config::{LockFileUpdateConfig, WorkspaceConfig},
    lock_file::UpdateLockFileOptions,
    WorkspaceLocator,
};
use clap::Parser;
use miette::{Context, IntoDiagnostic};
use pixi_config::ConfigCli;
use rattler_conda_types::{
    ExplicitEnvironmentEntry, ExplicitEnvironmentSpec, PackageRecord, Platform, RepoDataRecord,
};
use rattler_lock::{CondaPackageData, Environment, LockedPackageRef};

#[derive(Debug, Parser)]
#[clap(arg_required_else_help = false)]
pub struct Args {
    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,

    /// Output directory for rendered explicit environment spec files
    pub output_dir: PathBuf,

    /// The environments to render. Can be repeated for multiple environments.
    #[arg(short, long)]
    pub environment: Option<Vec<String>>,

    /// The platform to render. Can be repeated for multiple platforms.
    /// Defaults to all platforms available for selected environments.
    #[arg(short, long)]
    pub platform: Option<Vec<Platform>>,

    /// PyPI dependencies are not supported in the conda explicit spec file.
    #[arg(long, default_value = "false")]
    pub ignore_pypi_errors: bool,

    /// Source dependencies are not supported in the conda explicit spec file.
    #[arg(long, default_value = "false")]
    pub ignore_source_errors: bool,

    #[clap(flatten)]
    pub lock_file_update_config: LockFileUpdateConfig,

    #[clap(flatten)]
    config: ConfigCli,
}

fn build_explicit_spec<'a>(
    platform: &Platform,
    conda_packages: impl IntoIterator<Item = &'a RepoDataRecord>,
) -> miette::Result<ExplicitEnvironmentSpec> {
    let mut packages = Vec::new();

    for cp in conda_packages {
        let prec = &cp.package_record;
        let hash = prec.md5.ok_or(miette::miette!(
            "Package {} does not contain an md5 hash",
            prec.name.as_normalized()
        ))?;

        let mut url = cp.url.clone();
        url.set_fragment(Some(&format!("{:x}", hash)));

        packages.push(ExplicitEnvironmentEntry {
            url: url.to_owned(),
        });
    }

    Ok(ExplicitEnvironmentSpec {
        platform: Some(*platform),
        packages,
    })
}

fn render_explicit_spec(
    target: impl AsRef<Path>,
    exp_env_spec: &ExplicitEnvironmentSpec,
) -> miette::Result<()> {
    if exp_env_spec.packages.is_empty() {
        return Ok(());
    }

    let target = target.as_ref();

    let mut environment = String::new();
    environment.push_str("# Generated by `pixi workspace export`\n");
    environment.push_str(exp_env_spec.to_spec_string().as_str());

    fs_err::write(target, environment)
        .into_diagnostic()
        .with_context(|| format!("failed to write environment file: {}", target.display()))?;

    Ok(())
}

fn render_env_platform(
    output_dir: &Path,
    env_name: &str,
    env: &Environment,
    platform: &Platform,
    ignore_pypi_errors: bool,
) -> miette::Result<()> {
    let packages = env.packages(*platform).ok_or(miette::miette!(
        "platform '{platform}' not found for env {}",
        env_name,
    ))?;

    let mut conda_packages_from_lockfile: Vec<_> = Vec::new();

    for package in packages {
        match package {
            LockedPackageRef::Conda(CondaPackageData::Binary(p)) => {
                conda_packages_from_lockfile.push(p.clone())
            }
            LockedPackageRef::Conda(CondaPackageData::Source(_)) => {
                miette::bail!(
                        "Conda source packages are not supported in a conda explicit spec. \
                        Specify `--ignore-source-errors` to ignore this error and create \
                        a spec file containing only the binary conda dependencies from the lockfile."
                    );
            }
            LockedPackageRef::Pypi(pypi, _) => {
                if ignore_pypi_errors {
                    tracing::warn!(
                        "ignoring PyPI package {} since PyPI packages are not supported",
                        pypi.name
                    );
                } else {
                    miette::bail!(
                        "PyPI packages are not supported in a conda explicit spec. \
                        Specify `--ignore-pypi-errors` to ignore this error and create \
                        a spec file containing only the conda dependencies from the lockfile."
                    );
                }
            }
        }
    }

    // Topologically sort packages
    let repodata = conda_packages_from_lockfile
        .iter()
        .map(|p| RepoDataRecord::try_from(p.clone()))
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()
        .with_context(|| "Failed to convert conda packages to RepoDataRecords")?;

    let repodata = PackageRecord::sort_topologically(repodata);

    let ees = build_explicit_spec(platform, &repodata)?;

    tracing::info!("Creating conda explicit spec for env: {env_name} platform: {platform}");
    let target = output_dir
        .join(format!("{}_{}_conda_spec.txt", env_name, platform))
        .into_os_string();

    render_explicit_spec(target, &ees)?;

    Ok(())
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let workspace = WorkspaceLocator::for_cli()
        .with_search_start(args.workspace_config.workspace_locator_start())
        .locate()?
        .with_cli_config(args.config.clone());

    let lockfile = workspace
        .update_lock_file(UpdateLockFileOptions {
            lock_file_usage: args.lock_file_update_config.lock_file_usage(),
            no_install: args.lock_file_update_config.no_lockfile_update,
            max_concurrent_solves: workspace.config().max_concurrent_solves(),
        })
        .await?
        .lock_file;

    let mut environments = Vec::new();
    if let Some(env_names) = args.environment {
        for env_name in &env_names {
            environments.push((
                env_name.to_string(),
                lockfile
                    .environment(env_name)
                    .ok_or(miette::miette!("unknown environment {}", env_name))?,
            ));
        }
    } else {
        for (env_name, env) in lockfile.environments() {
            environments.push((env_name.to_string(), env));
        }
    };

    let mut env_platform = Vec::new();

    for (env_name, env) in environments {
        let available_platforms: HashSet<Platform> = HashSet::from_iter(env.platforms());

        if let Some(ref platforms) = args.platform {
            for plat in platforms {
                if available_platforms.contains(plat) {
                    env_platform.push((env_name.clone(), env, *plat));
                } else {
                    tracing::warn!(
                        "Platform {} not available for environment {}. Skipping...",
                        plat,
                        env_name,
                    );
                }
            }
        } else {
            for plat in available_platforms {
                env_platform.push((env_name.clone(), env, plat));
            }
        }
    }

    fs_err::create_dir_all(&args.output_dir).ok();

    for (env_name, env, plat) in env_platform {
        render_env_platform(
            &args.output_dir,
            &env_name,
            &env,
            &plat,
            args.ignore_pypi_errors,
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rattler_lock::LockFile;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_render_conda_explicit_spec() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/mock-projects/test-project-export/pixi.lock");
        let lockfile = LockFile::from_path(&path).unwrap();

        let output_dir = tempdir().unwrap();

        for (env_name, env) in lockfile.environments() {
            for platform in env.platforms() {
                // example contains pypi dependencies so should fail if `ignore_pypi_errors` is
                // false.
                assert!(
                    render_env_platform(output_dir.path(), env_name, &env, &platform, false)
                        .is_err()
                );
                render_env_platform(output_dir.path(), env_name, &env, &platform, true).unwrap();

                let file_path = output_dir
                    .path()
                    .join(format!("{}_{}_conda_spec.txt", env_name, platform));
                insta::assert_snapshot!(
                    format!("test_render_conda_explicit_spec_{}_{}", env_name, platform),
                    fs_err::read_to_string(file_path).unwrap()
                );
            }
        }
    }
}
