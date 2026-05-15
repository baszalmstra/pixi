//! Build a [`clap::Command`] from a pixi [`Task`] so that the same parser
//! used for the rest of pixi's CLI can validate task arguments and render
//! its familiar error messages (usage line, possible values, etc.).
//!
//! Tasks are defined in TOML rather than as Rust types, so this module uses
//! clap's builder API to construct the [`clap::Command`] dynamically from
//! the task's typed `args` definition.

use clap::{Arg, Command, builder::PossibleValuesParser};
use pixi_manifest::{Task, task::TaskArg};

/// Builds a [`clap::Command`] that parses the positional arguments for a
/// task.
///
/// Each declared [`TaskArg`] becomes a positional [`Arg`]. Defaults and
/// `choices` are translated into clap's `default_value` and
/// `value_parser(PossibleValuesParser)`.
///
/// The command uses `no_binary_name(true)` so callers can feed it just the
/// args after the task name. `--help` is not registered as a flag; tasks may
/// legitimately want to forward `--help` to the underlying command (e.g.
/// `pixi run ruff --help`).
pub fn task_clap_command(task_name: &str, task: &Task) -> Command {
    let mut command = Command::new(task_name.to_string())
        .no_binary_name(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .override_usage(format!("pixi run {task_name} [ARGS]"));

    if let Some(description) = task.description() {
        command = command.about(description.to_string());
    }

    if let Some(args) = task.args() {
        for arg in args {
            command = command.arg(task_arg_to_clap(arg));
        }
    }

    command
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

/// Parses CLI arguments for a task whose `args` schema is defined.
///
/// On success, returns the resolved value for each declared
/// [`TaskArg`] in the same order. On failure, returns the fully-rendered
/// clap error message (including the usage line and any "possible values"
/// hint), which the caller can surface as a diagnostic.
pub fn parse_typed_task_args(
    task_name: &str,
    task: &Task,
    args: &[String],
) -> Result<Vec<String>, String> {
    let task_args = task.args().unwrap_or(&[]);
    let command = task_clap_command(task_name, task);

    let matches = command
        .try_get_matches_from(args)
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
    use pixi_manifest::task::{ArgName, Execute, Task, TaskArg};
    use std::str::FromStr;

    use super::parse_typed_task_args;

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

    #[test]
    fn parses_positional_with_choices() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: Some(vec!["debug".into(), "release".into()]),
        }]);
        let values = parse_typed_task_args("build", &task, &["debug".to_string()]).unwrap();
        assert_eq!(values, vec!["debug".to_string()]);
    }

    #[test]
    fn reports_invalid_choice_with_usage() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: Some(vec!["debug".into(), "release".into()]),
        }]);
        let err = parse_typed_task_args("build", &task, &["profile".to_string()]).unwrap_err();
        assert!(err.contains("profile"), "error should mention bad value: {err}");
        assert!(
            err.contains("debug") && err.contains("release"),
            "error should list possible values: {err}"
        );
        assert!(err.contains("target"), "error should mention the arg name: {err}");
    }

    #[test]
    fn reports_missing_required_arg() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: None,
        }]);
        let err = parse_typed_task_args("build", &task, &[]).unwrap_err();
        assert!(err.contains("target"), "error should mention missing arg: {err}");
        assert!(err.contains("pixi run build"), "error should show usage: {err}");
    }

    #[test]
    fn applies_default_when_arg_omitted() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: Some("debug".to_string()),
            choices: None,
        }]);
        let values = parse_typed_task_args("build", &task, &[]).unwrap();
        assert_eq!(values, vec!["debug".to_string()]);
    }

    #[test]
    fn reports_unexpected_extra_arg() {
        let task = task_with_args(vec![TaskArg {
            name: ArgName::from_str("target").unwrap(),
            default: None,
            choices: None,
        }]);
        let err = parse_typed_task_args(
            "build",
            &task,
            &["debug".to_string(), "extra".to_string()],
        )
        .unwrap_err();
        assert!(
            err.contains("extra") || err.contains("unexpected"),
            "error should report extra arg: {err}"
        );
    }
}
