//! Development-only bridge from Bazel tests to Cargo-built test executables.
#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{env, ffi::OsString, fs, path::PathBuf, process::Command};

fn main() {
    if let Err(error) = run() {
        eprintln!("validation: {error:#}");
        std::process::exit(1);
    }
}

fn checked(command: &mut Command) -> Result<()> {
    let status = command.status().context("starting validation command")?;
    ensure!(status.success(), "validation command exited with {status}");
    Ok(())
}

fn compile(fixtures: bool) -> Result<Vec<Value>> {
    let mut command = Command::new("cargo");
    command.args(["build", "--locked", "--tests", "--message-format=json"]);
    command.env(
        "CARGO_TARGET_DIR",
        if fixtures {
            "target/bazel-e2e"
        } else {
            "target/bazel-unit"
        },
    );
    if fixtures {
        command.args(["--features", "test-fixtures"]);
    }
    let output = command.output().context("compiling test executables")?;
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    let mut artifacts = Vec::new();
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: Value = serde_json::from_slice(line)?;
        if message["reason"] == "compiler-message" {
            if let Some(rendered) = message["message"]["rendered"].as_str() {
                eprint!("{rendered}");
            }
        } else if message["reason"] == "compiler-artifact"
            && message["profile"]["test"] == true
            && message["executable"].is_string()
        {
            artifacts.push(message);
        }
    }
    ensure!(output.status.success(), "compiling tests failed");
    ensure!(!artifacts.is_empty(), "Cargo produced no test executables");
    Ok(artifacts)
}

fn harnesses(
    fixtures: bool,
    target: Option<&str>,
    filter: Option<&str>,
    ignored: bool,
) -> Result<()> {
    let artifacts = compile(fixtures)?;
    let mut count = 0;
    let mut passed = true;
    for artifact in artifacts {
        if target.is_some_and(|name| artifact["target"]["name"] != name) {
            continue;
        }
        if target == Some("bazelqueue") && artifact["target"]["kind"][0] != "lib" {
            continue;
        }
        let executable = artifact["executable"].as_str().unwrap();
        let mut command = Command::new(executable);
        if let Some(filter) = filter {
            // A misspelled filter must fail rather than produce a green zero-test run.
            let listed = Command::new(executable).args([filter, "--list"]).output()?;
            ensure!(
                listed.status.success()
                    && String::from_utf8_lossy(&listed.stdout).contains(": test"),
                "filter matched no tests"
            );
            command.arg(filter);
        }
        if ignored {
            command.arg("--ignored");
        }
        command.arg("--test-threads=1");
        count += 1;
        passed &= checked(&mut command).is_ok();
    }
    ensure!(count > 0 && passed, "one or more test harnesses failed");
    Ok(())
}

fn run() -> Result<()> {
    let mode = env::args().nth(1).context("missing validation mode")?;
    match mode.as_str() {
        "controller" => controller(),
        "unit" => harnesses(false, None, None, false),
        // Preserve the original feature-enabled suite, including its unit tests.
        "e2e" => harnesses(true, None, None, false),
        "native" => {
            ensure!(
                env::var_os("BAZELQUEUE_TEST_BAZEL").is_some(),
                "set BAZELQUEUE_TEST_BAZEL to the native Bazel executable"
            );
            ensure!(
                env::var("BAZELQUEUE_TEST_VERSION").is_ok_and(|value| !value.is_empty()),
                "set BAZELQUEUE_TEST_VERSION to the native Bazel version"
            );
            harnesses(true, Some("e2e"), Some("native_"), true)
        }
        "regression" => {
            let unit = harnesses(
                false,
                Some("bazelqueue"),
                Some("a_failed_legacy_discovery_queues_a_prepared_check_instead_of_failing_it"),
                false,
            );
            let e2e = harnesses(
                true,
                Some("e2e"),
                Some(
                    "an_unreadable_live_shell_holds_a_checked_build_in_the_queue_instead_of_failing_it",
                ),
                false,
            );
            unit.and(e2e)
        }
        "terminal" => {
            checked(
                Command::new("cargo")
                    .args(["build", "--locked", "--features", "test-fixtures", "--bins"])
                    .env("CARGO_TARGET_DIR", "target/bazel-e2e"),
            )?;
            let root = env::current_dir()?.join("target/bazel-e2e/debug");
            checked(
                Command::new("python3")
                    .arg("tests/terminal_e2e.py")
                    .env("BAZELQUEUE_TEST_BINARY", root.join("bazelqueue"))
                    .env("BAZELQUEUE_TEST_FIXTURE", root.join("fixture-backend")),
            )
        }
        "package" => {
            let archive = PathBuf::from(
                env::var_os("BAZELQUEUE_TEST_ARCHIVE").context("set BAZELQUEUE_TEST_ARCHIVE")?,
            );
            let version =
                env::var("BAZELQUEUE_TEST_VERSION").context("set BAZELQUEUE_TEST_VERSION")?;
            checked(
                Command::new("bash")
                    .arg("packaging/smoke.sh")
                    .arg(archive)
                    .arg(version),
            )
        }
        _ => bail!("unknown validation mode"),
    }
}

fn controller() -> Result<()> {
    let backend = env::var_os("BAZELQUEUE_TEST_BAZEL").context("set BAZELQUEUE_TEST_BAZEL")?;
    let version = Command::new(&backend).arg("--version").output()?;
    ensure!(
        version.status.success()
            && String::from_utf8_lossy(&version.stdout).trim() == "bazel 9.2.0",
        "the controller regression requires the official Bazel 9.2.0 binary"
    );
    let root = env::current_dir()?;
    let workspace = PathBuf::from(env::var_os("TEST_TMPDIR").context("run through Bazel")?)
        .join("controller-workspace");
    fs::create_dir_all(&workspace)?;
    fs::write(
        workspace.join("MODULE.bazel"),
        "module(name=\"controller_probe\")\n",
    )?;
    fs::write(
        workspace.join("BUILD.bazel"),
        "filegroup(name=\"source\",srcs=[\"MODULE.bazel\"])\n",
    )?;
    let mut rc = OsString::from("--bazelrc=");
    rc.push(root.join(".bazelrc"));
    let mut cache = OsString::from("--output_user_root=");
    cache.push(workspace.join(".build/cache"));
    checked(
        Command::new(backend)
            .args([
                "--batch",
                "--nosystem_rc",
                "--nohome_rc",
                "--noworkspace_rc",
            ])
            .arg(rc)
            .arg(cache)
            .arg("--host_jvm_args=-Xmx256m")
            .args(["build", "--nobuild", "--lockfile_mode=off", "//:source"])
            .current_dir(workspace),
    )
}
