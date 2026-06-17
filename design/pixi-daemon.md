# Pixi Daemon

## Motivation

Pixi is stateless. Every time you invoke pixi it rebuilds state from the files on disk.
Think, checking if the lockfile is up to date with the pixi.toml, checking if an environment is up to date with the installed environment, checking if the metadata of source packages is out of date or whether a source package needs to be rebuild because its source code changed, etc.
For large repositories though, this can take quite a lot of time. For an example at e:/navigation2 this can easily take 500ms.
Recreating this state is done a lot beceause every pixi command essentially checks whether its on disk state is consistent.
So simply running "pixi echo hello" also pays this cost. Furthermore we want to scale this out much, much further to HUGE repositories with hundreds of packages.
Our current approach doesnt scale.

I have been optimizing the crap out of making recreating the state from disk faster, but I feel this will hit a ceiling and doesnt scale well. Instead, my plan is to use a daemon process that keeps the state in memory and only rebuilds it when necessary. It would watch the filesystem for changes and only rebuild the state when necessary.

This is very similar to how other large-scale build systems work, such as Buck2, Bazel (are there others?).

One system that we could use in the daemon is the pixi compute engine to do incremental computations (although we would need a way to invalidate state first).

## Roles

What are the roles of the daemon? What state does it monitor? What does the API look like?

- Monitor the filesystem for changes?
- Parse the lock-file and check for satisfiability?
- Keep caches in memory, like build backend metadata?

## TODO

- [ ] Research existing software like Buck2 / Bazel
  - [ ] What is the role of the daemon, what is the role of the CLI?
  - [ ] How long do these daemons keep the state in memory?
  - [ ] One deamon per workspace? Or one global one?
  - [ ] How does the daemon communicate with the CLI?
  - [ ] Can we use shared memory to share the state between the daemon and the CLI? How can we structure this properly with Rust?
