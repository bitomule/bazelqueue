# bazelqueue

A native per-user admission queue for Bazel. Keep calling `bazel`, `bazelisk`,
and your existing Makefiles: queue position and waiting reasons go to stderr;
the actual command retains its arguments, stdin, stdout, terminal and result.

```sh
brew install bitomule/tap/bazelqueue
bazelqueue setup

bazel build //...
bazelisk test //... --test_output=errors
bazelqueue status
```

The first Homebrew publication is pending the repository's two release secrets.
The implementation can already be built and installed locally:

```sh
make release
./target/release/bazelqueue setup --backend /absolute/path/to/real/bazelisk
```

Existing shims are replaced only with an explicit, reversible migration:

```sh
bazelqueue setup --preview --replace --migrate
bazelqueue setup --replace --migrate
```

One approximately 5 MiB production executable supplies the CLI, coordinator,
foreground guardian and execution barrier. SQLite is linked into it. End users
need Bazel/Bazelisk; no Rust, Python, Pueue or database service is required.
Python is used only by development PTY tests.

macOS Apple Silicon is the first supported release. The native contract is
validated against Bazel 8.4.2 and 9.2.0. Unsupported versions, opaque wrappers,
custom invocation policies and complex resource overrides retain their original
arguments under exclusive compatibility admission.

The coordinator tracks CPU and memory reservations, waits through memory
pressure, keeps FIFO order within a server lane and allows other workspaces to
proceed when their resources fit. A lone managed build can receive the wide
profile; concurrent managed builds use smaller immutable reservations. The
initial concurrency is one until the machine is calibrated.

A backend cannot start before its process identity is durably acknowledged.
Frontend death triggers cancellation. Coordinator restart reconnects guardians
without repeating commands. Uncertain native work remains quarantined until an
authenticated nonblocking Bazel-server probe verifies idleness. Running builds
never lose reservations due to a TTL, waiting limit or admission error.

`bazel run` compiles using Bazel's generated run script, releases compilation
capacity, then runs the target with its terminal and arguments. The run phase
remains visible and its process footprint counts toward admission. Explicit
`--script_path`, `--norun`, and opaque modes retain native behavior.

## Commands

- `status`, `status --json`, `watch`: inspect active requests and queue reasons.
- `cancel REQUEST_ID`: cancel a queued or owned running request.
- `drain`, `resume`: pause/resume new admissions without killing current work.
- `doctor`: inspect backend, telemetry, installation and recovery state.
- `config show`: print the effective per-user configuration.
- `exec -- /absolute/backend ARGS...`: explicit queued invocation.
- `uninstall`: restore owned user shims before `brew uninstall bazelqueue`.

Default private state is `~/.local/state/bazelqueue`. `BAZELQUEUE_HOME` selects an
isolated namespace; `BAZELQUEUE_PROGRESS=quiet` suppresses queue notices. The
queue stores metadata, not full argv, environment, credentials or build output.
Absolute calls to unmanaged binaries and other users' processes are outside PATH
interception; machine memory pressure is still observed.

Managed resource policies own their configured scheduling caps. Proven stricter
direct numeric limits are retained; unproven forms use compatibility admission.
Bazel's memory values are estimates, not hard OS memory quotas. Unexpectedly
large actions can exceed predictions; the queue stops subsequent admission when
pressure rises. No universal maximum-throughput claim is made without calibration.

## Development

```sh
make all
BAZELQUEUE_TEST_BAZEL=/absolute/path/to/bazel make contract
make release
musts validate
```

Tests isolate HOME, configuration, state and Bazel workspaces. Process tests use
socket handshakes and observable states; watchdogs detect hangs. Ordinary tests
do not mutate the user's installation or launch real app/VM suites.

See [architecture](docs/architecture.md), [installation](docs/installation.md),
[release setup](docs/release.md), [validation](docs/validation.md), and the original
[implementation plan](PLAN.md).
