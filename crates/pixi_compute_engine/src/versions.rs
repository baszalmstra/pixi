//! Graph-version primitives for compute snapshots.
//!
//! Tracks graph versions held by read-only snapshots and assigns an
//! epoch to each active lifetime of a version.

use std::{collections::HashMap, fmt};

use parking_lot::Mutex;

/// Monotonic compute-graph version.
///
/// A [`ComputeSnapshot`](crate::ComputeSnapshot) is bound to one
/// `GraphVersion`. The engine exposes versions for diagnostics and
/// threads snapshot identity through compute contexts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphVersion(u64);

impl GraphVersion {
    /// The initial graph version of a fresh engine.
    pub const ZERO: Self = Self(0);

    /// Return the underlying integer representation.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for GraphVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Epoch for one active lifetime of a graph version.
///
/// If all snapshots for `v10` drop and another caller asks for `v10`
/// again before the graph advances, it receives a different epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct VersionEpoch(u64);

#[derive(Debug)]
struct ActiveVersionData {
    ref_count: usize,
    epoch: VersionEpoch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VersionTrackerStats {
    pub current: GraphVersion,
    pub invalid_before: GraphVersion,
    pub active_versions: usize,
    pub active_leases: usize,
}

#[derive(Debug)]
struct VersionTrackerState {
    current: GraphVersion,
    invalid_before: GraphVersion,
    active_versions: HashMap<GraphVersion, ActiveVersionData>,
    next_epoch: u64,
}

/// Tracks the current graph version and active read snapshots.
#[derive(Debug)]
pub(crate) struct VersionTracker {
    state: Mutex<VersionTrackerState>,
}

impl VersionTracker {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(VersionTrackerState {
                current: GraphVersion::ZERO,
                invalid_before: GraphVersion::ZERO,
                active_versions: HashMap::new(),
                next_epoch: 0,
            }),
        }
    }

    /// Acquire a lease on the current graph version.
    pub(crate) fn acquire_current(&self) -> (GraphVersion, VersionEpoch) {
        let mut state = self.state.lock();
        let current = state.current;
        if let Some(active) = state.active_versions.get_mut(&current) {
            active.ref_count += 1;
            return (current, active.epoch);
        }

        let epoch = VersionEpoch(state.next_epoch);
        state.next_epoch += 1;
        state.active_versions.insert(
            current,
            ActiveVersionData {
                ref_count: 1,
                epoch,
            },
        );
        (current, epoch)
    }

    /// Release an active version lease. Returns true when this was the
    /// final lease for the version lifetime.
    pub(crate) fn release(&self, version: GraphVersion, epoch: VersionEpoch) -> bool {
        let mut state = self.state.lock();
        let Some(active) = state.active_versions.get_mut(&version) else {
            debug_assert!(false, "released inactive graph version {version}");
            return false;
        };

        debug_assert_eq!(
            active.epoch, epoch,
            "released graph version {version} with stale epoch"
        );

        active.ref_count -= 1;
        if active.ref_count == 0 {
            state.active_versions.remove(&version);
            true
        } else {
            false
        }
    }

    pub(crate) fn current(&self) -> GraphVersion {
        self.state.lock().current
    }

    /// The oldest graph version that can still be observed by a live
    /// snapshot. If no older snapshot is active, current graph state is
    /// the retention floor.
    pub(crate) fn retention_floor(&self) -> GraphVersion {
        let state = self.state.lock();
        state
            .active_versions
            .keys()
            .copied()
            .filter(|version| *version >= state.invalid_before)
            .min()
            .unwrap_or(state.current)
    }

    pub(crate) fn is_active(&self, version: GraphVersion, epoch: VersionEpoch) -> bool {
        let state = self.state.lock();
        version >= state.invalid_before
            && state
                .active_versions
                .get(&version)
                .is_some_and(|active| active.epoch == epoch)
    }

    pub(crate) fn should_reject(&self, version: GraphVersion) -> bool {
        version < self.state.lock().invalid_before
    }

    pub(crate) fn stats(&self) -> VersionTrackerStats {
        let state = self.state.lock();
        VersionTrackerStats {
            current: state.current,
            invalid_before: state.invalid_before,
            active_versions: state.active_versions.len(),
            active_leases: state
                .active_versions
                .values()
                .map(|active| active.ref_count)
                .sum(),
        }
    }

    pub(crate) fn clear(&self) -> GraphVersion {
        let mut state = self.state.lock();
        state.current = state.current.next();
        state.invalid_before = state.current;
        state.current
    }

    pub(crate) fn commit_next_if_changed_with(
        &self,
        f: impl FnOnce(GraphVersion) -> bool,
    ) -> GraphVersion {
        let mut state = self.state.lock();
        let current = state.current;
        let next = current.next();
        if f(next) {
            state.current = next;
            next
        } else {
            current
        }
    }

    /// Test helper that advances the current graph version.
    #[allow(dead_code)]
    pub(crate) fn advance_for_tests(&self) -> GraphVersion {
        let mut state = self.state.lock();
        state.current = state.current.next();
        state.current
    }

    #[cfg(test)]
    fn active_ref_count(&self, version: GraphVersion) -> Option<usize> {
        self.state
            .lock()
            .active_versions
            .get(&version)
            .map(|active| active.ref_count)
    }
}

impl Default for VersionTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_and_release_current_version() {
        let tracker = VersionTracker::new();
        let (v1, e1) = tracker.acquire_current();
        assert_eq!(v1, GraphVersion::ZERO);
        assert_eq!(tracker.active_ref_count(v1), Some(1));

        let (v2, e2) = tracker.acquire_current();
        assert_eq!(v2, v1);
        assert_eq!(e2, e1);
        assert_eq!(tracker.active_ref_count(v1), Some(2));

        tracker.release(v1, e1);
        assert_eq!(tracker.active_ref_count(v1), Some(1));

        tracker.release(v2, e2);
        assert_eq!(tracker.active_ref_count(v1), None);
    }

    #[test]
    fn reacquiring_idle_version_allocates_new_epoch() {
        let tracker = VersionTracker::new();
        let (v1, e1) = tracker.acquire_current();
        tracker.release(v1, e1);

        let (v2, e2) = tracker.acquire_current();
        assert_eq!(v2, v1);
        assert_ne!(e2, e1);
    }

    #[test]
    fn advance_changes_current_version() {
        let tracker = VersionTracker::new();
        assert_eq!(tracker.current(), GraphVersion::ZERO);
        assert_eq!(tracker.advance_for_tests(), GraphVersion(1));
        let (version, _) = tracker.acquire_current();
        assert_eq!(version, GraphVersion(1));
    }
}
