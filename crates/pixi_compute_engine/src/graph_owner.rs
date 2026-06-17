//! Single-owner access to the compute graph.
//!
//! A dedicated thread owns the [`KeyGraph`] value. Engine handles send
//! short synchronous jobs to that thread for lookup, insertion, and
//! introspection, so graph state is mutated in one place.

use std::{
    panic::AssertUnwindSafe,
    sync::{Arc, mpsc},
    thread,
};

use crate::{
    AnyKey, ComputeError, InjectedKey, Key, UpdateError,
    key_graph::{
        KeyFuture, KeyGraph, KeyGraphStats, Lookup, NodeRecord, RecordedDeps, SpawnGeneration,
    },
    versions::{GraphVersion, VersionEpoch, VersionTracker, VersionTrackerStats},
};

struct ComputeState {
    graph: KeyGraph,
    versions: VersionTracker,
}

pub(crate) struct GraphOwnerStats {
    pub versions: VersionTrackerStats,
    pub graph: KeyGraphStats,
}

pub(crate) struct GraphUpdateResult {
    pub previous_version: GraphVersion,
    pub current_version: GraphVersion,
    pub changed: bool,
}

pub(crate) struct GraphChange {
    apply: Box<dyn FnOnce(&KeyGraph, GraphVersion) -> bool + Send + 'static>,
}

impl GraphChange {
    pub(crate) fn changed<K: Key>(key: K) -> Self {
        Self {
            apply: Box::new(move |graph, version| graph.mark_dirty(&key, version)),
        }
    }

    pub(crate) fn changed_to<K: Key>(key: K, value: K::Value) -> Self {
        Self {
            apply: Box::new(move |graph, version| graph.changed_to(&key, value, version)),
        }
    }

    fn apply(self, graph: &KeyGraph, version: GraphVersion) -> bool {
        (self.apply)(graph, version)
    }
}

type GraphJob = Box<dyn FnOnce(&ComputeState) + Send + 'static>;

/// Serialized owner for compute graph state.
pub(crate) struct GraphOwner {
    tx: mpsc::Sender<GraphJob>,
    state: Arc<ComputeState>,
}

