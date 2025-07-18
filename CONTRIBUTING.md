# Contributing to Pixi
## Introduction
Thank you for your interest in contributing to Pixi! We value the contributions from our community and look forward to collaborating with you.

## Code of Conduct
Before contributing, please read our [Code of Conduct](https://github.com/prefix-dev/pixi/blob/main/CODE_OF_CONDUCT.md). We expect all our contributors to follow it to ensure a welcoming and inclusive environment.

## Getting Started
**Familiarize yourself with Pixi:** Understand its functionality and codebase.
**Set up your environment:** Ensure you have the necessary tools and dependencies.

## How to Contribute
- **Find an Issue:** Look for open issues or create a new issue to discuss your idea.
- **Fork the Repository:** Make a copy of the repository to your account.
- **Create a Branch:** Create a branch in your fork to work on your changes.
- **Develop and Test:** Implement your changes and make sure they are thoroughly tested.
- **Write Clear Commit Messages:** Make your changes easy to understand.
- **Create a Pull Request:** Submit your changes for review.

> [!TIP]
> Discuss your change in the issue before developing the solution when the design is not predefined.
> This will help speedup the actual pull request review.

## Pull Request Process
- Ensure your code adheres to the project's coding standards, we use automated tooling for that, try `pixi run lint`.
- Document new code using the Rust docstrings like we do in most places.
- Update the `README.md` or documentation (`docs/`) with details of changes to the interface, if applicable.
- Your pull request will be reviewed by maintainers, who may suggest changes.

## Reporting Bugs
- Use the issue tracker to report bugs.
- Clearly describe the issue, including steps to reproduce the bug.
- Include any relevant logs or error messages.

## Feature Requests
- Use the issue tracker to suggest new features.
- Explain how the feature would be beneficial to the project.

## Questions and Discussions
For general questions, consider using our chat platform: [Discord](https://discord.gg/kKV8ZxyzY4)

## Acknowledgments
Your contributions are highly appreciated and will be credited accordingly.

## License
By contributing to Pixi, you agree that your contributions will be licensed under its [LICENSE](https://github.com/prefix-dev/pixi/blob/main/LICENSE).

# Tips while developing on `pixi`

## Pixi is a Pixi project itself, so use a preinstalled `pixi` to run the predefined tasks
```shell
pixi run build-debug # or `pixi run build-release` to build with optimizations
pixi run lint
pixi run test-all-fast
pixi run install # only works on unix systems as on windows you can't overwrite the binary while it's running
```

### Installing the target binaries to a custom location

Use the Pixi task `install-as` which invokes a python script to build the project and copy the executable to a custom location.
```shell
$ pixi run install-as
usage: install.py [-h] [--dest DEST] name

Build pixi and copy the executable to ~/.pixi/bin or a custom destination

positional arguments:
  name                  Name of the executable (e.g. pixid)

options:
  -h, --help            show this help message and exit
  --dest DEST           Destination directory for the executable, default: $PIXI_HOME/bin (or ~/.pixi/bin if $PIXI_HOME isn't set)
```

## Get your code ready for a PR
We use [`leftook`](https://lefthook.dev/) to run all the formatters and linters that we use.
You can run `pixi run pre-commit-install` to automatically run the formatters and linters before you commit.
This also installs a pre-push hook which runs `cargo clippy` —
use `pixi run pre-commit-install-minimal` instead to opt out of this hook.
To run all formatters and linters on all files:

```shell
pixi run lint
```

When you commit your code, please try to come up with a good commit message.
The maintainers (try to) use [conventional-commits](https://www.conventionalcommits.org/en/v1.0.0/).
```shell
git add FILES_YOU_CHANGED
# This is the conventional commit convention:
git commit -m "<type>[optional scope]: <description>"
# An example:
git commit -m "feat(exec): add xxx option"
```

## Color decisions in the ui code
We use the `console::style` function to colorize the output of the ui.
```rust
use console::style;
println!("{} {}", style("Hello").green(), style("world").red());
```

To sync the colors of the different parts of the ui, we use the following rules:
- `style("environment").magenta()`: The environment name
- `style("feature").cyan()`: The feature name
- `style("task").blue()`: The task name

These styles are put in the `consts` module or are a `.fancy_display()` on the names. If you want to add a new generic color, please add it there.


## Comment for Breaking Change
If you have a work-around for a potential breaking change, please add a comment in the code where you think the breaking change will happen. And what needs to be done when we **actually** want to break it. Use `BREAK:` in the comment to denote this.

For example:
```rust
// an enum to sort by size or name
#[derive(clap::ValueEnum, Clone, Debug, Serialize)]
pub enum SortBy {
    Size,
    Name,
    // BREAK: remove the alias
    #[value(alias = "type")]
    Kind,
}
```
