use miette::Diagnostic;
use pixi_core::environment::{EnvironmentHash, LockedEnvironmentHash};
use pixi_manifest::task::TemplateStringError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use thiserror::Error;
use xxhash_rust::xxh3::Xxh3;

use crate::{ExecutableTask, FileHashes, FileHashesError, InvalidWorkingDirectory};

/// The computation hash is a combined hash of all the inputs and outputs of a task.
///
/// Use a [`TaskHash`] to construct a computation hash.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct ComputationHash(String);

impl From<String> for ComputationHash {
    fn from(value: String) -> Self {
        ComputationHash(value)
    }
}

impl Display for ComputationHash {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The name hash is a combined hash of all the inputs and outputs of a task.
/// and it's used as a name for the task cache file.
///
/// Use a [`TaskHash`] to construct a name hash.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct NameHash(String);

impl From<String> for NameHash {
    fn from(value: String) -> Self {
        NameHash(value)
    }
}

impl From<&dyn Hasher> for NameHash {
    fn from(hasher: &dyn Hasher) -> Self {
        NameHash(format!("{:x}", hasher.finish()))
    }
}

impl Display for NameHash {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The cache of a task. It contains the hash of the task.
#[derive(Deserialize, Serialize, Debug)]
pub struct TaskCache {
    /// The hash of the task.
    pub hash: ComputationHash,
}

/// The [`TaskHash`] group all the hashes of a task. It can be converted to a [`ComputationHash`]
/// with the [`TaskHash::computation_hash`] method.
#[derive(Debug)]
pub struct TaskHash {
    /// Key over the prefix's installed conda content and the activation
    /// inputs, see [`TaskHash::environment_hashes`].
    pub environment: EnvironmentHash,
    /// Digest of the locked packages (conda *and* pypi) the prefix was last
    /// installed from, read from the `conda-meta/pixi` marker. Covers what
    /// the conda install fingerprint cannot see: pypi packages leave no
    /// trace there, but their locked URLs are folded into this digest at
    /// install time. `None` when the marker was never recorded.
    pub locked_environment: Option<LockedEnvironmentHash>,
    pub command: Option<String>,
    pub inputs: Option<InputHashes>,
    pub outputs: Option<OutputHashes>,
}

impl TaskHash {
    /// Constructs an instance from an executable task.
    ///
    /// Returns `Ok(None)` when no cache key can be derived because the
    /// environment has no completed-install fingerprint (see
    /// [`Self::environment_hashes`]), or when the task declares no inputs
    /// or outputs (there is nothing to cache) — in both cases task caching
    /// disengages.
    pub async fn from_task(task: &ExecutableTask<'_>) -> Result<Option<Self>, InputHashesError> {
        // Probe the cheap marker-derived key components first: without them
        // there is no cache key, so skip the (potentially expensive) file
        // hashing entirely.
        let Some((environment, locked_environment)) = Self::environment_hashes(task) else {
            return Ok(None);
        };

        let input_hashes = InputHashes::from_task(task).await?;
        let output_hashes = OutputHashes::from_task(task).await?;

        if input_hashes.is_none() && output_hashes.is_none() {
            return Ok(None);
        }

        Ok(Some(Self {
            command: task.full_command().ok().flatten(),
            outputs: output_hashes,
            inputs: input_hashes,
            environment,
            locked_environment,
        }))
    }

    /// The environment-derived components of the task-cache key, read from
    /// the markers a completed install leaves next to the prefix (no lock
    /// file needed):
    ///
    /// - an [`EnvironmentHash`] keyed on the install fingerprint
    ///   ([`pixi_utils::EnvironmentFingerprint`]), which follows the
    ///   *installed* conda content — so a source rebuild invalidates the
    ///   cache while lock-file churn that doesn't change the prefix does
    ///   not;
    /// - the [`LockedEnvironmentHash`] recorded in `conda-meta/pixi`, which
    ///   also covers pypi packages (invisible to the conda fingerprint).
    ///
    /// Returns `None` when no completed install recorded a fingerprint
    /// (never installed, interrupted install, or `--no-install` runs);
    /// callers treat that as "task caching disengaged".
    pub(crate) fn environment_hashes(
        task: &ExecutableTask<'_>,
    ) -> Option<(EnvironmentHash, Option<LockedEnvironmentHash>)> {
        let fingerprint = pixi_utils::EnvironmentFingerprint::read(&task.run_environment.dir())?;
        let environment = EnvironmentHash::for_activation(
            &task.run_environment,
            // Environment variables are deliberately not part of the task
            // cache key.
            &HashMap::new(),
            &fingerprint,
        );
        Some((environment, task.run_environment.installed_lock_file_hash()))
    }

