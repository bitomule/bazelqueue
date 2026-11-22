use std::{env, path::PathBuf};
use tempfile::TempDir;

/// A directory the test owns, inside the temporary directory Bazel gives each test. That directory
/// sits under a Bazel workspace, so the marker keeps lane detection from climbing out of it.
pub fn scratch(prefix: &str) -> TempDir {
    let base = env::var_os("TEST_TMPDIR").map_or_else(env::temp_dir, PathBuf::from);
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .unwrap();
    std::fs::write(directory.path().join("WORKSPACE"), "").unwrap();
    directory
}

/// Unix socket paths hold 104 bytes and Bazel's test directory is longer, so sockets live in the
/// user's own temporary directory. The test owns the directory and removes it when dropped.
pub fn sockets() -> TempDir {
    tempfile::Builder::new()
        .prefix("bq-")
        .tempdir_in(env::temp_dir())
        .unwrap()
}
