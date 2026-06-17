//! Regression tests for serialized compute-graph ownership.

use std::panic::{self, AssertUnwindSafe};

use derive_more::Display;
use pixi_compute_engine::{ComputeEngine, InjectedKey};

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("{_0}")]
struct GraphOwnerParam(u32);

impl InjectedKey for GraphOwnerParam {
    type Value = u64;
}

#[tokio::test(flavor = "current_thread")]
async fn graph_owner_stays_usable_after_lookup_panic() {
    let engine = ComputeEngine::new();

    let missing_injected = tokio::spawn({
        let engine = engine.clone();
        async move {
            let _ = engine.compute(&GraphOwnerParam(1)).await;
        }
    })
    .await;
    assert!(missing_injected.unwrap_err().is_panic());

    engine.inject(GraphOwnerParam(2), 20);
    assert_eq!(engine.read(&GraphOwnerParam(2)), Some(20));
    assert_eq!(engine.compute(&GraphOwnerParam(2)).await.unwrap(), 20);
}

#[tokio::test(flavor = "current_thread")]
async fn graph_owner_stays_usable_after_insert_panic() {
    let engine = ComputeEngine::new();
    engine.inject(GraphOwnerParam(1), 10);

    let duplicate_inject = panic::catch_unwind(AssertUnwindSafe(|| {
        engine.inject(GraphOwnerParam(1), 11);
    }));
    assert!(duplicate_inject.is_err());

    engine.inject(GraphOwnerParam(2), 20);
    assert_eq!(engine.read(&GraphOwnerParam(1)), Some(10));
    assert_eq!(engine.read(&GraphOwnerParam(2)), Some(20));
}
