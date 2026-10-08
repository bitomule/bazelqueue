# Validation evidence

The implementation has deterministic scheduler/framing tests, foreground process
E2E tests, installation interruption E2E tests, native Bazel contract tests, and
interactive-shell PTY tests. CI executes the ordinary suite and a separate real
Bazel 8.4.2/9.2.0 matrix.

A passing compile alone is insufficient. The required scenarios include queued
FIFO/rank, resource sums, pressure/recovery, frontend SIGKILL, coordinator restart,
guardian loss with a live child, native server idleness, exact argv/stdin/stdout,
target exit 9, reversible installation, upgrade rollback, interruption replay,
backup tampering, PATH activation, and Ctrl-C/Ctrl-Z/fg.

The native guardian-loss test holds a real Bazel action through a socket barrier,
verifies that the server rejects an idle probe while busy, kills the guardian,
observes quarantine and a waiting successor, then releases the action and checks
that only completed native work permits the successor. Test output and musts
ledger evidence distinguish native validation from fixture-only checks.

Installation tests inject controlled cutpoints before and after journal/image
application, during rollback and after commit. Re-entering the installer must
restore or finalize the correct state; upgrade failure must retain the prior
managed installation and original uninstall backups.

All fixtures use temporary HOME/state/workspaces. Real Bazel caches stay outside
the test workspace, including the repository-contents cache requirement in 9.2.
No ordinary test touches the live ~/.alondra, Keychain, launchd, Pi or user shims.

Performance measurements must separate queue/IPC overhead, warm/cold builds,
per-call latency, total makespan, memory pressure and swap activity. CPU-only
utilization and a cached no-op build do not prove maximum compile throughput.
Machine-specific calibration is stored locally, not shipped as a personal default.

An opt-in compiler experiment is available in `tests/calibrate.py`. It compares
one and two admitted builds across two isolated workspaces, using sixteen fresh
Clang `-O2` compilations per profile with warm JVMs and native pressure telemetry.
Pass `--binary`, `--bazel`, and an external `--output` JSON path. It does not
modify the installed configuration. Treat its result as evidence for this CPU
workload; Swift/Rust compilers and worker-heavy builds need their own measurements.

Local verification on an M3 Pro with 11 logical cores and 18 GiB RAM used
14 unit tests, 15 process E2E tests, 17 installation tests, 3 interactive PTY
tests, and 6 real Bazel contracts on each of 8.4.2 and 9.2.0. The native
contracts include simultaneous held actions in distinct workspaces and two
queued builds completing while a run target remains alive.

The compiler calibration performed one sample per profile:

| CPU tokens | Concurrent builds | Total time | Native pressure before / after |
| --- | --- | --- | --- |
| 8 | 1 | 22.179 s | normal / normal |
| 8 | 2 | 22.105 s | normal / warning |
| 10 | 2 | 93.200 s | warning / normal |

These are total command times, including queue admission. The third sample
started under memory pressure and waited for admission; it does not establish
compiler speed. The first two samples do not demonstrate a meaningful throughput
gain from simultaneous invocations. The local initial selection is 8 CPU tokens,
9 GiB reserved memory and one build, with parallel actions inside that build.
Other workloads should be calibrated before increasing invocation concurrency.

A noisy native recovery contract emits 128 KiB of stderr. Its test consumer
uses a file sink: an unread pipe can block the native client even after the
server finishes, and the queue correctly continues accounting for that client.
The negative ablation with an unread pipe must fail successor admission; the
consumed-output version must pass all busy/quarantine/idleness checks.

`make e2e`, `make contract`, and `make terminal` use a separate Cargo target
directory. Ordinary Cargo checks therefore cannot replace a feature-enabled
fixture executable while an E2E suite is using it.
