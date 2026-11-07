# Moving existing checkouts to a per-worktree output base

`tools/worktree-bazel-setup` (see the README) only affects Bazel invocations that start after it
runs. This is the order that keeps every running build alive.

## What not to touch

Leave every output base under `/Volumes/BazelCache/output` and the main checkout's own base alone
while a Bazel server is alive on it or a bazelqueue job is active in it. A build in progress keeps
its old base; the next invocation in that checkout starts in the new one.

Check before touching anything:

```sh
bazelqueue status --json        # any non-terminal job on that output base?
pgrep -fl 'A-server.jar|bazel.*output_base'   # any live server?
```

## Per worktree

1. Run `make worktree-bazel` in the worktree when it is rebased or created. Nothing is forced:
   a worktree that never runs it keeps using the shared output root, as today.
2. `bazel info output_base` must print `<worktree>/.bazel-output`, from the root and from a
   subdirectory.
3. The old server goes idle and `startup --max_idle_secs=120` retires it by itself. At most two
   servers exist for that worktree during the switch (old idle, new running).

The main checkout keeps its base under `output_user_root`, which is today's behaviour. Run
`make worktree-bazel` there only if you want it to follow the same rule.

## Orphaned bases

Once no server and no job use an old base under `/Volumes/BazelCache/output`, remove it by hand,
one at a time, never with `bazel clean` (it works on the current base and leaves the old one):

```sh
base=/Volumes/BazelCache/output/<hash>
bazel --output_base="$base" shutdown   # no-op if the server is already gone
rm -rf "$base"
```

`<hash>` is the directory name; `ls -lt` shows the least recently used first. Skip any base whose
server answers `bazel --output_base="$base" info server_pid` with a live process you did not
expect, and re-run the `status --json` check right before deleting. The external
`~/bin/bazelcache-gc.sh` reaper does the same job; it is no longer needed for worktrees.

## Deleting a worktree

Stop its server first, so no process is left holding a deleted directory:

```sh
bazel shutdown        # from inside the worktree: uses its .bazelrc.local
git worktree remove <path>
```

Delete the worktree only after `bazel shutdown` returns. Removing it takes `.bazel-output` and
its cache with it. The shared disk and repository caches live outside the worktree and stay.
