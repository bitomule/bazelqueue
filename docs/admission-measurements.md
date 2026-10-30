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
protect agent sessions rather than maximizing build throughput. Unlike that
policy, bazelqueue requires both swap-in and swap-out above 64 MiB/min to signal
thrashing. Swap occupancy and compressor size alone remain diagnostic.

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

Reproduce with the **baseline** feature binary (a newer policy no longer maps
warning to reduced admission):

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
