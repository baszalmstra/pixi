use std::sync::OnceLock;

use miette::{Context, IntoDiagnostic};
use pixi_default_versions::{default_glibc_version, default_mac_os_version};
use pixi_manifest::{LibCSystemRequirement, SystemRequirements};
use rattler_conda_types::MatchSpec;
use rattler_conda_types::{Arch, PackageRecord, Platform};
use regex::Regex;
use uv_platform_tags::Os;
use uv_platform_tags::Tags;

/// Returns true if the specified record refers to a version/variant of python.
pub fn is_python_record(record: impl AsRef<PackageRecord>) -> bool {
    package_name_is_python(&record.as_ref().name)
}

/// Returns true if the specified name refers to a version/variant of python.
/// TODO: Add support for more variants.
pub fn package_name_is_python(record: &rattler_conda_types::PackageName) -> bool {
    record.as_normalized() == "python"
}

/// Get the python version and implementation name for the specified platform.
pub fn get_pypi_tags(
    platform: Platform,
    system_requirements: &SystemRequirements,
    python_record: &PackageRecord,
) -> miette::Result<Tags> {
    let platform = get_platform_tags(platform, system_requirements)?;
    let python_version = get_python_version(python_record)?;
    let implementation_name = get_implementation_name(python_record)?;
    let gil_disabled = gil_disabled(python_record)?;
    create_tags(platform, python_version, implementation_name, gil_disabled)
}

/// Create a uv platform tag for the specified platform
fn get_platform_tags(
    platform: Platform,
    system_requirements: &SystemRequirements,
) -> miette::Result<uv_platform_tags::Platform> {
    if platform.is_linux() {
        get_linux_platform_tags(platform, system_requirements)
    } else if platform.is_windows() {
        get_windows_platform_tags(platform)
    } else if platform.is_osx() {
        get_macos_platform_tags(platform, system_requirements)
    } else {
        miette::bail!("unsupported platform for pypi packages {platform}")
    }
}

/// Get linux specific platform tags
fn get_linux_platform_tags(
    platform: Platform,
    system_requirements: &SystemRequirements,
) -> miette::Result<uv_platform_tags::Platform> {
    let arch = get_arch_tags(platform)?;

    // Find the glibc version
    match system_requirements
        .libc
        .as_ref()
        .map(LibCSystemRequirement::family_and_version)
    {
        None => {
            let (major, minor) = default_glibc_version()
                .as_major_minor()
                .expect("expected default glibc version to be a major.minor version");
            Ok(uv_platform_tags::Platform::new(
                Os::Manylinux {
                    major: major as _,
                    minor: minor as _,
                },
                arch,
            ))
        }
        Some(("glibc", version)) => {
            let Some((major, minor)) = version.as_major_minor() else {
                miette::bail!(
                    "expected glibc version to be a major.minor version, but got '{version}'"
                )
            };
            Ok(uv_platform_tags::Platform::new(
                Os::Manylinux {
                    major: major as _,
                    minor: minor as _,
                },
                arch,
            ))
        }
        Some((family, _)) => {
            miette::bail!("unsupported libc family for pypi packages '{family}'");
        }
    }
}

/// Get windows specific platform tags
fn get_windows_platform_tags(platform: Platform) -> miette::Result<uv_platform_tags::Platform> {
    let arch = get_arch_tags(platform)?;
    Ok(uv_platform_tags::Platform::new(Os::Windows, arch))
}

/// Get macos specific platform tags
fn get_macos_platform_tags(
    platform: Platform,
    system_requirements: &SystemRequirements,
) -> miette::Result<uv_platform_tags::Platform> {
    let osx_version = system_requirements
        .macos
        .clone()
        .unwrap_or_else(|| default_mac_os_version(platform));
    let Some((major, minor)) = osx_version.as_major_minor() else {
        miette::bail!("expected macos version to be a major.minor version, but got '{osx_version}'")
    };

    let arch = get_arch_tags(platform)?;

    Ok(uv_platform_tags::Platform::new(
        Os::Macos {
            major: major as _,
            minor: minor as _,
        },
        arch,
    ))
}

/// Get the arch tag for the specified platform
fn get_arch_tags(platform: Platform) -> miette::Result<uv_platform_tags::Arch> {
    match platform.arch() {
        None => unreachable!("every platform we support has an arch"),
        Some(Arch::X86) => Ok(uv_platform_tags::Arch::X86),
        Some(Arch::X86_64) => Ok(uv_platform_tags::Arch::X86_64),
        Some(Arch::Aarch64 | Arch::Arm64) => Ok(uv_platform_tags::Arch::Aarch64),
        Some(Arch::ArmV7l) => Ok(uv_platform_tags::Arch::Armv7L),
        Some(Arch::Ppc64le) => Ok(uv_platform_tags::Arch::Powerpc64Le),
        Some(Arch::Ppc64) => Ok(uv_platform_tags::Arch::Powerpc64),
        Some(Arch::S390X) => Ok(uv_platform_tags::Arch::S390X),
        Some(unsupported_arch) => {
            miette::bail!("unsupported arch for pypi packages '{unsupported_arch}'")
        }
    }
}

fn get_python_version(python_record: &PackageRecord) -> miette::Result<(u8, u8)> {
    let Some(python_version) = python_record.version.as_major_minor() else {
        miette::bail!(
            "expected python version to be a major.minor version, but got '{}'",
            &python_record.version
        );
    };
    Ok((python_version.0 as u8, python_version.1 as u8))
}

fn get_implementation_name(python_record: &PackageRecord) -> miette::Result<&'static str> {
    match python_record.name.as_normalized() {
        "python" => Ok("cpython"),
        "pypy" => Ok("pypy"),
        _ => {
            miette::bail!(
                "unsupported python implementation '{}'",
                python_record.name.as_source()
            );
        }
    }
}

/// Return whether the specified record has gil disabled (by being a free-threaded python interpreter)
fn gil_disabled(python_record: &PackageRecord) -> miette::Result<bool> {
    // In order to detect if the python interpreter is free-threaded, we look at the depends
    // field of the record. If the record has a dependency on `python_abi`, then
    // look at the build string to detect cpXXXt (free-threaded python interpreter).
    static REGEX: OnceLock<Regex> = OnceLock::new();

    let regex = REGEX.get_or_init(|| {
        Regex::new(r"cp\d{3}t").expect("regex for free-threaded python interpreter should compile")
    });

    let deps = python_record
        .depends
        .iter()
        .map(|dep| MatchSpec::from_str(dep, rattler_conda_types::ParseStrictness::Lenient))
        .collect::<Result<Vec<MatchSpec>, _>>()
        .into_diagnostic()?;

    Ok(deps.iter().any(|spec| {
        spec.name
            .as_ref()
            .is_some_and(|name| name.as_source() == "python_abi")
            && spec.build.as_ref().is_some_and(|build| {
                let raw_str = format!("{}", build);
                regex.is_match(&raw_str)
            })
    }))
}

fn create_tags(
    platform: uv_platform_tags::Platform,
    python_version: (u8, u8),
    implementation_name: &str,
    gil_disabled: bool,
) -> miette::Result<Tags> {
    // Build the wheel tags based on the interpreter, the target platform, and the python version.
    let tags = Tags::from_env(
        &platform,
        python_version,
        implementation_name,
        // TODO: This might not be entirely correct..
        python_version,
        true,
        gil_disabled,
    )
    .into_diagnostic()
    .context("failed to determine the python wheel tags for the target platform")?;

    Ok(tags)
}
