# Architecture

The package contains one Rust binary with CLI/shim, coordinator, guardian and
internal executor roles. The scheduler is pure; native APIs, SQLite and process
creation live behind separate modules. Backend argv uses OsString and is never
joined into a shell command.

The coordinator owns one per-user lock and Unix socket. Peer UID verification,
private state directories, bounded framed messages and process birth identities
prevent independent coordinators or PID-only ownership assumptions. Persistent
frame buffers survive selected read cancellation. The SQLite journal commits
reservations before granting them; completed history is bounded.

The guardian stays beside the calling terminal. A private liveness socket tells
it when the frontend disappears. Each actual backend, preparation probe and run
target begins as an internal executor blocked on a private permit. The guardian
records its identity, waits for the correlated ownership ACK, then releases the
permit. Crashing before that ACK cannot start real work.

When a request ends, the guardian writes its summary before telling the
coordinator, so a request the coordinator reports as finished always has its
summary on disk unless the guardian died first. Managed Bazel invocations get a
private Build Event Protocol JSON file the guardian reads for failed tests and
targets and then deletes; abandoned event files older than a day and summaries
beyond the newest 200 are pruned. `bazelqueue wait` reads the coordinator's
snapshot until the request is terminal, then the saved summary; when none exists
(the guardian died, or it was cancelled by recovery) it reports the coordinator's
recorded exit code.

The coordinator has its own process group, so suspending a foreground caller
cannot suspend the global queue. TTY execution hands foreground ownership to
owned child groups and restores it afterwards. Ctrl-C, Ctrl-Z/fg, stdin and isatty
are tested with a real interactive shell and PTY; pipes use inherited descriptors.

Guardians reconnect to a restarted coordinator using their durable request ID.
An owned run phase is an idempotent same-child transition. Native recovery never
trusts a dead preflight JVM: same-workspace servers are rediscovered and probed
through Bazel's authenticated CommandServer RPC with block_for_lock=false. The
cookie, current process birth identity, final command result and returned PID
must agree. Unknown work keeps its reservation until evidence is available.

The scheduler reserves CPU tokens and invocation memory, including overhead.
A managed build or test with history reserves an estimate: the guardian samples
the CPU time of its Bazel server's process tree (live members plus reaped
children, so workers, actions and tests all count) before and after the client
runs, keyed by repository, command and the arguments that change what gets built;
a run without its own history takes the median of its repository and command. Estimated reservations are
bounded by summed CPU and memory rather than `max_builds`. Large estimates share
the remaining machine with other waiting lanes; a lone invocation can use the
wide profile. CPU division assigns the remainder rather than losing a core.
Admission stays first come first served among available lanes.
Reservations stay immutable while executing. Profiles can broaden only before
execution; lowering settings drains active work. Warning has the same admission
capacity as normal. Critical pressure or swap-in AND swap-out rates above
64 MiB/min admit at most one managed build with the configured reduced budget,
subject to memory headroom. Opaque work waits in that mode. The next safe sample
restores normal admission, including when native pressure remains warning.
Historical swap occupancy and compressor size are diagnostic only. A known
thrashing state survives missing/reset counters until valid rates prove recovery.

Native samples arrive each second: macOS pressure and available percentage from
sysctl, VM counters from host_statistics64, and CPU gauges. Memory falls back to
sysinfo availability. Missing memory can reuse the last reading for five seconds,
then holds admission. Swap counter resets, missing counters and non-increasing
timestamps produce no rate. CPU load above four per logical core holds new
admission until below three, without changing active reservations.

The free-memory margin is max(memory_safety_mib, 5% RAM), initially 1024 MiB.
Fresh admission headroom subtracts growth promised to active work:
sum(max(reserved MiB - observed resident MiB, 0)). It then deducts every new grant.
This avoids both counting resident memory twice and giving multiple starts the
same still-free memory. The configured global budget remains an independent cap,
including the memory of live run targets. Sampling and process-tree inspection
stay outside the deterministic scheduler; tests inject both resources and time.
When availability is reused, its original residency snapshot is reused too.
Unused small-job shares are redistributed to larger waiting jobs before grants.
Opaque invocations own the CPU lane exclusively and account for the lesser of
real headroom and the configured memory remaining after run targets; their argv
is unchanged, so this accounting estimate does not impose a native memory quota.

Hourly utilization is persisted under the existing settings table, with no job
format or schema-version change. It integrates state transitions and splits
intervals at UTC hour boundaries. Busy time means at least one metered job in
running; reservation use includes preparation, starting and quarantine. Host CPU
is sampled separately. The latest 24 hourly buckets carry observed coverage;
restart gaps, sample gaps above five seconds and clock jumps are unknown. New
snapshot fields have serde defaults for compatible protocol-v1 upgrades.
An accepted UTC watermark is persisted so clock rewinds cannot count an interval
twice. Rejected forward jumps do not advance that watermark or prune history.

Native info discovers actual output-base lanes before execution. Busy native
servers yield preparation capacity rather than monopolizing the queue. Opaque
invocations retain arguments with exclusive admission. Read-only and utility
commands (info, query, cquery, aquery, help, version, canonicalize-flags,
analyze-profile, dump, license) are unmetered and admitted at once; Bazel's own
server lock still orders them within one output base. clean and shutdown are
unmetered maintenance: they wait only for their own workspace and output base.
Preflight hooks run inside the request's own reservation, one invocation at a
time under a host-wide hook lock. Existing project wrappers
are preserved; managed nested requests are rejected instead of deadlocking behind
their parent.

Configuration and runtime state remain per-user. No HTTP UI, remote queue, or
third-party queue daemon is needed. The only TCP connection created by the queue
is a local authenticated Bazel-server recovery probe.

Compatible protocol upgrades reuse the existing coordinator and owned executable
chain until `setup` commits; it then replaces a coordinator whose version or
executable differs from the installed one, so the configuration it wrote is
loaded by the matching release. Incompatible future protocols fail closed; automatic cross-protocol drain
and restart is not claimed by this release.
