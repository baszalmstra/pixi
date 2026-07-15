//! The pure probe model behind the workspace input fingerprint.
//!
//! Everything in this module is a pure function over bytes/values: probe
//! computation and validation take a [`FingerprintInputs`] snapshot instead of
//! touching a `Workspace` or the filesystem. The glue that adapts a
//! `Workspace` to a [`FingerprintInputs`] lives in the parent module. This
//! separation is what makes the model unit-testable without constructing a
//! workspace or mutating process-global state (like environment variables).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

/// The current schema version of the serialized [`WorkspaceFingerprint`].
///
/// Bump this when the meaning of an existing probe changes. A marker recorded
/// with a different schema version is treated as stale, never as an error.
pub const FINGERPRINT_SCHEMA_VERSION: u32 = 1;

/// Environment variables whose name starts with this prefix override virtual
/// package detection and therefore affect resolution.
pub const CONDA_OVERRIDE_PREFIX: &str = "CONDA_OVERRIDE_";

/// Environment variables whose name starts with this prefix override pixi's
/// own platform/virtual-package handling (`PIXI_OVERRIDE_PLATFORM` today) and
/// therefore affect resolution.
pub const PIXI_OVERRIDE_PREFIX: &str = "PIXI_OVERRIDE_";

/// Digest value recorded for the lock-file probe when no lock file exists on
/// disk. Distinct from every real digest (those are 16 hex characters).
const LOCK_FILE_ABSENT_DIGEST: &str = "absent";

/// Probe kind names, used for the `kind` tag on disk and in stale reasons.
///
/// These must mirror the serde kebab-case tags of [`InputProbe`]; the
/// `computed_probes_cover_exactly_the_required_kinds` test pins the sync.
mod kind {
    pub const MANIFEST_FILE_CONTENT: &str = "manifest-file-content";
    pub const RESOLVED_CONFIG: &str = "resolved-config";
    pub const LOCK_FILE_CONTENT: &str = "lock-file-content";
    pub const ENV_VARS: &str = "env-vars";
    pub const PIXI_VERSION: &str = "pixi-version";
}

/// Every probe kind a v1 fingerprint must record for validation to be able
/// to return fresh.
///
/// This is the single authoritative list: [`WorkspaceFingerprint::validate`]
/// checks marker completeness against it, and the
/// `computed_probes_cover_exactly_the_required_kinds` test asserts that
/// [`WorkspaceFingerprint::compute`] emits exactly these kinds — so adding a
/// probe to `compute` without extending this list (or vice versa) fails a
/// test instead of silently letting markers that lack the new probe validate
/// as fresh.
const REQUIRED_PROBE_KINDS: [&str; 5] = [
    kind::MANIFEST_FILE_CONTENT,
    kind::RESOLVED_CONFIG,
    kind::LOCK_FILE_CONTENT,
    kind::ENV_VARS,
    kind::PIXI_VERSION,
];

/// A single independently re-checkable statement about one resolution input.
///
/// The enum is internally tagged (`kind`) with a catch-all [`Self::Unknown`]
/// variant so that markers written by a newer pixi deserialize without error;
/// validation then reports them as stale instead of failing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InputProbe {
    /// xxh3 digest of the raw bytes of the workspace manifest file
    /// (`pixi.toml` / `pyproject.toml` / `mojoproject.toml`).
    ManifestFileContent {
        /// The absolute path of the manifest the digest was computed from.
        path: String,
        /// Hex-encoded xxh3 of the manifest file bytes.
        digest: String,
    },
    /// xxh3 digest of the JSON serialization of the merged, resolved
    /// configuration. Field-order drift between pixi versions is pinned by the
    /// [`InputProbe::PixiVersion`] probe.
    ResolvedConfig {
        /// Hex-encoded xxh3 of the serialized configuration.
        digest: String,
    },
    /// xxh3 digest of the raw on-disk bytes of `pixi.lock`, or a sentinel when
    /// the file does not exist.
    LockFileContent {
        /// Hex-encoded xxh3 of the lock file bytes, or `"absent"`.
        digest: String,
    },
    /// The resolution-affecting override environment variables and their
    /// values at record time. `None` means the variable was unset.
    EnvVars {
        /// Variable name to value (or `None` when unset at record time).
        vars: BTreeMap<String, Option<String>>,
    },
    /// The pixi version that recorded the fingerprint.
    PixiVersion {
        /// The value of `consts::PIXI_VERSION` at record time.
        version: String,
    },
    /// A probe kind this version of pixi does not know about. Deserialization
    /// never fails on it; validation reports it as stale.
    #[serde(other)]
    Unknown,
}

