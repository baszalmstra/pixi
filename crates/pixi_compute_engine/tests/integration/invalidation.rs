//! Tests for graph-version dirtying APIs.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use derive_more::Display;
use pixi_compute_engine::{
    ComputeCtx, ComputeEngine, ComputeError, DataStore, InjectedKey, Key, UpdateError,
};
use tokio::sync::Notify;

struct CounterData(AtomicUsize);

struct CascadeData {
    leaf: AtomicUsize,
    parent: AtomicUsize,
}

trait HasCounterData {
    fn counter_data(&self) -> &Arc<CounterData>;
}

impl HasCounterData for DataStore {
    fn counter_data(&self) -> &Arc<CounterData> {
        self.get::<Arc<CounterData>>()
    }
}

trait HasCascadeData {
    fn cascade_data(&self) -> &Arc<CascadeData>;
}

impl HasCascadeData for DataStore {
    fn cascade_data(&self) -> &Arc<CascadeData> {
        self.get::<Arc<CascadeData>>()
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("CountingKey")]
struct CountingKey;

impl Key for CountingKey {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.global_data()
            .counter_data()
            .0
            .fetch_add(1, Ordering::SeqCst)
            + 1
    }
}

#[tokio::test(flavor = "current_thread")]
async fn invalidate_recomputes_at_next_version_and_preserves_held_snapshot() {
    let counter = Arc::new(CounterData(AtomicUsize::new(0)));
    let engine = ComputeEngine::builder().with_data(counter.clone()).build();

    let held_snapshot = engine.snapshot();
    assert_eq!(held_snapshot.compute(&CountingKey).await.unwrap(), 1);

    let committed = engine.invalidate(&CountingKey).unwrap();
    assert_eq!(committed.as_u64(), 1);

    let current_snapshot = engine.snapshot();
    assert_eq!(current_snapshot.version(), committed);
    assert_eq!(current_snapshot.compute(&CountingKey).await.unwrap(), 2);
    assert_eq!(held_snapshot.compute(&CountingKey).await.unwrap(), 1);
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("LeafKey")]
struct LeafKey;

impl Key for LeafKey {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.global_data()
            .cascade_data()
            .leaf
            .fetch_add(1, Ordering::SeqCst)
            + 1
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("ParentKey")]
struct ParentKey;

impl Key for ParentKey {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let leaf = ctx.compute(&LeafKey).await;
        let parent = ctx
            .global_data()
            .cascade_data()
            .parent
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        leaf * 10 + parent
    }
}

#[tokio::test(flavor = "current_thread")]
async fn invalidate_dirties_transitive_dependents() {
    let data = Arc::new(CascadeData {
        leaf: AtomicUsize::new(0),
        parent: AtomicUsize::new(0),
    });
    let engine = ComputeEngine::builder().with_data(data.clone()).build();

    let held_snapshot = engine.snapshot();
    assert_eq!(held_snapshot.compute(&ParentKey).await.unwrap(), 11);

    let committed = engine.invalidate(&LeafKey).unwrap();
    assert_eq!(committed.as_u64(), 1);

    let current_snapshot = engine.snapshot();
    assert_eq!(current_snapshot.compute(&ParentKey).await.unwrap(), 22);
    assert_eq!(held_snapshot.compute(&ParentKey).await.unwrap(), 11);
    assert_eq!(data.leaf.load(Ordering::SeqCst), 2);
    assert_eq!(data.parent.load(Ordering::SeqCst), 2);
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("DynamicParam")]
struct DynamicParam;

impl InjectedKey for DynamicParam {
    type Value = usize;

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("DynamicReader")]
struct DynamicReader;

impl Key for DynamicReader {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.compute(&DynamicParam).await * 10
    }
}

