use crate::task::error::MissingTaskError;
use crate::task::ExecutableTask;
use futures::future::BoxFuture;
use miette::Diagnostic;
use std::borrow::Cow;
use std::collections::HashSet;
use std::future::{ready, Future};
use thiserror::Error;

/// An error that might occur when traversing a task (see [`ExecutableTask::traverse`]).
#[derive(Debug, Error, Diagnostic)]
pub enum TraversalError {
    #[error(transparent)]
    MissingTask(MissingTaskError),
}

impl<'p> ExecutableTask<'p> {
    /// Returns a list of [`ExecutableTask`]s that includes this task and its dependencies in the
    /// order they should be executed (topologically sorted).
    ///
    /// Internally this function uses the [`ExecutableTask::traverse`] function to collect the
    /// tasks in the order they are traversed.
    pub async fn get_ordered_dependencies(self) -> Result<Vec<ExecutableTask<'p>>, TraversalError> {
        self.traverse(
            Vec::new(),
            |mut tasks, task| async move {
                tasks.push(task);
                Ok(tasks)
            },
            |_, _| Box::pin(ready(Ok(true))),
        )
        .await
    }

    /// Traverses the task and its dependencies in topological order.
    ///
    /// The `visit` function is called for each task. If the `visit` function returns an error, the
    /// traversal is stopped and the error is returned.
    ///
    /// The `should_visit` function is called for each task. If the `should_visit` function returns
    /// `false`, the task and its dependencies are skipped.
    ///
    /// The `should_visit` function should return a Boxed future because the Rust type system is
    /// not (yet) able to express the lifetime of the returned future. See:
    /// <https://users.rust-lang.org/t/how-to-express-that-the-future-returned-by-a-closure-lives-only-as-long-as-its-argument/90039>
    pub async fn traverse<State, R, RFut, F, Err>(
        self,
        initial_state: State,
        mut visit: R,
        mut should_visit: F,
    ) -> Result<State, Err>
    where
        RFut: Future<Output = Result<State, Err>> + 'p,
        R: FnMut(State, ExecutableTask<'p>) -> RFut,
        F: for<'t> FnMut(&State, &'t ExecutableTask<'p>) -> BoxFuture<'t, Result<bool, Err>>,
        Err: From<TraversalError>,
    {
        return inner(
            initial_state,
            self,
            &mut HashSet::new(),
            &mut visit,
            &mut should_visit,
        )
        .await;

        #[async_recursion::async_recursion(?Send)]
        async fn inner<'p, State, R, RFut, F, Err>(
            state: State,
            task: ExecutableTask<'p>,
            visited: &mut HashSet<String>,
            visit: &mut R,
            should_visit: &mut F,
        ) -> Result<State, Err>
        where
            RFut: Future<Output = Result<State, Err>>,
            R: FnMut(State, ExecutableTask<'p>) -> RFut,
            F: for<'t> FnMut(&State, &'t ExecutableTask<'p>) -> BoxFuture<'t, Result<bool, Err>>,
            Err: From<TraversalError>,
            'p: 'async_recursion,
        {
            // If the task has a name that we already visited we can immediately return.
            if let Some(name) = task.name() {
                if visited.contains(name) {
                    return Ok(state);
                }
                visited.insert(name.to_string());
            }

            // Determine if we should even visit this task (and its dependencies in the first place).
            if !should_visit(&state, &task).await? {
                return Ok(state);
            }

            // Locate the dependencies in the project and add them to the stack
            let mut state = state;
            for dependency in task.task().depends_on() {
                let task_dependency = task
                    .project()
                    .task_opt(dependency, task.platform)
                    .ok_or_else(|| MissingTaskError {
                        task_name: dependency.clone(),
                    })
                    .map_err(TraversalError::MissingTask)?;

                state = inner(
                    state,
                    ExecutableTask {
                        project: task.project,
                        name: Some(dependency.clone()),
                        task: Cow::Borrowed(task_dependency),
                        additional_args: Vec::new(),
                        platform: task.platform,
                    },
                    visited,
                    visit,
                    should_visit,
                )
                .await?;
            }

            match visit(state, task).await {
                Ok(state) => Ok(state),
                Err(err) => Err(err),
            }
        }
    }
}
