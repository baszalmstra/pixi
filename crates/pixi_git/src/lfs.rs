//! [`GitLfs`] preference: whether a git checkout should also fetch git-LFS
//! objects. Mirrors `uv_git_types::GitLfs`'s two-variant shape.
//!
//! Unlike uv, pixi does **not** consult an environment variable: an
//! unspecified `lfs` deterministically means [`GitLfs::Disabled`]. Letting
//! the env decide would make the parsed manifest — and therefore the lock
//! file — depend on ambient state, so the same `pixi.toml` could resolve
//! differently on two machines. (uv's `UV_GIT_LFS` flows into its lock file
//! this way; we deliberately avoid that.)

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Whether to fetch git-LFS objects when checking out a repository.
/// Mirrors `uv_git_types::GitLfs`. Defaults to [`GitLfs::Disabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum GitLfs {
    /// Force-skip the smudge filter; pointer files stay as pointers.
    #[default]
    Disabled,
    /// Run `git lfs fetch` / `git lfs fsck` and let smudge filters
    /// materialise pointer files.
    Enabled,
}

impl GitLfs {
    /// `true` for [`GitLfs::Enabled`].
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

impl From<bool> for GitLfs {
    fn from(b: bool) -> Self {
        if b { Self::Enabled } else { Self::Disabled }
    }
}

impl From<GitLfs> for bool {
    fn from(lfs: GitLfs) -> Self {
        lfs.is_enabled()
    }
}

/// Resolve the manifest-input tri-state (`lfs = true/false` or absent) to a
/// concrete policy. Absence is [`GitLfs::Disabled`] — deterministic, no env
/// lookup, so the parsed manifest and lock file are a pure function of the
/// manifest text.
impl From<Option<bool>> for GitLfs {
    fn from(value: Option<bool>) -> Self {
        match value {
            Some(true) => Self::Enabled,
            Some(false) | None => Self::Disabled,
        }
    }
}

// Round-trip through serde as a bool. The manifest writes `lfs = true` and
// the locked URL writes `?lfs=true`; keeping the serde shape as `bool`
// avoids re-formatting either.
impl Serialize for GitLfs {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_bool(self.is_enabled())
    }
}

impl<'de> Deserialize<'de> for GitLfs {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        bool::deserialize(de).map(GitLfs::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_roundtrip() {
        assert_eq!(GitLfs::from(true), GitLfs::Enabled);
        assert_eq!(GitLfs::from(false), GitLfs::Disabled);
        assert!(GitLfs::Enabled.is_enabled());
        assert!(!GitLfs::Disabled.is_enabled());
    }

    /// `From<Option<bool>>` is the single tri-state → binary boundary.
    /// Absence is deterministically `Disabled` (no env lookup), so a given
    /// manifest always resolves the same way.
    #[test]
    fn from_option_bool_is_deterministic() {
        assert_eq!(GitLfs::from(Some(true)), GitLfs::Enabled);
        assert_eq!(GitLfs::from(Some(false)), GitLfs::Disabled);
        assert_eq!(GitLfs::from(None::<bool>), GitLfs::Disabled);
    }
}
