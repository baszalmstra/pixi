//! Build a [`clap::Command`] from a pixi [`Task`] so that the same parser
//! used for the rest of pixi's CLI can validate task arguments and render
//! its familiar error messages (usage line, possible values, etc.).
//!
//! Tasks are defined in TOML rather than as Rust types, so this module uses
//! clap's builder API to construct the [`clap::Command`] dynamically from
//! the task's typed `args` definition.

use clap::{Arg, Command, builder::PossibleValuesParser};
use pixi_manifest::{
    Task,
    task::{TaskArg, TypedDependencyArg},
};

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

/// Parses arguments coming from a TOML `depends-on` entry.
///
/// In TOML, a dependency may pass either positional or named arguments
/// (e.g. `args = ["debug"]` or `args = [{ target = "debug" }]`). We map
/// each declared [`TaskArg`] to a long-flag `--name` option on a clap
/// [`Command`], translate positionals into the corresponding `--name
/// value` pair (matched by declaration index), and let clap validate
/// defaults, choices and required-ness — so the dependency-graph path
/// produces the same error rendering as direct CLI invocations.
///
/// The "positional before named" rule is enforced here, before clap
/// sees the arguments, so the error stays clearly attributable.
pub fn parse_dep_task_args(
    task_name: &str,
    task: &Task,
    dep_args: &[TypedDependencyArg],
) -> Result<Vec<String>, String> {
    let task_args = task.args().unwrap_or(&[]);

    let mut seen_named = false;
    for arg in dep_args {
        match arg {
            TypedDependencyArg::Named(_, _) => seen_named = true,
            TypedDependencyArg::Positional(value) => {
                if seen_named {
                    return Err(format!(
                        "positional argument '{value}' found after named argument for task '{task_name}'"
                    ));
                }
            }
        }
    }

    // Flatten dep_args into a `--name value` stream. Positionals are
    // bound to the declared arg at that index; if there are more
    // positionals than declared args, the extras are appended raw so
    // clap reports them as `unexpected argument`.
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

/// Renders a clap-style "unknown command" error for `name` when it does
/// not match any of `known_task_names` but is close to one (Levenshtein
/// distance ≤ `threshold`).
///
/// Returns `None` if `name` is exactly known, or if no candidate is
/// similar enough — in which case the caller should fall through to
/// running the command as an executable (so `pixi run python script.py`
/// keeps working even when no `python` task exists).
pub fn unknown_command_error(name: &str, known_task_names: &[String]) -> Option<String> {
    if known_task_names.iter().any(|n| n == name) {
        return None;
    }
    if !is_bare_identifier(name) {
        return None;
    }

    let threshold = match name.len() {
        0..=3 => 1,
        4..=7 => 2,
        _ => 3,
    };
    let has_close_match = known_task_names
        .iter()
        .any(|candidate| strsim::levenshtein(name, candidate) <= threshold);
    if !has_close_match {
        return None;
    }

    let command = Command::new("pixi run")
        .no_binary_name(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .override_usage("pixi run <TASK> [ARGS]")
        .arg(
            Arg::new("task")
                .required(true)
                .value_name("TASK")
                .value_parser(PossibleValuesParser::new(known_task_names)),
        );

    Some(
        command
            .try_get_matches_from([name])
            .err()
            .map(|err| err.render().to_string())
            // unreachable: an unknown value against PossibleValuesParser
            // always produces an error.
            .unwrap_or_else(|| format!("no task named '{name}'")),
    )
}

fn is_bare_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use pixi_manifest::task::{ArgName, Execute, Task, TaskArg, TypedDependencyArg};
    use std::str::FromStr;

    use super::{parse_dep_task_args, parse_typed_task_args, unknown_command_error};

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
        assert_eq!(values, vec!["release".to_string()]);
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
            &[TypedDependencyArg::Named(
                "target".into(),
                "debug".into(),
            )],
        )
        .unwrap();
        assert_eq!(values, vec!["debug".to_string()]);
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

    #[test]
    fn dep_rejects_positional_after_named() {
        let task = task_with_args(vec![
            TaskArg {
                name: ArgName::from_str("a").unwrap(),
                default: None,
                choices: None,
            },
            TaskArg {
                name: ArgName::from_str("b").unwrap(),
                default: None,
                choices: None,
            },
        ]);
        let err = parse_dep_task_args(
            "task",
            &task,
            &[
                TypedDependencyArg::Named("a".into(), "1".into()),
                TypedDependencyArg::Positional("2".into()),
            ],
        )
        .unwrap_err();
        assert!(err.contains("after named"), "got: {err}");
    }

    #[test]
    fn unknown_command_suggests_close_match() {
        let known: Vec<String> = vec!["build".into(), "test".into(), "lint".into()];
        let err = unknown_command_error("buidl", &known).expect("should suggest");
        assert!(err.contains("buidl"), "got: {err}");
        assert!(err.contains("build"), "got: {err}");
    }

    #[test]
    fn unknown_command_silent_for_distant_word() {
        let known: Vec<String> = vec!["build".into(), "test".into()];
        assert!(unknown_command_error("python", &known).is_none());
    }

    #[test]
    fn unknown_command_silent_for_non_identifier() {
        let known: Vec<String> = vec!["build".into()];
        // Path-like, contains shell metas — fall through to shell.
        assert!(unknown_command_error("./script.sh", &known).is_none());
        assert!(unknown_command_error("a b c", &known).is_none());
    }

    #[test]
    fn unknown_command_silent_for_exact_match() {
        let known: Vec<String> = vec!["build".into()];
        assert!(unknown_command_error("build", &known).is_none());
    }
}
