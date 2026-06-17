//! Tests for daemon-facing compute-engine stats.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use derive_more::Display;
use futures::FutureExt;
use pixi_compute_engine::{
    ComputeCtx, ComputeEngine, DataStore, DependencyGraph, InjectedKey, Key,
};
use tokio::sync::Notify;

use super::common::poll_once;

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("StatsCountingKey")]
struct StatsCountingKey;

impl Key for StatsCountingKey {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.global_data()
            .get::<Arc<AtomicUsize>>()
            .fetch_add(1, Ordering::SeqCst)
            + 1
    }
}

#[test]
fn stats_track_active_snapshot_versions() {
    let engine = ComputeEngine::new();
    let snapshot = engine.snapshot();

    let stats = engine.stats();
    assert_eq!(stats.current_version.as_u64(), 0);
    assert_eq!(stats.active_versions, 1);
    assert_eq!(stats.active_leases, 1);
    assert_eq!(stats.node_count, 0);

    drop(snapshot);

    let stats = engine.stats();
    assert_eq!(stats.active_versions, 0);
    assert_eq!(stats.active_leases, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn stats_track_vacant_dirty_metadata_for_unseen_invalidations() {
    let counter = Arc::new(AtomicUsize::new(0));
    let engine = ComputeEngine::builder().with_data(counter.clone()).build();

    let committed = engine.invalidate(&StatsCountingKey).unwrap();
    assert_eq!(committed.as_u64(), 1);

    let graph = DependencyGraph::from_engine(&engine);
    assert_eq!(graph.len(), 0);

    let stats = engine.stats();
    assert_eq!(stats.current_version, committed);
    assert_eq!(stats.interned_keys, 1);
    assert_eq!(stats.node_count, 0);
    assert_eq!(stats.dirty_keys, 1);
    assert_eq!(stats.dirty_entries, 1);

    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 1);

    let stats = engine.stats();
    assert_eq!(stats.node_count, 1);
    assert_eq!(stats.completed_nodes, 1);
    assert_eq!(stats.in_flight_count, 0);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

struct StatsParkedData {
    started: Arc<Notify>,
    dropped: Arc<Notify>,
}

trait HasStatsParkedData {
    fn stats_parked_data(&self) -> &Arc<StatsParkedData>;
}

impl HasStatsParkedData for DataStore {
    fn stats_parked_data(&self) -> &Arc<StatsParkedData> {
        self.get::<Arc<StatsParkedData>>()
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("StatsParkedKey")]
struct StatsParkedKey;

impl Key for StatsParkedKey {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        struct DropNotify(Arc<Notify>);
        impl Drop for DropNotify {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }

        let data = ctx.global_data().stats_parked_data();
        let _drop = DropNotify(data.dropped.clone());
        data.started.notify_one();
        std::future::pending::<()>().await;
        1
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stats_drop_abandoned_in_flight_entries_when_last_lease_drops() {
    let data = Arc::new(StatsParkedData {
        started: Arc::new(Notify::new()),
        dropped: Arc::new(Notify::new()),
    });
    let engine = ComputeEngine::builder().with_data(data.clone()).build();

    let mut compute = engine.compute(&StatsParkedKey).boxed();
    poll_once(&mut compute).await;
    data.started.notified().await;

    let stats = engine.stats();
    assert_eq!(stats.active_leases, 1);
    assert_eq!(stats.in_flight_count, 1);
    assert_eq!(stats.stale_in_flight_count, 0);

    drop(compute);
    data.dropped.notified().await;

    let stats = engine.stats();
    assert_eq!(stats.active_leases, 0);
    assert_eq!(stats.in_flight_count, 0);
    assert_eq!(stats.stale_in_flight_count, 0);
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("StatsBumpKey")]
struct StatsBumpKey;

impl Key for StatsBumpKey {
    type Value = usize;

    async fn compute(&self, _ctx: &mut ComputeCtx) -> Self::Value {
        0
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completed_version_history_is_pruned_after_old_snapshot_release() {
    let counter = Arc::new(AtomicUsize::new(0));
    let engine = ComputeEngine::builder().with_data(counter.clone()).build();

    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 1);
    let held = engine.snapshot();
    assert_eq!(held.compute(&StatsCountingKey).await.unwrap(), 1);

    engine.invalidate(&StatsCountingKey).unwrap();
    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 2);
    engine.invalidate(&StatsCountingKey).unwrap();
    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 3);

    let stats = engine.stats();
    assert_eq!(stats.completed_nodes, 1);
    assert_eq!(stats.completed_versions, 3);
    assert_eq!(held.compute(&StatsCountingKey).await.unwrap(), 1);

    drop(held);

    let stats = engine.stats();
    assert_eq!(stats.completed_nodes, 1);
    assert_eq!(stats.completed_versions, 1);
    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 3);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn pruning_keeps_covering_value_for_active_middle_snapshot() {
    let counter = Arc::new(AtomicUsize::new(0));
    let engine = ComputeEngine::builder().with_data(counter.clone()).build();

    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 1);
    engine.invalidate(&StatsBumpKey).unwrap();
    assert_eq!(engine.current_version().as_u64(), 1);

    let middle = engine.snapshot();
    assert_eq!(middle.version().as_u64(), 1);
    assert_eq!(middle.compute(&StatsCountingKey).await.unwrap(), 1);

    engine.invalidate(&StatsCountingKey).unwrap();
    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 2);

    let stats = engine.stats();
    assert_eq!(stats.completed_versions, 2);
    assert_eq!(middle.compute(&StatsCountingKey).await.unwrap(), 1);

    drop(middle);

    let stats = engine.stats();
    assert_eq!(stats.completed_versions, 1);
    assert_eq!(engine.compute(&StatsCountingKey).await.unwrap(), 2);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("StatsInjectedKey")]
struct StatsInjectedKey;

impl InjectedKey for StatsInjectedKey {
    type Value = usize;

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[tokio::test(flavor = "current_thread")]
async fn injected_version_history_is_pruned_after_old_snapshot_release() {
    let engine = ComputeEngine::new();
    engine.inject(StatsInjectedKey, 1);
    let held = engine.snapshot();
    assert_eq!(held.compute(&StatsInjectedKey).await.unwrap(), 1);

    engine.re_inject(StatsInjectedKey, 2).unwrap();
    engine.re_inject(StatsInjectedKey, 3).unwrap();

    let stats = engine.stats();
    assert_eq!(stats.injected_nodes, 1);
    assert_eq!(stats.injected_versions, 3);
    assert_eq!(held.compute(&StatsInjectedKey).await.unwrap(), 1);

    drop(held);

    let stats = engine.stats();
    assert_eq!(stats.injected_nodes, 1);
    assert_eq!(stats.injected_versions, 1);
    assert_eq!(engine.compute(&StatsInjectedKey).await.unwrap(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_pre_reset_snapshots_do_not_retain_post_reset_history() {
    let engine = ComputeEngine::new();
    let rejected = engine.snapshot();

    engine.unstable_drop_everything();
    engine.try_inject(StatsInjectedKey, 1).unwrap();
    engine.re_inject(StatsInjectedKey, 2).unwrap();
    engine.re_inject(StatsInjectedKey, 3).unwrap();

    let stats = engine.stats();
    assert_eq!(stats.active_leases, 1);
    assert_eq!(stats.injected_versions, 1);

    assert!(rejected.compute(&StatsInjectedKey).await.is_err());
    assert_eq!(engine.compute(&StatsInjectedKey).await.unwrap(), 3);
}
