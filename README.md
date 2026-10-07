# bazelqueue

A per-user Bazel admission queue with foreground command semantics.

The intended interface is ordinary `bazel` and `bazelisk`: the caller receives
queue-position updates on stderr, waits for capacity, and then runs its original
command with its terminal, environment, working directory, and exit status.

**Status: architecture and implementation plan. No executable or installable
release exists yet.** The commands below describe the planned product.

```sh
brew install bitomule/tap/bazelqueue
bazelqueue setup

bazel build //...
bazelisk test //... --test_output=errors
bazelqueue status
```

One Rust executable provides the CLI, PATH shims, coordinator, and invocation
guardian. No queueing scripts, Pueue, database server, or Rust installation are
required on the user's machine. SQLite is linked into the executable.

The first supported release targets macOS Apple Silicon. Other platforms remain
an explicit follow-up until their process, resource, and terminal contracts pass
the same tests.

Read [PLAN.md](PLAN.md) for the complete design, implementation sequence,
acceptance criteria, installation lifecycle, and release pipeline.