/// A snapshot of the current values of all L0 inputs covered by v1 probes.
///
/// Pure data: the glue layer fills this from a `Workspace`, unit tests fill it
/// directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintInputs {
    /// Absolute path of the workspace manifest file.
    pub manifest_path: String,
    /// Raw bytes of the workspace manifest file.
    pub manifest_bytes: Vec<u8>,
    /// JSON serialization of the merged, resolved configuration.
    pub config_json: Vec<u8>,
    /// Raw bytes of the lock file, or `None` when no lock file exists.
    pub lock_file_bytes: Option<Vec<u8>>,
    /// The override environment variables, as gathered by
    /// [`collect_override_env_vars`].
    pub override_env_vars: BTreeMap<String, Option<String>>,
    /// The current pixi version.
    pub pixi_version: String,
}

/// The reason a recorded fingerprint no longer matches the current inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleReason {
    /// The marker was written with a different schema version.
    SchemaVersion {
        /// The schema version found in the marker.
        recorded: u32,
        /// The schema version this pixi supports.
        supported: u32,
    },
    /// The marker contains a probe kind this pixi does not understand.
    UnknownProbe,
    /// The marker lacks a probe kind that v1 requires.
    MissingProbe {
        /// The `kind` tag of the missing probe.
        kind: &'static str,
    },
    /// The manifest file moved or its content changed.
    ManifestChanged,
    /// The merged, resolved configuration changed.
    ConfigChanged,
    /// The lock file bytes changed (including appearing or disappearing).
    LockFileChanged,
    /// An override environment variable was set, unset, or changed.
    EnvVarChanged {
        /// The name of the first differing variable.
        name: String,
    },
    /// The fingerprint was recorded by a different pixi version.
    PixiVersionChanged {
        /// The version that recorded the marker.
        recorded: String,
        /// The currently running version.
        current: String,
    },
    /// A current input could not be gathered (e.g. the manifest disappeared).
    InputUnavailable {
        /// Human-readable description of what failed.
        reason: String,
    },
}

impl std::fmt::Display for StaleReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StaleReason::SchemaVersion {
                recorded,
                supported,
            } => write!(
                f,
                "the marker uses schema version {recorded}, this version of pixi supports {supported}"
            ),
            StaleReason::UnknownProbe => {
                write!(
                    f,
                    "the marker contains a probe kind unknown to this version of pixi"
                )
            }
            StaleReason::MissingProbe { kind } => {
                write!(f, "the marker lacks the required `{kind}` probe")
            }
            StaleReason::ManifestChanged => write!(f, "the workspace manifest changed"),
            StaleReason::ConfigChanged => write!(f, "the resolved configuration changed"),
            StaleReason::LockFileChanged => write!(f, "the lock file changed"),
            StaleReason::EnvVarChanged { name } => {
                write!(f, "the environment variable `{name}` changed")
            }
            StaleReason::PixiVersionChanged { recorded, current } => write!(
                f,
                "the marker was recorded by pixi {recorded}, currently running {current}"
            ),
            StaleReason::InputUnavailable { reason } => {
                write!(f, "a required input could not be read: {reason}")
            }
        }
    }
}

