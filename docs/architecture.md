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

Quarantine is a recovery state, not a terminal result. Recovery first verifies
that the owned client has exited; the guardian may remain alive or suspended once
the request is already quarantined. This does not reclassify a running request
just because its client exited. An authenticated nonblocking info command
distinguishes an idle server from a busy one. The independent authenticated Ping
RPC distinguishes an unresponsive server from a command whose info probe failed.
A responsive server with an unsuccessful or timed-out info probe is never killed.

After ten seconds of repeated unresponsive observations, recovery may stop only
the recorded server of an orphan request, with no live owned client or other
active queue owner of that workspace/output base. It repeats the liveness probe,
verifies the current PID/birth identity, private server metadata and discovered
workspace/output base, and commits the server's retirement intent before any
signal. Retirement stops that exact server, confirms process-wide suspension,
and checks its current direct children through native APIs. A server with children
or uncertain evidence is resumed and keeps its reservation; no descendant of an
unresponsive server is signalled. Only a verified stopped leaf server may be killed, and its
reservation lasts until exit is observed. Restart repeats these checks; a saved
intent never authorizes an unconditional kill. This uses Bazel's supervised
process tree; daemons deliberately detached by a target are outside invocation
tracking. A replacement server is probed
but never targeted as the old server. Async results are discarded if the job or
its connection changed while probing. No timeout directly frees capacity.

A deadlocked Bazel server can leave a metered job `running` with nothing moving,
and once its output base is deleted recovery cannot verify it either. The
coordinator therefore looks at the server of every watched job: a running job
whose guardian declared its progress, and any quarantined Bazel job. A look reads
only typed signals: the size and modification time of the Build Event Protocol
file and of `command.log`, the server's descendants, the CPU of its process tree,
and the connections established on the ports it listens on. It never opens a log.

`stall::Watch` is pure and takes each reading with its own monotonic time. Silence
ends when either file changes or the tree spends more than 5% of one core across
`stall_minutes`, and a signal that cannot be read restarts it, so elapsed time
alone proves nothing. The two thresholds come from measurement. A real Bazel 8.4.2
running a sleeping test left the event file untouched for 170 seconds and used
1.1% of a core, while `command.log` still advanced every 60 seconds (at most 151
seconds during loading). The server that deadlocked used at most 2.4% and its
`command.log` stopped for good. That progress line is what separates a slow test
from a hung server, so the default window of 10 minutes is four times the longest
measured gap. Children do not exempt a job, because idle persistent workers and
tests of a hung server stay children; a test run with Bazel's progress output
turned off and sleeping for the whole window is the known false positive.

Bazel 9.2.0 writes no `command.log` by default, and a sleeping test there moved
neither the event file nor anything else for 175 seconds, so that heartbeat does
not exist. Without it, a job with children and a connected client is never cut:
only Bazel's own timeouts apply. A job without children still is, once silent and
flat, which means a long remote action or download that shows no local activity
can be cut on Bazel 9. Passing `--write_command_log` restores the heartbeat.

A stall on a running job is stored with exit code 124 and sent to the guardian as
`Event::Stalled`. The guardian cuts its client through the usual interrupt and kill
sequence, then reports 124 and `TIMEOUT`. Only guardians that declared progress are
cut this way: older ones do not understand the event, and are judged once they die.
An exit the guardian already reported stays the result.

A quarantined job leaves when its server is proven stalled, even if the server
could not be verified over RPC: the PID and birth match the recorded identity,
the process argv still names the job's workspace and output base, the guardian is
gone, no other active owner shares the lane, and no client is connected. A live
client whose guardian died is killed first. A watched job with no recorded server,
as when its preparation timed out under load, adopts one only when exactly one
process serves its workspace. Retirement stops the
server, repeats those checks, stops and kills its descendants through their own
identities, then kills the server. The reservation lasts until the exit is
observed. The evidence lives only in the running coordinator: a restart starts the
silence again, and a stopped server is resumed if progress or a client appears
before the kill.

Recovery records `finished`, preserving any known exit code or using 125 when
the result was lost, and a bounded diagnosis visible in status and wait. It does
not resubmit the command. A disk-full receipt write cannot suppress the Finish
message: the receipt and socket are independent completion paths. New optional
reason/process fields default when old records are read; receipt tuples and the
existing terminal states remain compatible. A journal write failure prevents
new admission rather than allowing work without a durable reservation.

