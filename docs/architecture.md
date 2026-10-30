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
that the owned client has exited; a completed guardian can also supply that
evidence without disappearing itself. An authenticated nonblocking info command
distinguishes an idle server from a busy one. The independent authenticated Ping
RPC distinguishes an unresponsive server from a command whose info probe failed.
A responsive server with an unsuccessful or timed-out info probe is never killed.

After ten seconds of repeated unresponsive observations, recovery may stop only
the recorded server of an orphan request, with no live owned client or other
active queue owner of that workspace/output base. It repeats the liveness probe,
verifies the current PID/birth identity, private server metadata and discovered
workspace/output base, and commits captured descendant identities before
signalling them. Those identities retain the reservation across a coordinator
restart until every captured process has exited. A replacement server is probed
but never targeted as the old server. Async results are discarded if the job or
its connection changed while probing. No timeout directly frees capacity.

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
bounded by summed CPU and memory rather than `max_builds`, admission stays first
come first served, and warning pressure still admits one reduced build at a time.
Reservations stay immutable while executing. Profiles can broaden only before
execution; lowering settings drains active work. Native memory-pressure warning
admits one build at a time with a reduced budget; critical pressure stops new
admission; healthy samples provide recovery hysteresis before full capacity. Historical swap usage does not permanently block builds.

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
