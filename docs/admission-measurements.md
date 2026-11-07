# Admission research and measurements

## Sources and choices

[Bazel's resource flags](https://bazel.build/versions/9.2.0/reference/command-line-reference)
limit actions within a server. HOST_CPUS/HOST_RAM describe the host, not a share
coordinated between independent servers. Keep one global ledger and pass each
server numeric `--local_resources` shares. Keep `--jobs=64` separate so cache
checks do not wait behind local CPU limits. Experimental load-scheduling flags
are not a substitute for coordinating invocations.

[Ninja pools and GNU jobserver support](https://ninja-build.org/manual.html)
illustrate sharing finite tokens. Take that principle, without introducing
another build system or jobserver protocol.
[GNU Parallel](https://www.gnu.org/software/parallel/man.html) checks free memory
before starting work. Take the check, reject its suspension and kill/requeue
behavior: executing Bazel transactions retain reservations and foreground semantics.
[Buildkite queues](https://buildkite.com/docs/agent/queues) route work to agents;
its [capacity limits](https://buildkite.com/docs/platform/limits) sum machine CPU
shapes. Take summed reservations, not one fixed agent slot per local build.

Alondra #1330, commit `0fd2d0f1837cb940ced1e4561fe933f198e23a20`, uses native
memory percentage, swap-out deltas, anticipated memory growth and CPU overload.
Take those sensors and pending growth. Discard its 60-second memory window,
20%/30% free-memory floors and three-minute warning admission interval: those
protect agent sessions rather than maximizing build throughput. Like that
policy, bazelqueue now reacts to swap-out and swap growth; the
[November measurement](#disk-filled-with-swap-6-november-2026) explains why.
Swap occupancy that no longer grows and compressor size remain diagnostic.

## Disk filled with swap, 6 November 2026

Host: M3 Pro, 18 GiB RAM, v0.1.11. Swap filled the disk five times. `status`
showed warning pressure, 6819 MiB available, swap of 9-20 GB growing about
1 GB/min, three builds running and "full admission", while reservations summed
6 CPU / 8192 MiB. One job reserved 300 MiB while its server used more than 1 GB.

| Finding | Evidence |
|---|---|
| Swap counters never arrived | `swap_in_bytes`, `swap_out_bytes` and `compressor_mib` were null: the sampler required `HOST_VM_INFO64_COUNT` fields, and the kernel returned 62 of 104 with success |
| "Available" included memory in use | `kern.memorystatus_level` gave 6819 MiB; active + inactive + free + speculative was 6754 MiB; free + file-backed + purgeable was 2527 MiB |
| Swap growth was invisible to policy | Only simultaneous swap-in and swap-out above the threshold reduced admission |
| Reservations shrank to fit | Shares of what remained, with a 256 MiB minimum, regardless of the job's server |

Over 30 seconds of that load, swap counters moved 2053 swap-in pages (32 MiB at
16 KiB per page) while occupancy fell 40 MiB, with zero swap-out: the counters
are pages, and recovering pages is not a reason to hold. Available memory follows the
[XNU definition](https://cs.iossec.tech/xnu/latest/source/doc/vm/memorystatus_notify.md),
which counts active pages as available because they can be compressed.

The warning experiment below still stands: reduced admission costs speed. The
measured signals above replace warning as the protection against that swap growth.

## Queue stopped by CPU overload, 6 November 2026

Same host and day, v0.1.11, 11 logical CPUs, 6 CPU / 8192 MiB of capacity. From
`queue.sqlite3`, run summaries and the notices agents received:

| Finding | Evidence |
|---|---|
| Warning limited nothing in that version | `status` read "pressure warning" with "full admission"; one task saw the hold with normal pressure and 7557 MiB available |
| CPU overload held all new work | Above 4 per CPU (load 44) until below 3 (load 33): notices read "CPU overload", and builds started in bursts at 19:14, 20:03 and 21:57 |
| The queue sat empty with jobs waiting | No build ran from 20:25 to 21:34 with about 20 waiting; 10 jobs started between 19:12 and 21:34 |
| Waiting could not clear it | From 20:00 to 22:00 the queue reserved 13-17 % of its CPU and the host stayed at 99 % |
| The cost fell on short work | One job waited 91 minutes to run for 20 seconds, another 142 minutes |

What produced that load was not measured. A hold only helps against load the queue
causes, so CPU overload now halves new work instead of stopping it. Replaying those
23 jobs under that load, with the durations they really ran, the queue is never empty
while jobs wait and all 23 finish inside the 142 minutes in which 10 did.

Swap got the same treatment. Alondra logged 120 swap-out readings between 19:00 and
22:00, all above the 64 MiB/min threshold (lowest 72, hourly medians 238-308). It logs
them only while it holds nodes, so the sample is biased, but a full hold would have
stopped the queue at each of them. One reduced build with nothing else of ours running
keeps it moving; an almost full disk still stops it, because that one a build can make
worse ([7 November](#queue-stopped-by-low-disk-7-november-2026)).

## Queue stopped by low disk, 7 November 2026

Same host, v0.1.12, 10240 MiB of `disk_safety_mib`. Reported as a queue stuck under
warning pressure for hours. From the notices agents received, run summaries and
`queue.sqlite3`:

| Finding | Evidence |
|---|---|
| The hold was disk, not warning | Every notice in the two stopped windows (06:00-07:40, 08:30-10:30) read "low disk space"; `status` showed "pressure warning" because that field is macOS's level |
| Swap kept the disk under the margin | At 05:19: warning, swap 10210 MiB, 9060 MiB free; at 13:03: swap 7693 MiB, 10356 MiB free |
| Nothing started for two hours twice | No run started between 08:25 and 10:30; two jobs waited 1h56m and 2h18m, dozens ended waiting with 143 |

The queue did not fill that disk and waiting did not empty it. Low disk now admits one
reduced build with nothing else of ours running, as swapping does, which bounds what
the queue itself can write (2.2-4.2 GB per output base measured); below half the margin
it still holds. Replaying 23 jobs at 9060 MiB free, the queue is never empty while jobs
wait and every job finishes; before the change it held for the whole replay.

## Before-change ABBA measurement, 30 October 2026

Host: M3 Pro, 11 logical CPUs, 18 GiB RAM. Baseline v0.1.9, commit
`f7d3cf6b42a7a8113feb61297cb82027463133b9`, built with test-fixtures.
All runs used real Bazel 8.4.2 and the same sixteen Clang -O2 compilations of
1800 generated functions. HOME, queue state, sources and caches were isolated
under the task worktree. JVMs were warmed with `info`; action caches were empty,
remote cache reads disabled, jobs=64, one invocation per run.

The isolated pressure input selected full (1) or reduced (2) admission. Independent
kernel telemetry reported warning (2) in every physical sample, never critical.
No installed configuration was changed. VM counters were sampled each second
and at completion; every build succeeded and its final snapshot confirmed the budget.

| Order | CPU / MiB | Total seconds | Minimum free % | Mean host CPU % | Swap-in/out MiB | Compressor MiB before/after |
| --- | --- | --- | --- | --- | --- | --- |
| A1 | 8 / 9216 | 16.141 | 28 | 35.2 | 0.5 / 40.4 | 6624 / 6884 |
| B1 | 2 / 2304 | 25.315 | 29 | 30.9 | 2.2 / 0.0 | 6484 / 6360 |
| B2 | 2 / 2304 | 24.857 | 32 | 25.6 | 1.4 / 0.0 | 6300 / 6416 |
| A2 | 8 / 9216 | 16.447 | 27 | 36.5 | 0.5 / 430.4 | 6230 / 6574 |

Mean full time: 16.294 s; reduced: 25.086 s (**54% longer**). Host CPU is the
mean of Bazel profile system-CPU counters divided by eleven, including other
applications. Swap occupied 6902.94 MiB initially, 6943.31 MiB through the two
reduced runs, and 7373.75 MiB at the end. The last full run caused swap-out but
not corresponding swap-in; this does not establish bidirectional thrashing.

Reported queue wait was 0.0 s in all four runs. The guardian reported backend
run durations of 14.9 / 24.1 / 23.6 / 15.2 s; Bazel elapsed times were
14.828 / 24.081 / 23.524 / 15.152 s. Critical paths were 7.10 / 17.59 / 17.27 /
8.58 s. The total-time table also includes frontend, IPC and preparation overhead.

This small CPU workload supports removing the unconditional warning penalty.
It does not establish causal performance for arbitrary Swift/Rust builds,
prove that parallel invocations always improve makespan, or separate the CPU
effect from the smaller memory budget. Other applications were active.

Reproduce with the **baseline** feature binary, whose pressure input selects
both profiles:

```sh
cargo build --locked --features test-fixtures --bins
python3 tests/pressure_benchmark.py --binary target/debug/bazelqueue \
  --bazel /absolute/path/to/bazel-8.4.2 \
  --output-directory .scratch/pressure-experiment
```

Use a new output directory. It retains results.json with raw native samples,
profiles, logs and queue state. The benchmark shuts down only its own coordinator
and isolated Bazel server. A relative queue path avoids Unix socket path limits
inside long task worktree paths.

## Fast-lane measurement, 5 November 2026

Alondra at `origin/main`, the 22 patterns of its `musts run` (99 targets, 78
tests), Bazel 8.4.2 through bazelqueue, in a throwaway clone with its own output
base. RSS is the Bazel server's resident memory.

| Situation | Result | Wall time | Analysis | Peak RSS |
|---|---|---|---|---|
| Warm server, local action cache current | exit 0 | 3.5–4.8 s | 0.1–0.2 s | 0.70 GB |
| Cold server, local action cache current | exit 0 | 10.5–14.4 s | 4.7–7.4 s | 0.81 GB |
| Cold server, empty local action cache | exit 1 | 11.9 s | 4.7 s | 0.89 GB |
| New output base | exit 1 | 34 s | 19.8 s | — |
| One edited file, warm server | exit 1 | 3.3–3.8 s | 0.4 s | — |

The run that follows reuses the analysis (0.09–0.2 s). A different
`--invocation_policy` budget keeps the analysis cache and `--noblock_for_lock`
keeps the server, but a different worker count restarts the persistent worker
pool, so the check uses the run's worker count.

Only the local action cache counts. With a new output base and full BuildBuddy
and disk caches the check failed with 891 misses; the run right after had 923
remote and 767 disk cache hits. `--remote_cache=` and `bazel clean` give the same
failure. The check passes even when tests Bazel never caches exist; the run then
executes them (three in Alondra, 4 actions, about 5.7 s). Alternating `build` and
`test` in one output base changes the configuration and fails the check.

Exit 0 means up to date. Exit 1 (BUILD_FAILURE) means something would execute,
reported in the BEP as `failureDetail.execution.code = ACTION_NOT_UP_TO_DATE`, but
a real build error also exits 1. Exit 9 means the output base is locked by
another command. Only 0 admits.

bazelqueue's own records cannot replay this: 500 jobs (about 15 hours) and 200
summaries without arguments or executed actions. BuildBuddy received every
invocation: 4,648 completed builds and tests from 22 October to 5 November. An
invocation would have passed when its only local action cache misses were
unconditional test executions and nothing ran locally, remotely or from the disk
cache.

| Group | n | Would pass | Remote or disk cache only | Local work |
|---|---|---|---|---|
| `musts run` (almost all Alondra) | 1,084 | 13 % | 51 % | 36 % |
| All tests | 3,623 | 10 % | 41 % | 49 % |
| All builds | 1,025 | 16 % | 35 % | 49 % |

The first invocation in a workspace passed 0 of 42 `musts run` (6 of 407 overall),
hence the history requirement. After a successful invocation at most 120 s
earlier, 30 % passed; later, 13 %. Runs after a passing check took a median of
14 s (p75 79 s, p90 223 s) because of uncached tests. At the median queue wait of
863 s, the 139 passing `musts run` amount to about 33 hours of agent waiting in
two weeks; about 88 % of checks fail, each costing 3–14 s in its slot.
