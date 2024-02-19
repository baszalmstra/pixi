use crate::project::manifest::{LibCSystemRequirement, SystemRequirements};
use crate::project::virtual_packages::{default_glibc_version, default_mac_os_version};
use itertools::Itertools;
use platform_host::Os;
use platform_tags::Tags;
use rattler_conda_types::{Arch, PackageRecord, Platform, Version};
use rip::python_env::{WheelTag, WheelTags};
use std::str::FromStr;

/// Returns true if the specified record refers to a version/variant of python.
pub fn is_python_record(record: impl AsRef<PackageRecord>) -> bool {
    package_name_is_python(&record.as_ref().name)
}

/// Returns true if the specified name refers to a version/variant of python.
/// TODO: Add support for more variants.
pub fn package_name_is_python(record: &rattler_conda_types::PackageName) -> bool {
    record.as_normalized() == "python"
}

pub fn get_pypi_tags(
    platform: Platform,
    system_requirements: SystemRequirements,
    python_record: &PackageRecord,
) -> miette::Result<Tags> {
    let platform = if platform.is_linux() {
        let arch = match platform.arch() {
            None => unreachable!("every platform we support has an arch"),
            Some(Arch::X86) => platform_host::Arch::X86,
            Some(Arch::X86_64) => platform_host::Arch::X86_64,
            Some(Arch::Aarch64) => platform_host::Arch::Aarch64,
            Some(Arch::ArmV7l) => platform_host::Arch::Armv7L,
            Some(Arch::Ppc64le) => platform_host::Arch::Powerpc64Le,
            Some(Arch::Ppc64) => platform_host::Arch::Powerpc64,
            Some(Arch::S390X) => platform_host::Arch::S390X,
            Some(unsupported_arch) => {
                miette::miette!("unsupported arch for pypi packages '{unsupported_arch}'")
            }
        };

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
                platform_host::Platform::new(
                    Os::Manylinux {
                        major: major as _,
                        minor: minor as _,
                    },
                    arch,
                )
            }
            Some(("glibc", version)) => {
                let Some((major, minor)) = version.as_major_minor() else {
                    miette::miette!(
                        "expected glibc version to be a major.minor version, but got '{version}'"
                    )
                };
                platform_host::Platform::new(
                    Os::Manylinux {
                        major: major as _,
                        minor: minor as _,
                    },
                    arch,
                )
            }
            Some((family, _)) => {
                return Err(miette::miette!(
                    "unsupported libc family for pypi packages '{family}'"
                ));
            }
        }
    } else if platform.is_windows() {
        let arch = match platform.arch() {
            None => unreachable!("every platform we support has an arch"),
            Some(Arch::X86) => platform_host::Arch::X86,
            Some(Arch::X86_64) => platform_host::Arch::X86_64,
            Some(Arch::Aarch64) => platform_host::Arch::Aarch64,
            Some(unsupported_arch) => {
                miette::miette!("unsupported arch for pypi packages '{unsupported_arch}'")
            }
        };

        platform_host::Platform::new(Os::Windows, arch)
    } else if platform.is_osx() {
        let Some((major, minor)) = system_requirements
            .macos
            .unwrap_or_else(default_mac_os_version(platform))
            .as_major_minor()
        else {
            miette::miette!(
                "expected macos version to be a major.minor version, but got '{version}'"
            )
        };

        let arch = match platform.arch() {
            None => unreachable!("every platform we support has an arch"),
            Some(Arch::X86) => platform_host::Arch::X86,
            Some(Arch::X86_64) => platform_host::Arch::X86_64,
            Some(Arch::Aarch64) => platform_host::Arch::Aarch64,
            Some(unsupported_arch) => {
                miette::miette!("unsupported arch for pypi packages '{unsupported_arch}'")
            }
        };

        platform_host::Platform::new(
            Os::Macos {
                major: major as _,
                minor: minor as _,
            },
            arch,
        )
    } else {
        return Err(miette::miette!(
            "unsupported platform for pypi packages {platform}"
        ));
    };

    // Build the wheel tags based on the interpreter, the target platform, and the python version.
    let Some(python_version) = python_record.version.as_major_minor() else {
        return Err(miette::miette!(
            "expected python version to be a major.minor version, but got '{version}'"
        ));
    };
    let implementation_name = match python_record.name.as_normalized() {
        "python" => "cpython",
        "pypy" => "pypy",
        _ => {
            return Err(miette::miette!(
                "unsupported python implementation '{}'",
                python_record.name.as_source()
            ));
        }
    };
    let tags = Tags::from_env(
        &platform,
        (python_version.0 as u8, python_version.1 as u8),
        implementation_name,
        // TODO: This might not be entirely correct..
        (python_version.0 as u8, python_version.1 as u8),
    )
    .context("failed to determine the python wheel tags for the target platform")?;

    Ok(tags)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_cpython_tags() {
        let tags: Vec<_> = cpython_tags(&Version::from_str("3.11.2").unwrap(), vec!["win_amd64"])
            .into_iter()
            .map(|t| t.to_string())
            .collect();
        insta::assert_debug_snapshot!(tags);
    }

    #[test]
    fn test_py_interpreter_range() {
        let tags: Vec<_> = py_interpreter_range(&Version::from_str("3.11.2").unwrap()).collect();
        insta::assert_debug_snapshot!(tags);
    }

    #[test]
    fn test_compatible_tags() {
        let tags: Vec<_> =
            compatible_tags(&Version::from_str("3.11.2").unwrap(), vec!["win_amd64"])
                .map(|t| t.to_string())
                .collect();
        insta::assert_debug_snapshot!(tags);
    }

    #[test]
    fn test_linux_platform_tags() {
        let tags: Vec<_> =
            linux_platform_tags(Platform::Linux64, &Version::from_str("2.17").unwrap())
                .into_iter()
                .map(|t| t.to_string())
                .collect();
        insta::assert_debug_snapshot!(tags);
    }

    #[test]
    fn test_mac_platform_tags() {
        let tags: Vec<_> =
            mac_platform_tags(Platform::OsxArm64, &Version::from_str("14.0").unwrap())
                .into_iter()
                .map(|t| t.to_string())
                .collect();
        insta::assert_debug_snapshot!(tags);
    }
}
