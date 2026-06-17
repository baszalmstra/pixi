# Third-party notices

## ignore parallel walker

`src/walk/parallel.rs` is based on the parallel walking implementation from the
[`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore)
crate, version 0.4.25.

Original license: Unlicense OR MIT.

The Pixi version adapts the work-stealing traversal shape so workers can consult
and repair an indexed filesystem view instead of always enumerating directly from
the operating system.
