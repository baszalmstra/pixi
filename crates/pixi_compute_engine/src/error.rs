//! Framework-level errors returned by
//! [`ComputeEngine::compute`](crate::ComputeEngine::compute).
//!
//! User-level errors live inside [`Key::Value`](crate::Key::Value);
//! this enum carries only the framework's own failure modes.
//!
//! # Cycles
//!
//! A detected cycle is first offered to every
//! [`ComputeCtx::with_cycle_guard`](crate::ComputeCtx::with_cycle_guard)
//! scope that sits on the cycle path, as a
//! [`CycleError`]. If no user guard catches, the
//! cycle surfaces at the engine boundary as [`ComputeError::Cycle`],
//! carrying the full ring of keys.

use crate::CycleError;

/// An error returned by
/// [`ComputeEngine::compute`](crate::ComputeEngine::compute).
///
/// This enum carries only *framework*-level failure modes that remain
/// meaningful at the engine boundary. A Key's own compute body calls
/// [`ComputeCtx::compute`](crate::ComputeCtx::compute), which returns
/// the child's [`Value`](crate::Key::Value) directly (no `Result`).
/// User-level failures live inside that `Value`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ComputeError {
    /// The underlying spawned task was aborted before it could produce
    /// a value.
    ///
    /// This happens when every subscriber to an in-flight compute
    /// drops before the compute finishes: the engine cancels the task
    /// because nobody is waiting for the value. A request that
    /// re-arrives later will spawn a fresh compute.
    #[error("compute was canceled")]
    Canceled,

    /// The snapshot's graph version is no longer accepted by the engine.
    #[error("compute graph version was rejected")]
    Rejected,

    /// A dependency cycle was detected that no
    /// [`ComputeCtx::with_cycle_guard`](crate::ComputeCtx::with_cycle_guard)
    /// scope caught.
    ///
    /// The wrapped [`CycleError`]'s `path` lists the distinct keys
    /// on the cycle in order: `[caller, target, ...]`, where the
    /// closing edge is from the last entry back to the first.
    #[error("compute cycle detected: {0}")]
    Cycle(CycleError),
}

/// An error returned while preparing or committing graph updates.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    /// The same key appeared more than once in a single update transaction.
    #[error("duplicate graph update for key {key}")]
    DuplicateKey { key: String },

    /// Injected keys must be replaced with a value, not invalidated.
    #[error("injected key cannot be invalidated without a replacement value: {key}")]
    InjectedKeyInvalidated { key: String },

    /// The initial injected value was already provided for this key.
    #[error("injected key already set: {key}")]
    InjectedKeyAlreadySet { key: String },

    /// An external update source failed while preparing graph updates.
    #[error("external update failed: {message}")]
    External { message: String },
}
