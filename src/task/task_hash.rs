use crate::task::FileHashes;
use std::hash::Hasher;
use xxhash_rust::xxh3::Xxh3;

#[derive(Debug)]
pub struct TaskHash {
    pub name: String,
    pub inputs: FileHashes,
}

impl TaskHash {
    /// Computes a single hash for the task.
    pub fn hash(&self) -> String {
        let mut hasher = Xxh3::new();
        hasher.update(self.name.as_bytes());
        for (path, hash) in &self.inputs.files {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update(hash.as_bytes());
        }
        format!("{:x}", hasher.finish())
    }
}
