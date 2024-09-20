use std::{
    convert::identity,
    path::PathBuf,
    sync::{Arc, Weak},
};

use dashmap::{DashMap, Entry};
use pixi_record::{InputHash, InputHashError};
use rattler_digest::Sha256Hash;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct InputHashKey {
    pub root: PathBuf,
    pub globs: Vec<String>,
}

#[derive(Debug)]
enum HashCacheEntry {
    /// The value is currently being computed.
    Pending(Weak<broadcast::Sender<Sha256Hash>>),

    /// We have a value for this key.
    Done(Sha256Hash),
}

/// An object that caches the computation of input hashes. It deduplicates
/// requests for the same hash.
///
/// Its is safe and efficient to use this object from multiple threads.
#[derive(Debug, Default, Clone)]
pub struct InputHashCache {
    cache: Arc<DashMap<InputHashKey, HashCacheEntry>>,
}

impl InputHashCache {
    /// Computes the input hash of the given key. If the hash is already in the
    /// cache, it will return the cached value. If the hash is not in the
    /// cache, it will compute the hash (deduplicating any request) and return
    /// it.
    pub async fn compute_hash(
        &self,
        key: impl Into<InputHashKey>,
    ) -> Result<Sha256Hash, InputHashError> {
        let key = key.into();
        match self.cache.entry(key.clone()) {
            Entry::Vacant(entry) => {
                // Construct a channel over which we will be sending the result and store it in
                // the map. If another requests comes in for the same hash it will find this
                // entry.
                let (tx, _) = broadcast::channel(1);
                let weak_tx = Arc::downgrade(&Arc::new(tx.clone()));
                entry.insert(HashCacheEntry::Pending(weak_tx));

                // Spawn the computation of the hash
                let computation_key = key.clone();
                let result = tokio::task::spawn_blocking(move || {
                    InputHash::from_globs(&computation_key.root, computation_key.globs)
                })
                .await
                .map_or_else(
                    |err| match err.try_into_panic() {
                        Ok(panic) => std::panic::resume_unwind(panic),
                        Err(_) => Err(InputHashError::Cancelled),
                    },
                    identity,
                )?
                .hash;

                // Store the result in the cache
                self.cache.insert(key, HashCacheEntry::Done(result));

                // Broadcast the result, ignore the error. If the receiver is dropped, we don't
                // care.
                let _ = tx.send(result);

                Ok(result)
            }
            Entry::Occupied(entry) => {
                match entry.get() {
                    HashCacheEntry::Pending(weak_tx) => {
                        let sender = weak_tx.clone();
                        let mut subscriber = sender
                            .upgrade()
                            .ok_or(InputHashError::Cancelled)?
                            .subscribe();
                        drop(entry);
                        subscriber
                            .recv()
                            .await
                            .map_err(|_| InputHashError::Cancelled)
                    }
                    HashCacheEntry::Done(hash) => {
                        // We have a value for this key.
                        Ok(*hash)
                    }
                }
            }
        }
    }
}
