# bazelqueue agent instructions

- This is an independent Rust CLI repository. Alondra's product, deployment,
  release, and Bazel-only test rules do not apply here.
- `README.md` and `docs/` are the product and architecture contract; update
  them in the same change when behavior changes.
- The repository contains the implementation. Keep behavior and support claims
  aligned with executed native, terminal, installation and packaging evidence.
- Use Rust and native APIs for runtime behavior. Shell/Ruby/YAML glue is allowed
  only where external packaging or Bazel-generated run scripts require it.
- Keep the scheduler deterministic and independent of the OS, database, and
  process spawning. Time and resource samples must be injectable.
- Tests use isolated temporary state, configuration, HOME, and Bazel workspaces.
  Never modify the user's real shims, shell configuration, daemon, cache,
  Homebrew installation, or running builds from ordinary tests.
- No fixed sleeps, wall-clock success assertions, or retries hiding failures.
  Use barriers, pipes, observable state transitions, and virtual time.
- Never release capacity because a running build has exceeded a TTL. Never run
  outside the queue because waiting or admission failed.
- Do not execute argv through a shell, persist environment variables, or log
  complete invocation arguments. Preserve Unix argument bytes.
- Homebrew owns only `bin/bazelqueue`; the user setup owns PATH interception.
  Activation, uninstall, and upgrades must be reversible and ownership-aware.
- Runtime unsafe code belongs only in audited native boundary modules. Safe
  scheduler/protocol/store code must forbid unsafe code.
- Before implementation PRs: `make all`, a locked release build, and a clean
  `musts validate`. Add `MUSTS.yml` when the implementation is introduced;
  commit its evidence ledger. Validate packaging changes separately.
- Use Conventional Commit PR titles for release-plz. Never move published tags
  or replace already published release bytes.
- Agents learn how to wait from bazelqueue's own output (the hint on the first
  notice and the `bazelqueue: finished` summary). Keep those lines, the README's
  "Using it from an agent" section and their tests in step.