    pub async fn update_output(
        &mut self,
        task: &ExecutableTask<'_>,
    ) -> Result<(), InputHashesError> {
        self.outputs = OutputHashes::from_task(task).await?;
        Ok(())
    }

    pub async fn update_input(
        &mut self,
        task: &ExecutableTask<'_>,
    ) -> Result<(), InputHashesError> {
        self.inputs = InputHashes::from_task(task).await?;
        Ok(())
    }

    /// Computes a single hash for the task.
    pub fn computation_hash(&self) -> ComputationHash {
        let mut hasher = Xxh3::new();
        self.command.hash(&mut hasher);
        self.inputs.hash(&mut hasher);
        self.outputs.hash(&mut hasher);
        self.environment.hash(&mut hasher);
        self.locked_environment.hash(&mut hasher);
        ComputationHash(format!("{:x}", hasher.finish()))
    }

    /// Return the hash that should be used as the name of the task cache file.
    /// It takes the rendered inputs and rendered outputs of the task into account.
    pub fn task_args_hash(task: &ExecutableTask<'_>) -> Result<Option<NameHash>, InputHashesError> {
        let mut hasher = Xxh3::new();

        let Ok(execute) = task.task().as_execute() else {
            return Ok(None);
        };

        // Initialize the hasher state with the task args
        task.args().hash(&mut hasher);

        // We need to compute hash from input args
        // If no input args are provided, we treat them as empty list.
        let context = task.render_context();
        if let Some(ref inputs) = execute.inputs {
            let rendered_inputs = inputs.render(&context)?;
            rendered_inputs.hash(&mut hasher);
        }

        // and the same for output args
        if let Some(ref outputs) = execute.outputs {
            let rendered_outputs = outputs.render(&context)?;
            rendered_outputs.hash(&mut hasher);
        }

        // Create a namehash from the hasher
        Ok(Some(NameHash::from(&hasher as &dyn Hasher)))
    }
}

/// The combination of all the hashes of the inputs of a task.
#[derive(Debug, Hash)]
pub struct InputHashes {
    pub files: FileHashes,
}

impl InputHashes {
    /// Compute the input hashes from a task. Returns `None` if no files match.
    pub async fn from_task(task: &ExecutableTask<'_>) -> Result<Option<Self>, InputHashesError> {
        let Ok(execute) = task.task().as_execute() else {
            return Ok(None);
        };

        let Some(inputs) = &execute.inputs else {
            return Ok(None);
        };

        if inputs.is_empty() {
            return Ok(None);
        }

        let context = task.render_context();
        let rendered_inputs: Vec<String> = inputs
            .iter()
            .map(|i| i.render(&context))
            .collect::<Result<_, _>>()?;

        let files = FileHashes::from_files(task.project().root(), &rendered_inputs).await?;

        // If no files matched, treat as no inputs for caching purposes
        if files.files.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self { files }))
    }
}

/// The combination of all the hashes of the inputs of a task.
#[derive(Debug, Hash)]
pub struct OutputHashes {
    pub files: FileHashes,
}

impl OutputHashes {
    /// Compute the output hashes from a task. Returns `None` if no files match.
    pub async fn from_task(task: &ExecutableTask<'_>) -> Result<Option<Self>, InputHashesError> {
        let outputs: Vec<String> = match task.task().as_execute() {
            Ok(execute) => {
                if let Some(outputs) = execute.outputs.clone() {
                    let context = task.render_context();
                    let mut rendered_outputs = Vec::new();
                    for output in outputs.iter() {
                        match output.render(&context) {
                            Ok(rendered) => rendered_outputs.push(rendered),
                            Err(err) => return Err(InputHashesError::TemplateStringError(err)),
                        }
                    }
                    if rendered_outputs.is_empty() {
                        return Ok(None);
                    }
                    rendered_outputs
                } else {
                    return Ok(None);
                }
            }
            Err(_) => return Ok(None),
        };

        let files = FileHashes::from_files(task.project().root(), outputs.iter()).await?;

        // If no files matched, treat as no outputs for caching purposes
        if files.files.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self { files }))
    }
}

/// An error that might occur when computing the input hashes of a task.
#[derive(Debug, Error, Diagnostic)]
pub enum InputHashesError {
    #[error(transparent)]
    FileHashes(#[from] FileHashesError),

    #[error(transparent)]
    InvalidWorkingDirectory(#[from] InvalidWorkingDirectory),

    #[error(transparent)]
    #[diagnostic(transparent)]
    TemplateStringError(#[from] TemplateStringError),
}
