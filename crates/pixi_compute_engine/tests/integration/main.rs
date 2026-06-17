//! Integration tests for `pixi_compute_engine`, grouped into submodules that
//! all compile into a single test binary.

mod basic;
mod cancellation;
mod combinators;
mod common;
mod cycle;
mod data_store;
mod demand;
mod executor;
mod graph_owner;
mod injected;
mod introspection;
mod invalidation;
mod misc;
mod snapshot;
mod spawn_hook;
mod stats;
mod structured_deps;