The scheduler reserves CPU tokens and invocation memory, including overhead.
A managed build or test with history reserves an estimate: the guardian samples
the CPU time of its Bazel server's process tree (live members plus reaped
children, so workers, actions and tests all count) before and after the client
runs, keyed by repository, command and the arguments that change what gets built;
a run without its own history takes the median of its repository and command. Estimated reservations are
bounded by summed CPU and memory rather than `max_builds`. Large estimates share
the remaining machine with other waiting lanes; a lone invocation can use the
wide profile. CPU division assigns the remainder rather than losing a core.
Admission goes by priority (`Request::priority`: high, normal, low) and then first
come first served among available lanes, and a job that does
not fit does not stop the ones behind it: those that fit start, and `Job::passed`
counts them. After four passes nothing starts ahead of that job until it does, so
large and opaque work cannot starve. Durations are too unreliable to reserve its
start as EASY backfilling does, so the bound is a count. A job whose workspace is
busy neither blocks nor counts. `waiting_order` is the one place that orders the queue. Priority only reorders it:
it never raises a pressure limit, cuts a running job or changes a reservation, and
there is no aging, so `low` can wait while higher work keeps arriving. Old guardians
and stored jobs have no priority and read as `normal`.
Reservations stay immutable while executing. Profiles can broaden only before
execution; lowering settings drains active work. Monitor reports one typed
`Limit`; the display reason derives from it and no decision reads text. A limit
that waiting cannot clear must not stop the queue. CPU overload admits new work
while the reservations stay below half the capacity, each with at least
`worker_instances` CPUs. Swapping and critical pressure admit one managed build
with the reduced budget, only with no metered job holding capacity, and opaque work
waits. Low disk and any memory, swap or disk sensor without data hold new metered
admission, and rank above the others. Warning alone is no limit. The next safe
sample restores normal admission.

Native samples arrive each second: macOS pressure from sysctl, memory and swap
counters from host_statistics64, swap occupancy from vm.swapusage, and CPU gauges.
Free memory is free + file-backed + purgeable pages, without counting speculative
pages twice. kern.memorystatus_level is not used: it includes active anonymous
pages, and reported 6819 MiB when only 2527 MiB could be had without compressing
or swapping. The kernel returns fewer host_statistics64 fields than current
headers declare, so the sampler requires only those it reads. Disk space comes
from statvfs on the state directory and each live job's lane, keeping the lowest.

With the output base inside each worktree, the lane of a job is that worktree's own
`.bazel-output`, so the lowest free space is the lowest across the volumes the live worktrees
sit on, with no Bazel-specific code. Admission needs nothing else to support this: N building
worktrees mean N servers and N independent lanes, each ordered by its own server lock, each
with its own LaneMemory peak, and idle servers leave by Bazel's `max_idle_secs`. The e2e
`each_worktree_output_base_is_its_own_lane` and the unit test
`disk_is_measured_on_the_lane_of_every_job_still_in_play` pin both behaviours.

Swapping means swap-out or occupancy growth above swap_thrashing_mib_per_min across
a 60-second window, so pauses between bursts are not recovery and a burst
right after start is not a sustained rate. Swap-in alone and
old occupancy with no growth are not limits. Recovery needs both rates; a reset
counter, a missing reading or a non-increasing timestamp produces no rate and
leaves a known swapping state in place. Until the first rates exist, and five
seconds after the last ones, admission holds. Missing memory can reuse the last
reading for five seconds, then holds admission. CPU load above four per logical
core holds new admission until below three, without changing active reservations.

The free-memory margin is max(memory_safety_mib, 5% RAM), initially 1024 MiB.
Fresh admission headroom subtracts growth promised to active work:
sum(max(reserved MiB - observed resident MiB, 0)). It then deducts every new grant.
This avoids both counting resident memory twice and giving multiple starts the
same still-free memory. The configured global budget remains an independent cap,
including the memory of live run targets, and sums each active job as the larger
of its reservation and its measured tree. Reservations themselves stay immutable.

Resident memory covers the job's child and Bazel server with their descendants.
LaneMemory keeps the peak of the latest three builds per output base in the
settings table, and fast-lane runs do not count. A queued build's reservation
is raised to that peak plus a quarter, at least the fast-lane budget within its
max_builds share, capped by what admission could grant. A start takes from
headroom only what its live server has not already taken. A build that does not
fit real headroom waits. Only with no metered job holding
capacity and admission not held may the first queued build exceed headroom: free
memory is cautious, since macOS can still compress, and the swap signal stops
anything after it. Sampling and process-tree inspection stay outside the deterministic
scheduler; tests inject resources, residency and time.
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
A queued managed build or test may be checked once in a fast-lane slot before
its turn: at most `fast_lane_slots` at a time, each reserving the worker count in
CPUs (so Bazel's persistent worker pool keeps its size) and 1536 MiB of real
headroom, outside the build capacity. A slot is never granted while admission is
held, draining or legacy work drains, while the workspace's lane is busy
or earlier work waits in it, or before a build or test of that workspace has
finished successfully. The coordinator sends the slot's budget with the check. In the
slot the guardian runs the hooks and the info probe, then the caller's invocation
with `--noblock_for_lock`, the up-to-date check flag and no build event outputs,
as an owned executor behind the same permit barrier. Only exit code 0 admits, and only while
admission is not held: the request moves straight to starting in its slot
and does not record an estimate. Anything else returns it, prepared and with its own budget, to its
original queue position; a busy server returns it unprepared. Fast-lane state is
an optional job field, so older journals and coordinators keep the ordinary path.
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
