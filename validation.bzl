"""Local Cargo compilation bridge; only Bazel executes the test harnesses."""

def _validation_impl(ctx):
    runner = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(
        runner,
        """#!/bin/bash
set -euo pipefail
cd "$TEST_SRCDIR/$TEST_WORKSPACE"
# Resolve the source input, not the runfiles directory: Cargo's reusable build
# trees belong to this checkout. These targets deliberately run only locally.
ROOT=$(python3 -c 'import pathlib; print(pathlib.Path("Cargo.toml").resolve().parent)')
cd "$ROOT"
export CARGO_TARGET_DIR="$ROOT/target/bazel-driver"
exec cargo run --quiet --locked --features test-fixtures --bin validation-driver -- %s "$@"
""" % ctx.attr.mode,
        is_executable = True,
    )
    return [DefaultInfo(
        executable = runner,
        runfiles = ctx.runfiles(files = ctx.files.data),
    )]

validation_test = rule(
    implementation = _validation_impl,
    test = True,
    attrs = {
        "mode": attr.string(mandatory = True),
        "data": attr.label_list(allow_files = True),
    },
)
