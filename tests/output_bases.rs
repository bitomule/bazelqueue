use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

fn run(home: &Path, volume: &Path, extra: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bazelqueue"))
        .env("HOME", home)
        .env("BAZELQUEUE_HOME", home.join("state"))
        .arg("output-bases")
        .arg(volume)
        .args(extra)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn base(volume: &Path, name: &str, checkout: &Path) {
    let base = volume.join("bazel").join(name);
    fs::create_dir_all(base.join("server")).unwrap();
    fs::write(
        base.join("DO_NOT_BUILD_HERE"),
        checkout.display().to_string(),
    )
    .unwrap();
}

#[test]
fn the_command_lists_unused_bases_and_only_deletes_them_when_asked() {
    let dir = TempDir::new().unwrap();
    let volume = dir.path().join("volume");
    let alive = dir.path().join("checkout");
    fs::create_dir_all(&alive).unwrap();
    base(&volume, "kept", &alive);
    base(&volume, "unused", &dir.path().join("deleted-checkout"));

    let (listed, output) = run(dir.path(), &volume, &[]);
    assert!(listed);
    assert!(output.contains("unused"), "{output}");
    assert!(!output.contains("kept"), "{output}");
    assert!(output.contains("rerun with --delete"), "{output}");
    assert!(volume.join("bazel/unused").exists());

    let (deleted, output) = run(dir.path(), &volume, &["--delete"]);
    assert!(deleted);
    assert!(output.contains("Deleted 1 unused output base"), "{output}");
    assert!(!volume.join("bazel/unused").exists());
    assert!(volume.join("bazel/kept").exists());
}

#[test]
fn a_volume_with_nothing_unused_says_so() {
    let dir = TempDir::new().unwrap();
    let (ok, output) = run(dir.path(), &dir.path().join("empty-volume"), &["--delete"]);
    assert!(ok);
    assert!(output.contains("No unused output bases"), "{output}");
}