impl GraphOwner {
    pub(crate) fn new() -> Self {
        let (tx, rx) = mpsc::channel::<GraphJob>();
        let state = Arc::new(ComputeState {
            graph: KeyGraph::default(),
            versions: VersionTracker::default(),
        });
        let owner_state = state.clone();
        thread::Builder::new()
            .name("pixi-compute-graph".to_owned())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job(&owner_state);
                }
            })
            .expect("failed to spawn compute graph owner");

        Self { tx, state }
    }

    pub(crate) fn acquire_current(&self) -> (GraphVersion, VersionEpoch) {
        self.run(|state| state.versions.acquire_current())
    }

    pub(crate) fn release_version(&self, version: GraphVersion, epoch: VersionEpoch) {
        self.run(move |state| {
            if state.versions.release(version, epoch) {
                state.graph.remove_in_flight_for(version, epoch);
                state.graph.prune_history(state.versions.retention_floor());
            }
        });
    }

    pub(crate) fn current_version(&self) -> GraphVersion {
        self.run(|state| state.versions.current())
    }

    pub(crate) fn should_reject(&self, version: GraphVersion) -> bool {
        self.run(move |state| state.versions.should_reject(version))
    }

    pub(crate) fn unstable_drop_everything(&self) -> GraphVersion {
        self.run(|state| {
            let version = state.versions.clear();
            state.graph.clear();
            version
        })
    }

    #[cfg(test)]
    pub(crate) fn advance_for_tests(&self) -> GraphVersion {
        self.run(|state| state.versions.advance_for_tests())
    }

    pub(crate) fn get_or_insert_with<K, F>(
        &self,
        key: K,
        version: GraphVersion,
        epoch: VersionEpoch,
        make_future: F,
    ) -> Lookup<K::Value>
    where
        K: Key,
        F: FnOnce(SpawnGeneration) -> KeyFuture<K> + Send + 'static,
    {
        self.run(move |state| {
            state
                .graph
                .get_or_insert_with(&key, version, epoch, make_future)
        })
    }

    pub(crate) fn lookup<K: Key>(
        &self,
        key: K,
        version: GraphVersion,
        epoch: VersionEpoch,
    ) -> Option<Lookup<K::Value>> {
        self.run(move |state| state.graph.lookup(&key, version, epoch))
    }

    pub(crate) fn lookup_value<K: Key>(&self, key: K, version: GraphVersion) -> Option<K::Value> {
        self.state.graph.lookup_value(&key, version)
    }

    pub(crate) fn update_state_with_result(&self, changes: Vec<GraphChange>) -> GraphUpdateResult {
        self.run(move |state| {
            let previous_version = state.versions.current();
            let current_version = state.versions.commit_next_if_changed_with(|version| {
                let mut changed = false;
                for change in changes {
                    changed |= change.apply(&state.graph, version);
                }
                changed
            });
            let changed = current_version != previous_version;
            if changed {
                state.graph.prune_history(state.versions.retention_floor());
            }
            GraphUpdateResult {
                previous_version,
                current_version,
                changed,
            }
        })
    }

    pub(crate) fn deps_unchanged_since(
        &self,
        deps: Vec<AnyKey>,
        verified_from: GraphVersion,
        version: GraphVersion,
    ) -> bool {
        self.run(move |state| {
            state
                .graph
                .deps_unchanged_since(&deps, verified_from, version)
        })
    }

    pub(crate) fn mark_unchanged<K: Key>(
        &self,
        key: K,
        value: K::Value,
        deps: RecordedDeps,
        verified_from: GraphVersion,
        version: GraphVersion,
        epoch: VersionEpoch,
    ) -> Result<(), ComputeError> {
        self.run(move |state| {
            if state.versions.is_active(version, epoch) {
                state
                    .graph
                    .mark_unchanged(&key, value, deps, verified_from, version);
                Ok(())
            } else {
                Err(ComputeError::Rejected)
            }
        })
    }

    pub(crate) fn insert_computed_direct<K: Key>(
        &self,
        key: K,
        value: K::Value,
        deps: RecordedDeps,
        version: GraphVersion,
        epoch: VersionEpoch,
    ) -> Result<(), ComputeError> {
        self.run(move |state| {
            if state.versions.is_active(version, epoch) {
                state
                    .graph
                    .insert_computed_direct(&key, value, deps, version);
                Ok(())
            } else {
                Err(ComputeError::Rejected)
            }
        })
    }

    pub(crate) fn insert_completed<K: Key>(
        &self,
        key: K,
        value: K::Value,
        deps: RecordedDeps,
        version: GraphVersion,
        epoch: VersionEpoch,
        generation: SpawnGeneration,
    ) -> Result<(), ComputeError> {
        self.run(move |state| {
            if !state.versions.is_active(version, epoch) {
                return Err(ComputeError::Rejected);
            }
            if state
                .graph
                .insert_completed(&key, value, deps, version, epoch, generation)
            {
                Ok(())
            } else {
                Err(ComputeError::Canceled)
            }
        })
    }

    pub(crate) fn insert_injected<K: InjectedKey>(
        &self,
        key: K,
        value: K::Value,
    ) -> Result<(), UpdateError> {
        let display_key = AnyKey::new(key.clone()).to_string();
        self.run(move |state| {
            if state
                .graph
                .insert_injected(&key, value, state.versions.current())
            {
                Ok(())
            } else {
                Err(UpdateError::InjectedKeyAlreadySet { key: display_key })
            }
        })
    }

    pub(crate) fn snapshot_records(&self) -> Vec<NodeRecord> {
        self.run(|state| {
            let mut records = Vec::new();
            state
                .graph
                .for_each_slot(|slot| slot.snapshot(&mut records));
            records
        })
    }

    pub(crate) fn stats(&self) -> GraphOwnerStats {
        self.run(|state| GraphOwnerStats {
            versions: state.versions.stats(),
            graph: state.graph.stats(),
        })
    }

    fn run<R>(&self, op: impl FnOnce(&ComputeState) -> R + Send + 'static) -> R
    where
        R: Send + 'static,
    {
        let (response_tx, response_rx) = mpsc::sync_channel(1);
        let job = Box::new(move |state: &ComputeState| {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| op(state)));
            let _ = response_tx.send(result);
        });
        self.tx
            .send(job)
            .expect("compute graph owner thread stopped");
        match response_rx
            .recv()
            .expect("compute graph owner dropped a response")
        {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

impl Default for GraphOwner {
    fn default() -> Self {
        Self::new()
    }
}
