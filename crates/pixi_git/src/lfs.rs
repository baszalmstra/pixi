//! [`GitLfs`] preference: whether a git checkout should also fetch git-LFS
//! objects. Mirrors `uv_git_types::GitLfs`'s two-variant shape; pixi keeps
//! the "no opinion" state at the call site as `Option<GitLfs>` so the env
//! var (`PIXI_GIT_LFS`) can act as a global default.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The environment variable consulted by [`GitLfs::from_env`].
pub const PIXI_GIT_LFS_ENV: &str = "PIXI_GIT_LFS";

/// Whether to fetch git-LFS objects when checking out a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GitLfs {
    /// Run `git lfs fetch` / `git lfs fsck` and let smudge filters
    /// materialise pointer files.
    Enabled,
    /// Force-skip the smudge filter; pointer files stay as pointers.
    Disabled,
}

impl GitLfs {
    /// Parse [`PIXI_GIT_LFS_ENV`]. Accepts `1`/`0`, `true`/`false`,
    /// `yes`/`no`, `on`/`off` (case-insensitive). Unset / empty → `None`;
    /// unrecognised values are treated as `Enabled` with a tracing warning
    /// (back-compat with the env-only landing in #6183).
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(PIXI_GIT_LFS_ENV).ok()?;
        let value = raw.trim();
        if value.is_empty() {
            return None;
        }
        if value == "0"
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("no")
            || value.eq_ignore_ascii_case("off")
        {
            return Some(Self::Disabled);
        }
        if value == "1"
            || value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("yes")
            || value.eq_ignore_ascii_case("on")
        {
            return Some(Self::Enabled);
        }
        tracing::warn!("unrecognised value for {PIXI_GIT_LFS_ENV}: {raw:?}; treating as enabled");
        Some(Self::Enabled)
    }

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

// Round-trip through serde as a bool. The manifest already writes
// `lfs = true/false` and the locked URL writes `?lfs=true|false`; keeping
// the serde shape as `bool` avoids re-formatting either.
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

    /// Serialised env-var swap to keep parallel tests from racing on
    /// the process-global env.
    fn with_env<R>(value: Option<&str>, body: impl FnOnce() -> R) -> R {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(PIXI_GIT_LFS_ENV).ok();
        // SAFETY: tests are serialised by LOCK above.
        match value {
            Some(v) => unsafe { std::env::set_var(PIXI_GIT_LFS_ENV, v) },
            None => unsafe { std::env::remove_var(PIXI_GIT_LFS_ENV) },
        }
        let out = body();
        match previous {
            Some(v) => unsafe { std::env::set_var(PIXI_GIT_LFS_ENV, v) },
            None => unsafe { std::env::remove_var(PIXI_GIT_LFS_ENV) },
        }
        out
    }

    #[test]
    fn env_unset_is_none() {
        with_env(None, || assert_eq!(GitLfs::from_env(), None));
    }

    #[test]
    fn env_empty_is_none() {
        with_env(Some(""), || assert_eq!(GitLfs::from_env(), None));
        with_env(Some("   "), || assert_eq!(GitLfs::from_env(), None));
    }

    #[test]
    fn env_truthy_is_enabled() {
        for v in ["1", "true", "TRUE", "yes", "YES", "on", "ON"] {
            with_env(Some(v), || {
                assert_eq!(GitLfs::from_env(), Some(GitLfs::Enabled), "value={v}")
            });
        }
    }

    #[test]
    fn env_falsy_is_disabled() {
        for v in ["0", "false", "FALSE", "no", "NO", "off", "OFF"] {
            with_env(Some(v), || {
                assert_eq!(GitLfs::from_env(), Some(GitLfs::Disabled), "value={v}")
            });
        }
    }
}