#[tokio::test(flavor = "current_thread")]
async fn re_inject_versions_injected_value_and_dirties_dependents() {
    let engine = ComputeEngine::new();
    engine.inject(DynamicParam, 1);

    let held_snapshot = engine.snapshot();
    assert_eq!(held_snapshot.compute(&DynamicReader).await.unwrap(), 10);
    assert_eq!(held_snapshot.compute(&DynamicParam).await.unwrap(), 1);

    let committed = engine.re_inject(DynamicParam, 2).unwrap();
    assert_eq!(committed.as_u64(), 1);

    let current_snapshot = engine.snapshot();
    assert_eq!(current_snapshot.compute(&DynamicParam).await.unwrap(), 2);
    assert_eq!(current_snapshot.compute(&DynamicReader).await.unwrap(), 20);
    assert_eq!(held_snapshot.compute(&DynamicParam).await.unwrap(), 1);
    assert_eq!(held_snapshot.compute(&DynamicReader).await.unwrap(), 10);
}

#[tokio::test(flavor = "current_thread")]
async fn re_inject_equal_value_does_not_commit_new_version() {
    let engine = ComputeEngine::new();
    engine.inject(DynamicParam, 1);
    assert_eq!(engine.current_version().as_u64(), 0);

    let committed = engine.re_inject(DynamicParam, 1).unwrap();
    assert_eq!(committed.as_u64(), 0);
    assert_eq!(engine.current_version().as_u64(), 0);
    assert_eq!(engine.compute(&DynamicParam).await.unwrap(), 1);
}

struct EqualLeafData {
    leaf: AtomicUsize,
    parent: AtomicUsize,
}

trait HasEqualLeafData {
    fn equal_leaf_data(&self) -> &Arc<EqualLeafData>;
}

