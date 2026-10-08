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
Reservations stay immutable while executing. Profiles can broaden only before
execution; lowering settings drains active work. Native memory-pressure warning
or unavailable telemetry stops new expensive admission; healthy samples provide
recovery hysteresis. Historical swap usage does not permanently block builds.

Native info discovers actual output-base lanes before execution. Busy native
servers yield preparation capacity rather than monopolizing the queue. Opaque
invocations retain arguments with exclusive admission. Existing project wrappers
are preserved; managed nested requests are rejected instead of deadlocking behind
their parent.

Configuration and runtime state remain per-user. No HTTP UI, remote queue, or
third-party queue daemon is needed. The only TCP connection created by the queue
is a local authenticated Bazel-server recovery probe.

Compatible protocol upgrades reuse the existing coordinator and owned executable
chain. Incompatible future protocols fail closed; automatic cross-protocol drain
and restart is not claimed by this release.
