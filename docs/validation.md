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
14 unit tests, 17 process E2E tests, 17 installation tests, 3 interactive PTY
tests, and 7 real Bazel contracts on each of 8.4.2 and 9.2.0. The native
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

Retained-image tests delete the original frontend executable after admission,
restart the coordinator, and require unchanged ownership and successful native
run handoff. A targeted SIGINT test addresses the frontend PID directly and
requires the owned backend and caller to terminate with the same signal.

The published v0.1.1 archive and SHA256 were downloaded back from GitHub and
verified before Homebrew installation. `brew test bitomule/tap/bazelqueue` passed.
Live user activation preserved the original shim bytes/symlink and main bazelrc
fingerprint. A fresh login shell in a temporary workspace resolved both names
to the native shims. Two real calls reported queue positions 1 and 2, waited
behind drain, and completed successfully after resume. Another agent's queued
clean command also completed through the installed queue. The queue was left
resumed; no running user build was terminated.

## Cache checks beyond the reserved CPUs

`--jobs` used to equal the reserved CPUs and `--local_test_jobs` was 2. Under
memory pressure the queue reserves 2 CPUs, so a cold build checked and fetched
its cache hits two at a time. Measured on Alondra at 195f391, the 63 `bazel
test` targets of its `MUSTS.yml` in one command, each run in a fresh worktree
with its own output base and `--repo_contents_cache=` (the shared repository
cache had a broken entry that day). Every run below was admitted with 2 CPU /
2304 MiB; v0.1.8 therefore passed `--jobs=2`, this change `--jobs=64` with
`--local_resources=cpu=2`.

| Cached test results, same disk and remote cache | v0.1.8 | `--jobs=64` |
| --- | --- | --- |
| Cold command | 233 s | 78 s |
| Cold remote cache checks (wall / summed) | 73.4 s / 107.8 s | 19.9 s / 7.7 min |
| Cold critical path | 11.8 s | 18.0 s |
| Warm command, after the server exited | 40 s | 51 s |

With `//crates/alondrad:alondrad_test` executed locally (about 5 minutes on
the critical path) the cold command took 8.4 min before and 7.1 min after, and
remote cache checks 102.7 s and 28.8 s of wall time. That test timed out at
300 s in three of four local runs, before and after; with 2 reserved CPUs both
versions run at most two tests at once.

Memory was sampled every 2 s while each command ran. Other builds kept the Mac
at the warning level throughout (87–100 % of samples, in both versions). The
`--jobs=64` runs never reached critical and kept at least 21 % free; the
v0.1.8 runs reached critical in 35 % of one warm run's samples and 18 % free.
Swap grew 0.4–2.1 GiB per run in both.

## Orphan quarantine and disk-full completion (30 October 2026)

The live 0.1.8 coordinator (84301) held request
`96533-1793367982921034000` in quarantine with 2 CPU / 2304 MiB. That consumed
the entire warning budget, with requests queued and no managed build running.
Its guardian 96533 and client 78122 were gone; server 78014 remained. Its output
base's command log explicitly reported `No space left on device` during
dependency extraction. Neither a completion receipt nor a saved summary existed.
This does not establish why the guardian died.

The same recovery path remained on freshly fetched main at f7d3cf6. A direct
nonblocking `info server_pid release` query on the affected output base reported
`Killed non-responsive server process (pid=78014)`. Bazel replaced that server;
the coordinator then released the quarantine and status showed request
`42696-1793368547389235000` running. No queue cancellation was sent. The cache
volume had 10 GiB available at inspection time.

The regression suite injects native liveness outcomes, guardian/client state,
elapsed recovery time and ENOSPC without filling any real volume. It checks
successor admission after recovery, preservation of real execution reservations,
late results and reconnects, PID/birth mismatches, authenticated busy versus hung
RPCs, and completion delivery when receipt storage fails. Native contracts use
held actions and isolated HOME/state/output bases; they retain capacity for a
busy server and verify that a stopped orphan server and its captured descendants
exit before a successor starts. Execution and ablation results are recorded with
the PR and the committed validation ledger.