impl HasEqualLeafData for DataStore {
    fn equal_leaf_data(&self) -> &Arc<EqualLeafData> {
        self.get::<Arc<EqualLeafData>>()
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("EqualLeaf")]
struct EqualLeaf;

impl Key for EqualLeaf {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.global_data()
            .equal_leaf_data()
            .leaf
            .fetch_add(1, Ordering::SeqCst);
        1
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("EqualParent")]
struct EqualParent;

impl Key for EqualParent {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.global_data()
            .equal_leaf_data()
            .parent
            .fetch_add(1, Ordering::SeqCst);
        ctx.compute(&EqualLeaf).await * 10
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dirty_dependent_reuses_value_when_dependency_recomputes_equal() {
    let data = Arc::new(EqualLeafData {
        leaf: AtomicUsize::new(0),
        parent: AtomicUsize::new(0),
    });
    let engine = ComputeEngine::builder().with_data(data.clone()).build();

    assert_eq!(engine.compute(&EqualParent).await.unwrap(), 10);
    assert_eq!(data.leaf.load(Ordering::SeqCst), 1);
    assert_eq!(data.parent.load(Ordering::SeqCst), 1);

    let committed = engine.invalidate(&EqualLeaf).unwrap();
    assert_eq!(committed.as_u64(), 1);

    assert_eq!(engine.compute(&EqualParent).await.unwrap(), 10);
    assert_eq!(data.leaf.load(Ordering::SeqCst), 2);
    assert_eq!(data.parent.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn updater_batches_mixed_changes_into_one_version() {
    let counter = Arc::new(CounterData(AtomicUsize::new(0)));
    let engine = ComputeEngine::builder().with_data(counter.clone()).build();
    engine.inject(DynamicParam, 1);

    let held_snapshot = engine.snapshot();
    assert_eq!(held_snapshot.compute(&CountingKey).await.unwrap(), 1);
    assert_eq!(held_snapshot.compute(&DynamicReader).await.unwrap(), 10);

    let mut updater = engine.updater();
    updater.changed(CountingKey).unwrap();
    updater.changed_to(DynamicParam, 2).unwrap();
    let committed = updater.commit().unwrap();
    assert_eq!(committed.as_u64(), 1);
    assert_eq!(engine.current_version(), committed);

    assert_eq!(engine.compute(&CountingKey).await.unwrap(), 2);
    assert_eq!(engine.compute(&DynamicReader).await.unwrap(), 20);
    assert_eq!(held_snapshot.compute(&CountingKey).await.unwrap(), 1);
    assert_eq!(held_snapshot.compute(&DynamicReader).await.unwrap(), 10);
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn updater_noops_do_not_commit_new_version() {
    let engine = ComputeEngine::new();
    engine.inject(DynamicParam, 1);

    let mut updater = engine.updater();
    updater.changed_to(DynamicParam, 1).unwrap();
    let committed = updater.commit().unwrap();

    assert_eq!(committed.as_u64(), 0);
    assert_eq!(engine.current_version().as_u64(), 0);
    assert_eq!(engine.compute(&DynamicParam).await.unwrap(), 1);
}

#[test]
fn updater_commit_report_distinguishes_changed_from_noop_batches() {
    let engine = ComputeEngine::new();
    engine.inject(DynamicParam, 1);

    let mut noop = engine.updater();
    noop.changed_to(DynamicParam, 1).unwrap();
    let report = noop.commit_report().unwrap();
    assert_eq!(report.previous_version.as_u64(), 0);
    assert_eq!(report.current_version.as_u64(), 0);
    assert!(!report.changed);
    assert_eq!(report.change_count, 1);

    let mut changed = engine.updater();
    changed.changed_to(DynamicParam, 2).unwrap();
    let report = changed.commit_report().unwrap();
    assert_eq!(report.previous_version.as_u64(), 0);
    assert_eq!(report.current_version.as_u64(), 1);
    assert!(report.changed);
    assert_eq!(report.change_count, 1);
}

#[test]
fn updater_rejects_duplicate_key_changes_without_committing() {
    let engine = ComputeEngine::new();
    let mut updater = engine.updater();
    updater.changed(CountingKey).unwrap();

    let err = updater.changed(CountingKey).unwrap_err();
    assert_eq!(
        err,
        UpdateError::DuplicateKey {
            key: "CountingKey(CountingKey)".to_string()
        }
    );
    assert_eq!(updater.commit().unwrap_err(), err);
    assert_eq!(engine.current_version().as_u64(), 0);
}

#[test]
fn updater_rejects_invalidating_injected_keys_without_panicking() {
    let engine = ComputeEngine::new();
    let mut updater = engine.updater();

    let err = updater.changed(DynamicParam).unwrap_err();
    assert_eq!(
        err,
        UpdateError::InjectedKeyInvalidated {
            key: "DynamicParam(DynamicParam)".to_string()
        }
    );
    assert_eq!(updater.commit().unwrap_err(), err);
    assert_eq!(engine.current_version().as_u64(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn unstable_drop_everything_rejects_held_snapshots_and_clears_graph() {
    let counter = Arc::new(CounterData(AtomicUsize::new(0)));
    let engine = ComputeEngine::builder().with_data(counter.clone()).build();

    let held_snapshot = engine.snapshot();
    assert_eq!(held_snapshot.compute(&CountingKey).await.unwrap(), 1);

    let committed = engine.unstable_drop_everything();
    assert_eq!(committed.as_u64(), 1);
    assert_eq!(engine.current_version(), committed);

    let err = held_snapshot.compute(&CountingKey).await.unwrap_err();
    assert!(matches!(err, ComputeError::Rejected));

    assert_eq!(engine.compute(&CountingKey).await.unwrap(), 2);
    assert_eq!(counter.0.load(Ordering::SeqCst), 2);
}

struct RealUpdateParkedData {
    starts: AtomicUsize,
    started: Notify,
    release: Notify,
}

trait HasRealUpdateParkedData {
    fn real_update_parked_data(&self) -> &Arc<RealUpdateParkedData>;
}

impl HasRealUpdateParkedData for DataStore {
    fn real_update_parked_data(&self) -> &Arc<RealUpdateParkedData> {
        self.get::<Arc<RealUpdateParkedData>>()
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("RealUpdateParkedKey")]
struct RealUpdateParkedKey;

impl Key for RealUpdateParkedKey {
    type Value = u64;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let data = ctx.global_data().real_update_parked_data();
        data.starts.fetch_add(1, Ordering::SeqCst);
        data.started.notify_waiters();
        data.release.notified().await;
        ctx.graph_version().as_u64()
    }
}

async fn wait_for_real_update_starts(data: &RealUpdateParkedData, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while data.starts.load(Ordering::SeqCst) < expected {
            data.started.notified().await;
        }
    })
    .await
    .expect("parked compute did not start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_update_preserves_in_flight_snapshot_compute_and_spawns_current_compute() {
    let data = Arc::new(RealUpdateParkedData {
        starts: AtomicUsize::new(0),
        started: Notify::new(),
        release: Notify::new(),
    });
    let engine = ComputeEngine::builder().with_data(data.clone()).build();

    let old_snapshot = engine.snapshot();
    let old_task = tokio::spawn(async move {
        old_snapshot
            .compute(&RealUpdateParkedKey)
            .await
            .expect("old snapshot compute should complete")
    });
    wait_for_real_update_starts(&data, 1).await;

    let committed = engine.invalidate(&RealUpdateParkedKey).unwrap();
    assert_eq!(committed.as_u64(), 1);

    let current_task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .compute(&RealUpdateParkedKey)
                .await
                .expect("current compute should complete")
        })
    };
    wait_for_real_update_starts(&data, 2).await;

    data.release.notify_waiters();
    assert_eq!(old_task.await.unwrap(), 0);
    assert_eq!(current_task.await.unwrap(), 1);
}

struct ResetParkedData {
    started: Notify,
    release: Notify,
}

trait HasResetParkedData {
    fn reset_parked_data(&self) -> &Arc<ResetParkedData>;
}

impl HasResetParkedData for DataStore {
    fn reset_parked_data(&self) -> &Arc<ResetParkedData> {
        self.get::<Arc<ResetParkedData>>()
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("ResetParkedKey")]
struct ResetParkedKey;

impl Key for ResetParkedKey {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let data = ctx.global_data().reset_parked_data();
        data.started.notify_one();
        data.release.notified().await;
        99
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstable_drop_everything_rejects_in_flight_compute() {
    let data = Arc::new(ResetParkedData {
        started: Notify::new(),
        release: Notify::new(),
    });
    let engine = ComputeEngine::builder().with_data(data.clone()).build();
    let snapshot = engine.snapshot();

    let task = tokio::spawn(async move { snapshot.compute(&ResetParkedKey).await });
    data.started.notified().await;

    let committed = engine.unstable_drop_everything();
    assert_eq!(committed.as_u64(), 1);
    data.release.notify_one();

    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, ComputeError::Rejected));
}

struct DropThreadProbe {
    tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,
}

impl Drop for DropThreadProbe {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let name = std::thread::current()
                .name()
                .unwrap_or("<unnamed>")
                .to_string();
            let _ = tx.send(name);
        }
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("DropThreadKey")]
struct DropThreadKey;

impl Key for DropThreadKey {
    type Value = Arc<DropThreadProbe>;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        Arc::new(DropThreadProbe {
            tx: ctx
                .global_data()
                .get::<Arc<Mutex<Option<mpsc::Sender<String>>>>>()
                .clone(),
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unstable_drop_everything_drops_cached_values_off_owner_thread() {
    let (tx, rx) = mpsc::channel::<String>();
    let drop_sender = Arc::new(Mutex::new(Some(tx)));
    let engine = ComputeEngine::builder().with_data(drop_sender).build();

    let value = engine.compute(&DropThreadKey).await.unwrap();
    drop(value);

    let committed = engine.unstable_drop_everything();
    assert_eq!(committed.as_u64(), 1);

    let drop_thread = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("cached value was not dropped");
    assert_eq!(drop_thread, "pixi-compute-graph-drop");
}
