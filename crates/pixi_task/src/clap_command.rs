//! Build a [`clap::Command`] for `pixi run` where each pixi task is a
//! dynamically-registered subcommand. This lets clap drive everything we
//! used to do by hand — subcommand matching, did-you-mean for unknown
//! tasks, validation of typed arguments, and arg forwarding — so the
//! diagnostics look like the rest of pixi's CLI.
//!
//! Tasks live in TOML rather than as Rust types, so the [`Command`] is
//! assembled at runtime using clap's builder API.

use std::collections::HashMap;

use clap::{Arg, Command, builder::PossibleValuesParser, error::ErrorKind};
use pixi_manifest::{
    Task,
    task::{TaskArg, TypedDependencyArg},
};

/// Sentinel arg id used to collect everything after `--` for tasks with
/// typed args.
const EXTRA_ARG_ID: &str = "__pixi_extra";
/// Sentinel arg id used to slurp every argument for tasks without typed
/// args (free-form forwarding).
const FREEFORM_ARG_ID: &str = "__pixi_free";

/// Builds a [`Command`] that represents `pixi run` with every task in
/// `tasks` registered as a subcommand.
///
/// `tasks` is consumed in order; if the same name appears more than once
/// (a task with the same name defined in multiple environments) the first
/// definition wins for purposes of the args schema.
///
/// Unknown subcommands are accepted via clap's
/// `allow_external_subcommands` and returned as
/// [`RunParseResult::External`] so the caller can run them through the
/// shell (preserving `pixi run python script.py`).
pub fn build_run_command<'a, I>(tasks: I) -> Command
where
    I: IntoIterator<Item = (&'a str, &'a Task)>,
{
    let mut command = Command::new("pixi run")
        .no_binary_name(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .subcommand_required(true)
        .allow_external_subcommands(true)
        .override_usage("pixi run <TASK> [ARGS]");

    let mut seen = std::collections::HashSet::new();
    for (name, task) in tasks {
        if seen.insert(name.to_string()) {
            command = command.subcommand(task_subcommand(name, task));
        }
    }

    command
}

/// Builds the [`Command`] used as a subcommand for a single task.
///
/// * Typed tasks expose each declared [`TaskArg`] as a positional, plus a
///   `last(true)` trailing arg that captures whatever follows `--` so it
///   can be forwarded to the underlying command.
/// * Free-form tasks (no `args` schema) use a single
///   `trailing_var_arg(true) + allow_hyphen_values(true)` positional so
///   the entire tail — including `--help`, `--`, hyphenated flags —
///   reaches the underlying command verbatim.
///
/// `disable_help_flag(true)` is set on every subcommand: pixi's design is
/// that `pixi run ruff --help` forwards `--help` to ruff, never
/// intercepts it.
fn task_subcommand(name: &str, task: &Task) -> Command {
    let mut sub = Command::new(name.to_string())
        .disable_help_flag(true)
        .disable_version_flag(true)
        .override_usage(format!("pixi run {name} [ARGS]"));

    if let Some(description) = task.description() {
        sub = sub.about(description.to_string());
    }

    match task.args() {
        Some(args) if !args.is_empty() => {
            for arg in args {
                sub = sub.arg(task_arg_to_clap(arg));
            }
            // Hidden sentinel: collects whatever follows `--` so it can be
            // forwarded verbatim. Kept out of the usage line to avoid
            // leaking the internal `__pixi_extra` name.
            sub = sub.arg(
                Arg::new(EXTRA_ARG_ID)
                    .last(true)
                    .num_args(0..)
                    .allow_hyphen_values(true)
                    .hide(true),
            );
        }
        _ => {
            sub = sub.arg(
                Arg::new(FREEFORM_ARG_ID)
                    .trailing_var_arg(true)
                    .allow_hyphen_values(true)
                    .num_args(0..)
                    .hide(true),
            );
        }
    }

    sub
}

fn task_arg_to_clap(task_arg: &TaskArg) -> Arg {
    let name = task_arg.name.as_str().to_string();
    let mut arg = Arg::new(name.clone()).value_name(name);
    if let Some(default) = &task_arg.default {
        arg = arg.default_value(default.clone()).required(false);
    } else {
        arg = arg.required(true);
    }
    if let Some(choices) = &task_arg.choices {
        arg = arg.value_parser(PossibleValuesParser::new(choices));
    }
    arg
}

/// Outcome of running `pixi run`'s args through the dynamic clap
/// subcommand tree.
#[derive(Debug)]
pub enum RunParseResult {
    /// `pixi run` invoked without anything after it.
    Empty,
    /// A known task matched and its arguments validated cleanly.
    Task {
        name: String,
        /// One entry per declared [`TaskArg`], in declaration order.
        values: Vec<String>,
        /// Anything after `--` (for typed tasks) or the entire forwarded
        /// arg list (for free-form tasks).
        extra: Vec<String>,
    },
    /// The first token didn't match any task and clap didn't recommend a
    /// close match — the caller should run it verbatim as a shell
    /// command, preserving `pixi run python script.py`.
    External(Vec<String>),
}

/// Error produced when clap rejects the user's `pixi run` invocation.
#[derive(Debug)]
pub enum RunParseError {
    /// A known task's args failed validation. `rendered` is clap's
    /// pre-formatted error message (usage line, possible-values hint,
    /// duplicate-arg, etc.).
    TaskArgs { task: String, rendered: String },
}

/// Parse the trailing arguments of `pixi run` against the dynamic task
/// subcommand tree.
///
/// Unknown subcommands come back as [`RunParseResult::External`] (via
/// clap's `allow_external_subcommands`); the caller is expected to run
/// them as a shell command and fall back to printing `command_not_found`
/// if the shell exits 127.
pub fn parse_run_args<'a, I>(
    tasks: I,
    cli_args: &[String],
) -> Result<RunParseResult, RunParseError>
where
    I: IntoIterator<Item = (&'a str, &'a Task)>,
{
    if cli_args.is_empty() {
        return Ok(RunParseResult::Empty);
    }

    // We need the task map to look up args after clap matches a
    // subcommand. Collect first, then hand references back to clap.
    let task_vec: Vec<(String, &Task)> = tasks
        .into_iter()
        .map(|(name, task)| (name.to_string(), task))
        .collect();
    let mut task_map: HashMap<&str, &Task> = HashMap::new();
    for (name, task) in &task_vec {
        task_map.entry(name.as_str()).or_insert(*task);
    }

    let command = build_run_command(task_vec.iter().map(|(n, t)| (n.as_str(), *t)));

    let matches = match command.try_get_matches_from(cli_args) {
        Ok(m) => m,
        Err(err) => return dispatch_clap_error(err, cli_args),
    };

    let (name, sub_matches) = matches
        .subcommand()
        .expect("subcommand_required guarantees one matched");

    // Known task → extract typed args + trailing extras.
    if let Some(task) = task_map.get(name) {
        let task_args = task.args().unwrap_or(&[]);
        let values: Vec<String> = task_args
            .iter()
            .map(|a| {
                sub_matches
                    .get_one::<String>(a.name.as_str())
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();

        let extra_key = if task_args.is_empty() {
            FREEFORM_ARG_ID
        } else {
            EXTRA_ARG_ID
        };
        let extra: Vec<String> = sub_matches
            .get_many::<String>(extra_key)
            .into_iter()
            .flatten()
            .cloned()
            .collect();

        return Ok(RunParseResult::Task {
            name: name.to_string(),
            values,
            extra,
        });
    }

    // Unknown subcommand accepted by `allow_external_subcommands(true)`.
    // The trailing args land under the empty key as `OsString`.
    let trailing: Vec<String> = sub_matches
        .get_many::<std::ffi::OsString>("")
        .into_iter()
        .flatten()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let mut argv = Vec::with_capacity(1 + trailing.len());
    argv.push(name.to_string());
    argv.extend(trailing);
    Ok(RunParseResult::External(argv))
}

/// Translates a clap parse error from `parse_run_args` into either an
/// `Empty` (missing-subcommand) result or a user-facing diagnostic.
///
/// With `allow_external_subcommands(true)` clap doesn't produce
/// `InvalidSubcommand` for unknown names — those come back as `Ok` with
/// an external subcommand match. The only remaining error kinds are
/// arg-validation failures inside a known subcommand.
fn dispatch_clap_error(
    err: clap::Error,
    cli_args: &[String],
) -> Result<RunParseResult, RunParseError> {
    match err.kind() {
        ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            Ok(RunParseResult::Empty)
        }
        _ => Err(RunParseError::TaskArgs {
            // The error happened inside a subcommand's parsing; the
            // first cli arg is the task name.
            task: cli_args[0].clone(),
            rendered: err.render().to_string(),
        }),
    }
}

/// Parses arguments coming from a TOML `depends-on` entry.
///
/// In TOML, a dependency may pass either positional or named arguments
/// (e.g. `args = ["debug"]` or `args = [{ target = "debug" }]`). We map
/// each declared [`TaskArg`] to a long-flag `--name` option on a clap
/// [`Command`], translate positionals into the corresponding `--name
/// value` pair (matched by declaration index), and hand the resulting
/// stream to clap. Defaults, choices, required-ness, unknown names,
/// duplicate values and unexpected extras are all reported by clap with
/// its usual rendering — no custom validation is layered on top.
pub fn parse_dep_task_args(
    task_name: &str,
    task: &Task,
    dep_args: &[TypedDependencyArg],
) -> Result<Vec<String>, String> {
    let task_args = task.args().unwrap_or(&[]);

    let mut cli_args: Vec<String> = Vec::new();
    for (i, arg) in dep_args.iter().enumerate() {
        match arg {
            TypedDependencyArg::Positional(value) => {
                if let Some(declared) = task_args.get(i) {
                    cli_args.push(format!("--{}", declared.name.as_str()));
                    cli_args.push(value.clone());
                } else {
                    cli_args.push(value.clone());
                }
            }
            TypedDependencyArg::Named(name, value) => {
                cli_args.push(format!("--{name}"));
                cli_args.push(value.clone());
            }
        }
    }

    let mut command = Command::new(task_name.to_string())
        .no_binary_name(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .override_usage(format!("depends-on {{ task = \"{task_name}\", args = [...] }}"));

    if let Some(description) = task.description() {
        command = command.about(description.to_string());
    }

    for declared in task_args {
        let name = declared.name.as_str().to_string();
        let mut a = Arg::new(name.clone())
            .long(name.clone())
            .value_name(name);
        if let Some(default) = &declared.default {
            a = a.default_value(default.clone()).required(false);
        } else {
            a = a.required(true);
        }
        if let Some(choices) = &declared.choices {
            a = a.value_parser(PossibleValuesParser::new(choices));
        }
        command = command.arg(a);
    }

    let matches = command
        .try_get_matches_from(&cli_args)
        .map_err(|err| err.render().to_string())?;

    let mut values = Vec::with_capacity(task_args.len());
    for declared in task_args {
        let name = declared.name.as_str();
        let value = matches
            .get_one::<String>(name)
            .cloned()
            .unwrap_or_default();
        values.push(value);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use pixi_manifest::task::{ArgName, Execute, Task, TaskArg, TypedDependencyArg};
    use std::str::FromStr;

    use super::{RunParseError, RunParseResult, parse_dep_task_args, parse_run_args};

    fn task_with_args(args: Vec<TaskArg>) -> Task {
        Task::Execute(Box::new(Execute {
            cmd: pixi_manifest::task::CmdArgs::Single("echo {{ target }}".into()),
            inputs: None,
            outputs: None,
            depends_on: vec![],
            cwd: None,
            env: None,
            default_environment: None,
            description: Some("Build the project".to_string()),
            clean_env: false,
            args: Some(args),
        }))
    }

    fn free_form_task() -> Task {
        Task::Execute(Box::new(Execute {
            cmd: pixi_manifest::task::CmdArgs::Single("echo".into()),
            inputs: None,
            outputs: None,
            depends_on: vec![],
            cwd: None,
            env: None,
            default_environment: None,
            description: Some("Say hello".to_string()),
            clean_env: false,
            args: None,
        }))
    }

    fn s(v: &str) -> String {
        v.to_string()
    }

    #[test]
    fn empty_args_returns_empty() {
        let task = free_form_task();
        let result = parse_run_args([("hello", &task)], &[]).unwrap();
        assert!(matches!(result, RunParseResult::Empty));
    }

    #[test]
    fn known_task_with_typed_args_parses() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: Some(vec!["debug".into(), "release".into()]),
        }]);
        let result = parse_run_args([("build", &task)], &[s("build"), s("debug")]).unwrap();
        match result {
            RunParseResult::Task { name, values, extra } => {
                assert_eq!(name, "build");
                assert_eq!(values, vec![s("debug")]);
                assert!(extra.is_empty());
            }
            other => panic!("expected Task, got {other:?}"),
        }
    }

    #[test]
    fn typed_task_collects_extras_after_dash_dash() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: None,
        }]);
        let result = parse_run_args(
            [("build", &task)],
            &[s("build"), s("debug"), s("--"), s("--verbose"), s("foo")],
        )
        .unwrap();
        match result {
            RunParseResult::Task { values, extra, .. } => {
                assert_eq!(values, vec![s("debug")]);
                assert_eq!(extra, vec![s("--verbose"), s("foo")]);
            }
            other => panic!("expected Task, got {other:?}"),
        }
    }

    #[test]
    fn free_form_task_forwards_everything() {
        let task = free_form_task();
        let result = parse_run_args(
            [("hello", &task)],
            &[s("hello"), s("--help"), s("--"), s("more")],
        )
        .unwrap();
        match result {
            RunParseResult::Task { name, values, extra } => {
                assert_eq!(name, "hello");
                assert!(values.is_empty());
                assert_eq!(extra, vec![s("--help"), s("--"), s("more")]);
            }
            other => panic!("expected Task, got {other:?}"),
        }
    }

    #[test]
    fn unknown_token_falls_through() {
        // Both close-match typos and genuinely unrelated tokens fall
        // through to the shell via clap's `allow_external_subcommands`.
        // The CLI layer surfaces `command_not_found` if the shell exits
        // 127.
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: None,
        }]);

        let typo = parse_run_args([("build", &task)], &[s("buidl")]).unwrap();
        assert!(matches!(typo, RunParseResult::External(ref a) if a == &vec![s("buidl")]));

        let distant =
            parse_run_args([("build", &task)], &[s("python"), s("script.py")]).unwrap();
        assert!(
            matches!(distant, RunParseResult::External(ref a) if a == &vec![s("python"), s("script.py")])
        );
    }

    #[test]
    fn invalid_choice_surfaces_clap_error() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: Some(vec!["debug".into(), "release".into()]),
        }]);
        let err = parse_run_args([("build", &task)], &[s("build"), s("profile")]).unwrap_err();
        match err {
            RunParseError::TaskArgs { task, rendered } => {
                assert_eq!(task, "build");
                assert!(rendered.contains("profile"), "{rendered}");
                assert!(
                    rendered.contains("debug") && rendered.contains("release"),
                    "{rendered}"
                );
            }
            other => panic!("expected TaskArgs, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_arg_surfaces_clap_error() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: None,
        }]);
        let err = parse_run_args([("build", &task)], &[s("build")]).unwrap_err();
        assert!(matches!(err, RunParseError::TaskArgs { .. }));
    }

    #[test]
    fn default_value_applied_when_missing() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: Some("debug".into()),
            choices: None,
        }]);
        let result = parse_run_args([("build", &task)], &[s("build")]).unwrap();
        match result {
            RunParseResult::Task { values, .. } => assert_eq!(values, vec![s("debug")]),
            other => panic!("expected Task, got {other:?}"),
        }
    }

    // --- depends-on tests, unchanged behaviour ---

    #[test]
    fn dep_parses_positional() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: Some(vec!["debug".into(), "release".into()]),
        }]);
        let values = parse_dep_task_args(
            "build",
            &task,
            &[TypedDependencyArg::Positional("release".into())],
        )
        .unwrap();
        assert_eq!(values, vec![s("release")]);
    }

    #[test]
    fn dep_parses_named() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: Some(vec!["debug".into(), "release".into()]),
        }]);
        let values = parse_dep_task_args(
            "build",
            &task,
            &[TypedDependencyArg::Named("target".into(), "debug".into())],
        )
        .unwrap();
        assert_eq!(values, vec![s("debug")]);
    }

    #[test]
    fn dep_rejects_invalid_choice() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: Some(vec!["debug".into(), "release".into()]),
        }]);
        let err = parse_dep_task_args(
            "build",
            &task,
            &[TypedDependencyArg::Positional("profile".into())],
        )
        .unwrap_err();
        assert!(err.contains("profile") && err.contains("debug"), "got: {err}");
    }

    #[test]
    fn dep_rejects_unknown_named_arg() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: None,
        }]);
        let err = parse_dep_task_args(
            "build",
            &task,
            &[TypedDependencyArg::Named("flavor".into(), "x".into())],
        )
        .unwrap_err();
        assert!(err.contains("flavor"), "got: {err}");
    }
}
