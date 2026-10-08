#!/bin/bash
set -euo pipefail
ARCHIVE=$1
VERSION=$2
STAGING=$(mktemp -d)
trap 'rm -rf "$STAGING"' EXIT
PREFIX=bazelqueue-aarch64-apple-darwin
printf '%s\n' "$PREFIX/" "$PREFIX/LICENSE-APACHE" "$PREFIX/LICENSE-MIT" "$PREFIX/README.md" "$PREFIX/bazelqueue" | LC_ALL=C sort > "$STAGING/expected"
tar -tJf "$ARCHIVE" | LC_ALL=C sort > "$STAGING/actual"
diff -u "$STAGING/expected" "$STAGING/actual"
tar -xJf "$ARCHIVE" -C "$STAGING"
BAZELQUEUE_HOME="$STAGING/state" "$STAGING/$PREFIX/bazelqueue" --version | tee "$STAGING/version"
test "$(cat "$STAGING/version")" = "bazelqueue $VERSION"
BAZELQUEUE_HOME="$STAGING/state" "$STAGING/$PREFIX/bazelqueue" package formula --archive "$ARCHIVE" --release-version "$VERSION" --output "$STAGING/bazelqueue.rb"
ruby -c "$STAGING/bazelqueue.rb"
if grep -Eq 'bin\.install(_symlink)? .*"(bazel|bazelisk)"' "$STAGING/bazelqueue.rb"; then
  echo 'formula would own a shared Bazel command' >&2
  exit 1
fi
