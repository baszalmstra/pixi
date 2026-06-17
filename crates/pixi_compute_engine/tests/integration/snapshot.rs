use std::sync::atomic::Ordering;

use derive_more::Display;
use pixi_compute_engine::{ComputeCtx, ComputeEngine, Key};

use crate::common::{DoubleKey, test_counter};

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("version")]
struct VersionKey;

impl Key for VersionKey {
    type Value = u64;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        ctx.graph_version().as_u64()
    }
}

#[tokio::test]
async fn snapshot_compute_matches_engine_compute() {
    let engine = ComputeEngine::builder().with_data(test_counter()).build();
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.compute(&DoubleKey { id: 21 }).await.unwrap(), 42);
    assert_eq!(engine.compute(&DoubleKey { id: 21 }).await.unwrap(), 42);
}

#[tokio::test]
async fn snapshot_and_engine_share_completed_cache() {
    let counter = test_counter();
    let calls = counter.0.clone();
    let engine = ComputeEngine::builder().with_data(counter).build();
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.compute(&DoubleKey { id: 11 }).await.unwrap(), 22);
    assert_eq!(engine.compute(&DoubleKey { id: 11 }).await.unwrap(), 22);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn snapshot_keeps_engine_state_alive() {
    let engine = ComputeEngine::new();
    let snapshot = engine.snapshot();
    drop(engine);

    assert_eq!(
        snapshot.compute(&VersionKey).await.unwrap(),
        snapshot.version().as_u64()
    );
}

#[tokio::test]
async fn compute_ctx_reports_snapshot_version() {
    let engine = ComputeEngine::new();
    let snapshot = engine.snapshot();

    assert_eq!(
        snapshot.compute(&VersionKey).await.unwrap(),
        snapshot.version().as_u64()
    );
}

#[test]
fn cloned_snapshots_share_version() {
    let engine = ComputeEngine::new();
    let snapshot = engine.snapshot();
    let clone = snapshot.clone();

    assert_eq!(snapshot.version(), clone.version());
    assert_eq!(snapshot.version(), engine.current_version());
}
