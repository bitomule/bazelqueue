# Installation and migration

Homebrew installs bin/bazelqueue and private libexec/shims/{bazel,bazelisk}.
It never owns Homebrew's shared bin/bazel or bin/bazelisk. Setup installs owned
user links and, for supported zsh/bash environments requiring it, a reversible
PATH blocks. Zsh activation honors ZDOTDIR and the final login/interactive
startup stages. Bash keeps the existing active login profile. Existing process environments cannot be rewritten: new shells and
agents must inherit the activated PATH. Doctor and command resolution should be
checked before assuming interception.

Setup previews foreign destinations before an explicit --replace. Its native
installation journal captures before/after images for configuration, executable
selection, links, backups, shell integration and manifest before mutation. A
committed transaction finalizes; an interrupted one rolls back idempotently.
Original uninstall backups remain unchanged through upgrades. Uninstall removes
only owned links and unchanged owned PATH blocks, preserving independent user
edits. It refuses active requests until the queue has drained.

The control binary remains Homebrew-owned. User interception uses a private retained executable chain. Every frontend,
including explicit `bazelqueue exec`, selects a retained helper image before
starting its guardian. Homebrew can remove an old keg without removing that
guardian’s executable for later handoff or coordinator recovery. `bazelqueue setup` is the upgrade step after
`brew upgrade`: until it runs, the old coordinator keeps running the old
retained executable, and `status`/`doctor` report the version difference.

After the transaction commits, setup compares the running coordinator's
reported version and executable with the installed `current` executable. A
different coordinator is asked to retire (it refuses while any request holds
capacity), and a coordinator too old to know that operation receives SIGINT
only after the control socket's peer uid and pid and the process birth identity
match its status snapshot. Setup waits for it to exit, starts the successor
from `current`, and resumes the queue there. Queued requests keep their place
and their guardians reconnect. If the replacement fails, setup resumes the old
coordinator when it accepts the configuration; otherwise it names the process
to stop and the command to run, and leaves the transaction pending so the next
setup retries the replacement. Preserve the backend's opt-stable absolute path.

On this Mac, --migrate records existing shell shim process birth identities and
adds the existing cache guard and bounded external-rc synchronization as optional
local hooks. Hooks run before native server discovery under exclusive preparation.
A persistent source-script watcher also catches legacy calls arriving during
the cutover, including relative script paths. The old processes are allowed to finish; new requests wait while their ownership
remains active. The old slot TTL is not used to grant new capacity. Original
scripts and symlink targets are backed up privately; caches and credentials are
not moved into this repository.

The initial production default is one expensive invocation. Calibrate before
increasing max_builds. CPU and memory capacity are global; a concurrently admitted
job receives a share rather than independently claiming the whole host. Opaque
requests remain exclusive. A lone supported job can use the wider profile.

For recovery, inspect bazelqueue status --json and doctor. Supported quarantined
servers are checked automatically. If native process identity cannot be verified,
capacity remains held; do not delete locks or the database to bypass that state.
Use the native client's controlled cancellation/shutdown and retain diagnostics.