/// The outcome of validating a parsed fingerprint against current inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeValidation {
    /// Every probe matched: the inputs are unchanged.
    Fresh,
    /// At least one probe did not match.
    Stale(StaleReason),
}

/// The outcome of evaluating an on-disk marker, including the cases where no
/// usable marker exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshnessCheck {
    /// A valid marker exists and every probe matched.
    Fresh,
    /// A valid marker exists but at least one probe did not match.
    Stale(StaleReason),
    /// No marker exists, or it could not be read or parsed.
    Absent,
}

/// A recorded, re-checkable trace of the workspace's resolution inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFingerprint {
    /// Schema version of this marker; see [`FINGERPRINT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The recorded probes.
    pub probes: Vec<InputProbe>,
}

/// Returns the hex-encoded xxh3 digest of `bytes`.
pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:016x}", xxh3_64(bytes))
}

/// Returns the digest recorded for the lock-file probe: the digest of the
/// bytes, or a distinct sentinel when the file is absent.
fn lock_file_digest(bytes: Option<&[u8]>) -> String {
    match bytes {
        Some(bytes) => digest_bytes(bytes),
        None => LOCK_FILE_ABSENT_DIGEST.to_string(),
    }
}

/// Collects the resolution-affecting override environment variables from an
/// arbitrary variable listing.
///
/// Included are every variable whose name starts with
/// [`CONDA_OVERRIDE_PREFIX`] or [`PIXI_OVERRIDE_PREFIX`]; both are prefix
/// matches so that overrides added later (in rattler or in
/// `Workspace::host_platform` and friends, see
/// `crate::workspace::PlatformOverrides`) are captured without this list
/// needing to know about them — over-invalidation is safe,
/// under-invalidation is not. `PIXI_OVERRIDE_PLATFORM` is additionally always
/// present in the result (with `None` when unset) so set/unset transitions
/// are caught. Pure function: pass `std::env::vars_os()` (lossily converted)
/// in production, a literal iterator in tests.
pub fn collect_override_env_vars(
    vars: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, Option<String>> {
    let mut collected: BTreeMap<String, Option<String>> = BTreeMap::new();
    // Always present so a set/unset transition changes the map.
    collected.insert(
        pixi_consts::consts::PIXI_OVERRIDE_PLATFORM.to_string(),
        None,
    );
    for (name, value) in vars {
        if name.starts_with(CONDA_OVERRIDE_PREFIX) || name.starts_with(PIXI_OVERRIDE_PREFIX) {
            collected.insert(name, Some(value));
        }
    }
    collected
}

/// Returns the first variable name at which the two maps differ, if any.
fn first_env_var_difference(
    recorded: &BTreeMap<String, Option<String>>,
    current: &BTreeMap<String, Option<String>>,
) -> Option<String> {
    recorded
        .iter()
        .find(|(name, value)| current.get(*name) != Some(value))
        .map(|(name, _)| name.clone())
        .or_else(|| {
            current
                .keys()
                .find(|name| !recorded.contains_key(*name))
                .cloned()
        })
}

impl WorkspaceFingerprint {
    /// Computes the fingerprint of the given input snapshot.
    pub fn compute(inputs: &FingerprintInputs) -> Self {
        Self {
            schema_version: FINGERPRINT_SCHEMA_VERSION,
            probes: vec![
                InputProbe::ManifestFileContent {
                    path: inputs.manifest_path.clone(),
                    digest: digest_bytes(&inputs.manifest_bytes),
                },
                InputProbe::ResolvedConfig {
                    digest: digest_bytes(&inputs.config_json),
                },
                InputProbe::LockFileContent {
                    digest: lock_file_digest(inputs.lock_file_bytes.as_deref()),
                },
                InputProbe::EnvVars {
                    vars: inputs.override_env_vars.clone(),
                },
                InputProbe::PixiVersion {
                    version: inputs.pixi_version.clone(),
                },
            ],
        }
    }

    /// Validates this fingerprint against the given input snapshot.
    ///
    /// Returns [`ProbeValidation::Fresh`] only when the schema version
    /// matches, every recorded probe re-validates, no probe is of an unknown
    /// kind, and every probe kind required by v1 is present.
    pub fn validate(&self, inputs: &FingerprintInputs) -> ProbeValidation {
        if self.schema_version != FINGERPRINT_SCHEMA_VERSION {
            return ProbeValidation::Stale(StaleReason::SchemaVersion {
                recorded: self.schema_version,
                supported: FINGERPRINT_SCHEMA_VERSION,
            });
        }

        let mut seen_kinds = Vec::with_capacity(self.probes.len());
        for probe in &self.probes {
            match probe {
                InputProbe::ManifestFileContent { path, digest } => {
                    seen_kinds.push(kind::MANIFEST_FILE_CONTENT);
                    if *path != inputs.manifest_path
                        || *digest != digest_bytes(&inputs.manifest_bytes)
                    {
                        return ProbeValidation::Stale(StaleReason::ManifestChanged);
                    }
                }
                InputProbe::ResolvedConfig { digest } => {
                    seen_kinds.push(kind::RESOLVED_CONFIG);
                    if *digest != digest_bytes(&inputs.config_json) {
                        return ProbeValidation::Stale(StaleReason::ConfigChanged);
                    }
                }
                InputProbe::LockFileContent { digest } => {
                    seen_kinds.push(kind::LOCK_FILE_CONTENT);
                    if *digest != lock_file_digest(inputs.lock_file_bytes.as_deref()) {
                        return ProbeValidation::Stale(StaleReason::LockFileChanged);
                    }
                }
                InputProbe::EnvVars { vars } => {
                    seen_kinds.push(kind::ENV_VARS);
                    if let Some(name) = first_env_var_difference(vars, &inputs.override_env_vars) {
                        return ProbeValidation::Stale(StaleReason::EnvVarChanged { name });
                    }
                }
                InputProbe::PixiVersion { version } => {
                    seen_kinds.push(kind::PIXI_VERSION);
                    if *version != inputs.pixi_version {
                        return ProbeValidation::Stale(StaleReason::PixiVersionChanged {
                            recorded: version.clone(),
                            current: inputs.pixi_version.clone(),
                        });
                    }
                }
                InputProbe::Unknown => {
                    return ProbeValidation::Stale(StaleReason::UnknownProbe);
                }
            }
        }

        // Every probe kind v1 relies on must be present: a marker that lost a
        // probe proves nothing about that input.
        for required in REQUIRED_PROBE_KINDS {
            if !seen_kinds.contains(&required) {
                return ProbeValidation::Stale(StaleReason::MissingProbe { kind: required });
            }
        }

        ProbeValidation::Fresh
    }

    /// Serializes the fingerprint to pretty-printed JSON bytes.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }
}

/// Evaluates an on-disk marker (if any) against the current inputs.
///
/// * `None` or unparseable bytes ⇒ [`FreshnessCheck::Absent`],
/// * a parseable marker ⇒ the result of [`WorkspaceFingerprint::validate`].
///
/// Every failure mode degrades to a miss (slow but correct), never an error.
pub fn evaluate_marker(marker_bytes: Option<&[u8]>, inputs: &FingerprintInputs) -> FreshnessCheck {
    let Some(bytes) = marker_bytes else {
        return FreshnessCheck::Absent;
    };
    match serde_json::from_slice::<WorkspaceFingerprint>(bytes) {
        Ok(fingerprint) => match fingerprint.validate(inputs) {
            ProbeValidation::Fresh => FreshnessCheck::Fresh,
            ProbeValidation::Stale(reason) => FreshnessCheck::Stale(reason),
        },
        // A corrupt marker is not a marker.
        Err(_) => FreshnessCheck::Absent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully populated input snapshot used as the "recorded" state.
    fn sample_inputs() -> FingerprintInputs {
        FingerprintInputs {
            manifest_path: "/workspace/pixi.toml".to_string(),
            manifest_bytes: b"[workspace]\nname = \"test\"\n".to_vec(),
            config_json: br#"{"default-channels":["conda-forge"]}"#.to_vec(),
            lock_file_bytes: Some(b"version: 6\nenvironments: {}\npackages: []\n".to_vec()),
            override_env_vars: collect_override_env_vars([
                ("CONDA_OVERRIDE_CUDA".to_string(), "12.0".to_string()),
                ("HOME".to_string(), "/home/user".to_string()),
            ]),
            pixi_version: "1.2.3".to_string(),
        }
    }

    #[test]
    fn collect_override_env_vars_filters_and_pins_platform_override() {
        let vars = collect_override_env_vars([
            ("CONDA_OVERRIDE_CUDA".to_string(), "12.0".to_string()),
            ("CONDA_OVERRIDE_GLIBC".to_string(), "2.28".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("CONDA_PREFIX".to_string(), "/opt/conda".to_string()),
        ]);

        // Both CONDA_OVERRIDE_* vars are captured with their values.
        assert_eq!(
            vars.get("CONDA_OVERRIDE_CUDA"),
            Some(&Some("12.0".to_string()))
        );
        assert_eq!(
            vars.get("CONDA_OVERRIDE_GLIBC"),
            Some(&Some("2.28".to_string()))
        );
        // PIXI_OVERRIDE_PLATFORM is always present, `None` when unset.
        assert_eq!(vars.get("PIXI_OVERRIDE_PLATFORM"), Some(&None));
        // Unrelated variables are not captured.
        assert_eq!(vars.len(), 3);

        // When PIXI_OVERRIDE_PLATFORM is set, its value is captured.
        let vars = collect_override_env_vars([(
            "PIXI_OVERRIDE_PLATFORM".to_string(),
            "linux-64".to_string(),
        )]);
        assert_eq!(
            vars.get("PIXI_OVERRIDE_PLATFORM"),
            Some(&Some("linux-64".to_string()))
        );
        assert_eq!(vars.len(), 1);

        // Any future pixi-side override variable is captured by prefix, so
        // the probe cannot silently lag behind newly honored overrides
        // (over-invalidation is safe, under-invalidation is not).
        let vars = collect_override_env_vars([(
            "PIXI_OVERRIDE_VIRTUAL_PACKAGES".to_string(),
            "cuda=12".to_string(),
        )]);
        assert_eq!(
            vars.get("PIXI_OVERRIDE_VIRTUAL_PACKAGES"),
            Some(&Some("cuda=12".to_string()))
        );
    }

    #[test]
    fn computed_probes_cover_exactly_the_required_kinds() {
        // Pins the sync between `compute` and `REQUIRED_PROBE_KINDS`: adding
        // a probe to one without the other must fail here, otherwise markers
        // lacking the new probe would keep validating as fresh.
        let fingerprint = WorkspaceFingerprint::compute(&sample_inputs());
        let computed_kinds: Vec<String> = fingerprint
            .probes
            .iter()
            .map(|probe| {
                serde_json::to_value(probe).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(computed_kinds, REQUIRED_PROBE_KINDS);
    }

    #[test]
    fn serde_roundtrip_preserves_fingerprint() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);
        assert_eq!(fingerprint.schema_version, FINGERPRINT_SCHEMA_VERSION);

        let bytes = fingerprint.to_json_bytes().unwrap();
        let parsed: WorkspaceFingerprint = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, fingerprint);

        // A roundtripped fingerprint still validates as fresh.
        assert_eq!(parsed.validate(&inputs), ProbeValidation::Fresh);
    }

    #[test]
    fn all_probes_matching_is_fresh() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);
        assert_eq!(fingerprint.validate(&inputs), ProbeValidation::Fresh);
        assert_eq!(
            evaluate_marker(Some(&fingerprint.to_json_bytes().unwrap()), &inputs),
            FreshnessCheck::Fresh
        );
    }

    #[test]
    fn manifest_content_change_is_stale() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);

        let mut changed = inputs.clone();
        changed.manifest_bytes = b"[workspace]\nname = \"changed\"\n".to_vec();
        assert_eq!(
            fingerprint.validate(&changed),
            ProbeValidation::Stale(StaleReason::ManifestChanged)
        );
    }

    #[test]
    fn manifest_path_change_is_stale() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);

        let mut changed = inputs.clone();
        changed.manifest_path = "/workspace/pyproject.toml".to_string();
        assert_eq!(
            fingerprint.validate(&changed),
            ProbeValidation::Stale(StaleReason::ManifestChanged)
        );
    }

    #[test]
    fn config_change_is_stale() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);

        let mut changed = inputs.clone();
        changed.config_json = br#"{"default-channels":["bioconda"]}"#.to_vec();
        assert_eq!(
            fingerprint.validate(&changed),
            ProbeValidation::Stale(StaleReason::ConfigChanged)
        );
    }

    #[test]
    fn lock_file_change_is_stale() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);

        // Changed bytes.
        let mut changed = inputs.clone();
        changed.lock_file_bytes = Some(b"version: 6\nsomething: else\n".to_vec());
        assert_eq!(
            fingerprint.validate(&changed),
            ProbeValidation::Stale(StaleReason::LockFileChanged)
        );

        // Present -> absent.
        let mut removed = inputs.clone();
        removed.lock_file_bytes = None;
        assert_eq!(
            fingerprint.validate(&removed),
            ProbeValidation::Stale(StaleReason::LockFileChanged)
        );

        // Absent at record time -> present now.
        let mut absent_inputs = inputs.clone();
        absent_inputs.lock_file_bytes = None;
        let absent_fingerprint = WorkspaceFingerprint::compute(&absent_inputs);
        assert_eq!(
            absent_fingerprint.validate(&absent_inputs),
            ProbeValidation::Fresh
        );
        assert_eq!(
            absent_fingerprint.validate(&inputs),
            ProbeValidation::Stale(StaleReason::LockFileChanged)
        );
    }

    #[test]
    fn env_var_changes_are_stale() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);

        // Changing the value of a recorded variable.
        let mut changed = inputs.clone();
        changed.override_env_vars =
            collect_override_env_vars([("CONDA_OVERRIDE_CUDA".to_string(), "11.8".to_string())]);
        assert_eq!(
            fingerprint.validate(&changed),
            ProbeValidation::Stale(StaleReason::EnvVarChanged {
                name: "CONDA_OVERRIDE_CUDA".to_string()
            })
        );

        // Unsetting a recorded variable.
        let mut unset = inputs.clone();
        unset.override_env_vars = collect_override_env_vars(std::iter::empty());
        assert_eq!(
            fingerprint.validate(&unset),
            ProbeValidation::Stale(StaleReason::EnvVarChanged {
                name: "CONDA_OVERRIDE_CUDA".to_string()
            })
        );

        // Setting a new override variable that was unset at record time.
        let mut added = inputs.clone();
        added.override_env_vars = collect_override_env_vars([
            ("CONDA_OVERRIDE_CUDA".to_string(), "12.0".to_string()),
            ("CONDA_OVERRIDE_OSX".to_string(), "14.0".to_string()),
        ]);
        assert_eq!(
            fingerprint.validate(&added),
            ProbeValidation::Stale(StaleReason::EnvVarChanged {
                name: "CONDA_OVERRIDE_OSX".to_string()
            })
        );

        // Setting PIXI_OVERRIDE_PLATFORM (recorded as unset).
        let mut platform_set = inputs.clone();
        platform_set.override_env_vars = collect_override_env_vars([
            ("CONDA_OVERRIDE_CUDA".to_string(), "12.0".to_string()),
            (
                "PIXI_OVERRIDE_PLATFORM".to_string(),
                "osx-arm64".to_string(),
            ),
        ]);
        assert_eq!(
            fingerprint.validate(&platform_set),
            ProbeValidation::Stale(StaleReason::EnvVarChanged {
                name: "PIXI_OVERRIDE_PLATFORM".to_string()
            })
        );
    }

    #[test]
    fn pixi_version_change_is_stale() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);

        let mut changed = inputs.clone();
        changed.pixi_version = "9.9.9".to_string();
        assert_eq!(
            fingerprint.validate(&changed),
            ProbeValidation::Stale(StaleReason::PixiVersionChanged {
                recorded: "1.2.3".to_string(),
                current: "9.9.9".to_string(),
            })
        );
    }

    #[test]
    fn unknown_probe_kind_deserializes_and_is_stale() {
        let inputs = sample_inputs();
        let fingerprint = WorkspaceFingerprint::compute(&inputs);

        // Simulate a marker written by a future pixi that records an extra
        // probe kind with fields we know nothing about.
        let mut value: serde_json::Value =
            serde_json::from_slice(&fingerprint.to_json_bytes().unwrap()).unwrap();
        value["probes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "kind": "file-set",
                "root": "/workspace/src",
                "globs": ["**/*.rs"],
                "digest": "0011223344556677"
            }));
        let bytes = serde_json::to_vec(&value).unwrap();

        // Deserialization must NOT fail...
        let parsed: WorkspaceFingerprint = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed.probes.contains(&InputProbe::Unknown));

        // ...and validation must report stale, not fresh and not absent.
        assert_eq!(
            evaluate_marker(Some(&bytes), &inputs),
            FreshnessCheck::Stale(StaleReason::UnknownProbe)
        );
    }

    #[test]
    fn schema_version_mismatch_is_stale() {
        let inputs = sample_inputs();
        let mut fingerprint = WorkspaceFingerprint::compute(&inputs);
        fingerprint.schema_version = FINGERPRINT_SCHEMA_VERSION + 1;
        assert_eq!(
            fingerprint.validate(&inputs),
            ProbeValidation::Stale(StaleReason::SchemaVersion {
                recorded: FINGERPRINT_SCHEMA_VERSION + 1,
                supported: FINGERPRINT_SCHEMA_VERSION,
            })
        );
    }

    #[test]
    fn missing_probe_is_stale() {
        let inputs = sample_inputs();
        let mut fingerprint = WorkspaceFingerprint::compute(&inputs);
        fingerprint
            .probes
            .retain(|probe| !matches!(probe, InputProbe::PixiVersion { .. }));
        assert_eq!(
            fingerprint.validate(&inputs),
            ProbeValidation::Stale(StaleReason::MissingProbe {
                kind: "pixi-version"
            })
        );
    }

    #[test]
    fn corrupt_or_missing_marker_is_absent() {
        let inputs = sample_inputs();

        // Missing marker.
        assert_eq!(evaluate_marker(None, &inputs), FreshnessCheck::Absent);

        // Not JSON at all.
        assert_eq!(
            evaluate_marker(Some(b"definitely not json"), &inputs),
            FreshnessCheck::Absent
        );

        // Valid JSON with the wrong shape.
        assert_eq!(
            evaluate_marker(Some(br#"{"probes": "nope"}"#), &inputs),
            FreshnessCheck::Absent
        );

        // Truncated marker.
        let bytes = WorkspaceFingerprint::compute(&inputs)
            .to_json_bytes()
            .unwrap();
        assert_eq!(
            evaluate_marker(Some(&bytes[..bytes.len() / 2]), &inputs),
            FreshnessCheck::Absent
        );
    }

    #[test]
    fn digests_are_hex_encoded_xxh3() {
        // Pin the exact digest encoding: 16 lowercase hex chars of xxh3_64.
        let digest = digest_bytes(b"hello");
        assert_eq!(digest.len(), 16);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, format!("{:016x}", xxh3_64(b"hello")));
    }
}
