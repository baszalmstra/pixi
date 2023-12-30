use crate::task::{ExecutableTask, FileHashes, FileHashesError};
use miette::Diagnostic;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

/// The computation hash is a combined hash of all the inputs and outputs of a task.
///
/// Use a [`TaskHash`] to construct a computation hash.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ComputationHash(String);

impl From<String> for ComputationHash {
    fn from(value: String) -> Self {
        ComputationHash(value)
    }
}

impl ComputationHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ComputationHash {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The [`TaskHash`] group all the hashes of a task. It can be converted to a [`ComputationHash`]
/// with the [`TaskHash::computation_hash`] method.
#[derive(Debug)]
pub struct TaskHash {
    pub command: Option<String>,
    pub inputs: InputHashes,
}

impl TaskHash {
    /// Constructs an instance from an executable task.
    pub async fn from_task(task: &ExecutableTask<'_>) -> Result<Option<Self>, InputHashesError> {
        let Some(inputs) = InputHashes::from_task(task).await? else {
            return Ok(None);
        };

        Ok(Some(Self {
            command: task.full_command(),
            inputs,
        }))
    }

    /// Computes a single hash for the task.
    pub fn computation_hash(&self) -> ComputationHash {
        let mut hasher = Xxh3::new();
        self.command.hash(&mut hasher);
        self.inputs.hash(&mut hasher);
        ComputationHash(format!("{:x}", hasher.finish()))
    }
}

/// The combination of all the hashes of the inputs of a task.
///
/// TODO: We should add more inputs besides files, e.g. environment variables, dependencies, etc.
#[derive(Debug, Hash)]
pub struct InputHashes {
    pub files: FileHashes,
}

impl InputHashes {
    /// Compute the input hashes from a task.
    pub async fn from_task(task: &ExecutableTask<'_>) -> Result<Option<Self>, InputHashesError> {
        let Some(cache) = task.task.cache() else {
            return Ok(None);
        };
        let Some(inputs) = cache.inputs.as_deref() else {
            return Ok(None);
        };
        let files = FileHashes::from_files(task.project.root(), inputs.iter()).await?;
        Ok(Some(Self { files }))
    }
}

/// An error that might occur when computing the input hashes of a task.
#[derive(Debug, Error, Diagnostic)]
pub enum InputHashesError {
    #[error(transparent)]
    FileHashes(#[from] FileHashesError),
}
