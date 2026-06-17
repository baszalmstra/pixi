//! Tests for structured dependency validation.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use derive_more::Display;
use pixi_compute_engine::{ComputeCtx, ComputeEngine, DataStore, Key};
use tokio::sync::Notify;

struct ParallelDepsData {
    park_recomputes: AtomicBool,
    starts: [AtomicUsize; 2],
    started: Notify,
    release: Notify,
}

trait HasParallelDepsData {
    fn parallel_deps_data(&self) -> &Arc<ParallelDepsData>;
}

impl HasParallelDepsData for DataStore {
    fn parallel_deps_data(&self) -> &Arc<ParallelDepsData> {
        self.get::<Arc<ParallelDepsData>>()
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("ParallelLeaf({_0})")]
struct ParallelLeaf(usize);

impl Key for ParallelLeaf {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let data = ctx.global_data().parallel_deps_data();
        data.starts[self.0].fetch_add(1, Ordering::SeqCst);
        data.started.notify_waiters();
        if data.park_recomputes.load(Ordering::SeqCst) {
            data.release.notified().await;
        }
        1
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

#[derive(Clone, Debug, Display, Hash, PartialEq, Eq)]
#[display("ParallelParent")]
struct ParallelParent;

impl Key for ParallelParent {
    type Value = usize;

    async fn compute(&self, ctx: &mut ComputeCtx) -> Self::Value {
        let (a, b) = ctx
            .compute2(
                async |ctx| ctx.compute(&ParallelLeaf(0)).await,
                async |ctx| ctx.compute(&ParallelLeaf(1)).await,
            )
            .await;
        a + b
    }

    fn equality(a: &Self::Value, b: &Self::Value) -> bool {
        a == b
    }
}

async fn wait_for_starts(data: &ParallelDepsData, expected_each: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if data.starts[0].load(Ordering::SeqCst) >= expected_each
                && data.starts[1].load(Ordering::SeqCst) >= expected_each
            {
                break;
            }
            data.started.notified().await;
        }
    })
    .await
    .expect("both parallel deps did not start checking");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_deps_validates_parallel_dep_branches_concurrently() {
    let data = Arc::new(ParallelDepsData {
        park_recomputes: AtomicBool::new(false),
        starts: [AtomicUsize::new(0), AtomicUsize::new(0)],
        started: Notify::new(),
        release: Notify::new(),
    });
    let engine = ComputeEngine::builder().with_data(data.clone()).build();

    assert_eq!(engine.compute(&ParallelParent).await.unwrap(), 2);
    assert_eq!(data.starts[0].load(Ordering::SeqCst), 1);
    assert_eq!(data.starts[1].load(Ordering::SeqCst), 1);

    data.park_recomputes.store(true, Ordering::SeqCst);
    engine.invalidate(&ParallelLeaf(0)).unwrap();
    engine.invalidate(&ParallelLeaf(1)).unwrap();

    let compute = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.compute(&ParallelParent).await.unwrap() })
    };

    wait_for_starts(&data, 2).await;
    data.release.notify_waiters();
    assert_eq!(compute.await.unwrap(), 2);
}
