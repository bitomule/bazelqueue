#![cfg(feature = "test-fixtures")]
use bazelqueue::{
    config::{Config, Paths},
    coordinator, platform,
    protocol::{Event, Snapshot},
};
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::{
        ffi::OsStringExt,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Harness {
    root: TempDir,
    paths: Paths,
    daemon: Child,
    children: Vec<Child>,
}
impl Harness {
    fn new(parallel: usize) -> Self {
        let root = tempfile::Builder::new()
            .prefix("bqe-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let state = root.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::write(state.join("pressure"), "1").unwrap();
        let paths = Paths {
            root: state.clone(),
            config: state.join("config.toml"),
            database: state.join("queue.sqlite3"),
            socket: state.join("control.sock"),
        };
        Config {
            backend: PathBuf::from(env!("CARGO_BIN_EXE_fixture-backend")),
            cpu_capacity: 8,
            memory_capacity_mib: 4096,
            action_memory_mib: 2048,
            max_builds: parallel,
            pressure_recovery_samples: 1,
            ..Config::default()
        }
        .save(&paths)
        .unwrap();
        let daemon = Command::new(env!("CARGO_BIN_EXE_bazelqueue"))
            .args(["daemon", "run"])
            .env("BAZELQUEUE_HOME", &state)
            .env("HOME", root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut harness = Self {
            root,
            paths,
            daemon,
            children: Vec::new(),
        };
        harness.wait(|snapshot| snapshot.daemon.pid != 0);
        harness
    }
    fn snapshot(&self) -> Option<Snapshot> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        match runtime.block_on(async {
            let mut stream = coordinator::connect(&self.paths).await?;
            bazelqueue::protocol::send(
                &mut stream,
                &bazelqueue::protocol::Message::Control {
                    operation: "status".into(),
                    id: None,
                },
            )
            .await?;
            bazelqueue::protocol::receive::<_, Event>(&mut stream).await
        }) {
            Ok(Event::Snapshot { snapshot }) => Some(snapshot),
            _ => None,
        }
    }
    fn wait(&mut self, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(snapshot) = self.snapshot()
                && predicate(&snapshot)
            {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "condition timed out; daemon log/status: {:?}",
                self.snapshot()
            );
            std::thread::yield_now();
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazelqueue"));
        command
            .env("BAZELQUEUE_HOME", &self.paths.root)
            .env("HOME", self.root.path())
            .current_dir(self.root.path());
        command
    }
    fn spawn_hold(&mut self, label: &str) -> UnixListener {
        let lane = self.root.path().join(label);
        fs::create_dir_all(&lane).unwrap();
        let listener = UnixListener::bind(self.root.path().join(format!("{label}.sock"))).unwrap();
        listener.set_nonblocking(true).unwrap();
        let child = self
            .command()
            .args(["exec", "--", env!("CARGO_BIN_EXE_fixture-backend"), "hold"])
            .arg(listener.local_addr().unwrap().as_pathname().unwrap())
            .current_dir(lane)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        self.children.push(child);
        listener
    }
    fn accepted(listener: &UnixListener) -> UnixStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut ready = [0];
                    stream.read_exact(&mut ready).unwrap();
                    assert_eq!(&ready, b"R");
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "backend never started");
                    std::thread::yield_now();
                }
                Err(error) => panic!("{error}"),
            }
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(snapshot) = self.snapshot() {
            for job in snapshot.jobs {
                if let Some(child) = job.child {
                    let _ = platform::signal_group(child.pid, libc::SIGKILL);
                }
                if platform::alive(&job.request.owner) {
                    platform::signal_pid(job.request.owner.pid, libc::SIGTERM);
                }
            }
        }
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        if let Some(snapshot) = self.snapshot() {
            platform::signal_pid(snapshot.daemon.pid, libc::SIGKILL);
        }
    }
}

#[test]
fn queue_positions_and_fifo_follow_actual_backend_completion() {
    let mut harness = Harness::new(1);
    let first = harness.spawn_hold("first");
    let mut running = Harness::accepted(&first);
    let second = harness.spawn_hold("second");
    harness.wait(|s| s.jobs.iter().filter(|job| job.state == "queued").count() == 1);
    let third = harness.spawn_hold("third");
    let snapshot = harness.wait(|s| s.jobs.iter().filter(|job| job.state == "queued").count() == 2);
    assert_eq!(
        snapshot
            .jobs
            .iter()
            .filter(|job| job.state == "running")
            .count(),
        1
    );
    assert!(second.accept().is_err());
    running.write_all(b"F").unwrap();
    let mut next = Harness::accepted(&second);
    assert!(third.accept().is_err());
    next.write_all(b"F").unwrap();
    let mut last = Harness::accepted(&third);
    last.write_all(b"F").unwrap();
    for child in &mut harness.children {
        assert!(child.wait().unwrap().success());
        let mut error = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut error)
            .unwrap();
        assert!(error.contains("bazelqueue:"));
    }
}

#[test]
fn parallel_admission_never_exceeds_capacity() {
    let mut harness = Harness::new(2);
    let first = harness.spawn_hold("a");
    let mut a = Harness::accepted(&first);
    let second = harness.spawn_hold("b");
    let mut b = Harness::accepted(&second);
    let third = harness.spawn_hold("c");
    let snapshot = harness.wait(|s| s.jobs.iter().filter(|job| job.state == "queued").count() == 1);
    assert_eq!(
        snapshot
            .jobs
            .iter()
            .filter(|job| job.holds_capacity())
            .map(|job| job.request.budget.cpu)
            .sum::<u32>(),
        8
    );
    a.write_all(b"F").unwrap();
    let mut c = Harness::accepted(&third);
    b.write_all(b"F").unwrap();
    c.write_all(b"F").unwrap();
}

#[test]
fn frontend_death_cancels_backend_and_releases_only_after_exit() {
    let mut harness = Harness::new(1);
    let first = harness.spawn_hold("a");
    let _running = Harness::accepted(&first);
    let second = harness.spawn_hold("b");
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    harness.children[0].kill().unwrap();
    harness.children[0].wait().unwrap();
    let mut running = Harness::accepted(&second);
    running.write_all(b"F").unwrap();
    let snapshot = harness.wait(|s| s.jobs.iter().any(|job| job.code == Some(130)));
    assert!(
        snapshot
            .jobs
            .iter()
            .any(|job| job.code == Some(130) && job.terminal())
    );
}

#[test]
fn coordinator_restart_does_not_spawn_a_second_backend() {
    let mut harness = Harness::new(1);
    let first = harness.spawn_hold("a");
    let mut running = Harness::accepted(&first);
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    let snapshot = harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    assert_eq!(snapshot.jobs.len(), 1);
    let second = harness.spawn_hold("b");
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    running.write_all(b"F").unwrap();
    let mut next = Harness::accepted(&second);
    next.write_all(b"F").unwrap();
}

#[test]
fn guardian_death_quarantines_a_still_running_child() {
    let mut harness = Harness::new(1);
    let first = harness.spawn_hold("a");
    let mut running = Harness::accepted(&first);
    let active = harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    let guardian = active.jobs[0].request.owner.pid;
    platform::signal_pid(guardian, libc::SIGKILL);
    harness.wait(|s| s.jobs[0].state == "quarantined");
    let second = harness.spawn_hold("b");
    let snapshot = harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    assert_eq!(snapshot.jobs[0].state, "quarantined");
    running.write_all(b"F").unwrap();
    let mut next = Harness::accepted(&second);
    next.write_all(b"F").unwrap();
}

#[test]
fn stdout_stdin_and_non_utf8_arguments_are_preserved() {
    let harness = Harness::new(1);
    let argument = OsString::from_vec(vec![0xff, b' ', b'\"', 0xfe]);
    let output = harness
        .command()
        .args([
            "exec",
            "--",
            env!("CARGO_BIN_EXE_fixture-backend"),
            "argv",
            "",
        ])
        .arg(&argument)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args: Vec<Vec<u8>> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(args, vec![vec![], vec![0xff, b' ', b'\"', 0xfe]]);
    let mut child = harness
        .command()
        .args(["exec", "--", env!("CARGO_BIN_EXE_fixture-backend"), "echo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"exact\0stdin\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"exact\0stdin\n");
    let result = harness
        .command()
        .args([
            "exec",
            "--",
            env!("CARGO_BIN_EXE_fixture-backend"),
            "exit",
            "9",
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(9));
}

#[test]
fn memory_pressure_blocks_and_recovery_resumes_the_queue() {
    let mut harness = Harness::new(1);
    fs::write(harness.paths.root.join("pressure"), "2").unwrap();
    harness.wait(|s| s.pressure == "warning");
    let listener = harness.spawn_hold("a");
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    fs::write(harness.paths.root.join("pressure"), "1").unwrap();
    let mut running = Harness::accepted(&listener);
    running.write_all(b"F").unwrap();
}

#[test]
fn queued_cancellation_never_executes_the_target() {
    let mut harness = Harness::new(1);
    let first = harness.spawn_hold("first");
    let mut running = Harness::accepted(&first);
    let next = harness.spawn_hold("next");
    let snapshot = harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    let id = snapshot
        .jobs
        .iter()
        .find(|job| job.state == "queued")
        .unwrap()
        .request
        .id
        .clone();
    let output = harness.command().args(["cancel", &id]).output().unwrap();
    assert!(output.status.success());
    harness.wait(|s| {
        s.jobs
            .iter()
            .any(|job| job.request.id == id && job.terminal())
    });
    assert!(next.accept().is_err());
    running.write_all(b"F").unwrap();
}

#[test]
fn lowering_limits_preserves_active_budget_and_reprofiles_waiters() {
    let mut harness = Harness::new(1);
    let first = harness.spawn_hold("first");
    let mut running = Harness::accepted(&first);
    let next = harness.spawn_hold("next");
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    let mut config = Config::load(&harness.paths).unwrap();
    config.cpu_capacity = 4;
    config.memory_capacity_mib = 2048;
    config.action_memory_mib = 1024;
    config.save(&harness.paths).unwrap();
    let snapshot = harness.wait(|s| {
        s.cpu_capacity == 4
            && s.jobs
                .iter()
                .any(|job| job.state == "queued" && job.request.budget.cpu == 4)
    });
    assert_eq!(
        snapshot
            .jobs
            .iter()
            .find(|job| job.state == "running")
            .unwrap()
            .request
            .budget
            .cpu,
        8
    );
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    running.write_all(b"F").unwrap();
    let mut admitted = Harness::accepted(&next);
    admitted.write_all(b"F").unwrap();
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real Bazel run-phase restart"]
fn native_run_target_reconnects_and_accepts_cancel_after_restart() {
    let mut harness = Harness::new(1);
    let socket = harness.root.path().join("target.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    native_workspace(
        &harness,
        &format!(
            r#"genrule(name="runner",outs=["runner.sh"],executable=True,cmd="echo '#!/bin/sh' > $@; echo 'exec {} hold {}' >> $@; chmod +x $@")"#,
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    );
    let mut args = native_args(&harness, "run");
    args.push("//:runner".into());
    let child = native_command(&harness, args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(harness.root.path().join("native-output.log"))
                .unwrap(),
        ))
        .spawn()
        .unwrap();
    harness.children.push(child);
    let _target = Harness::accepted(&listener);
    let snapshot = harness.wait(|s| s.jobs.iter().any(|job| job.state == "run_phase"));
    let id = snapshot.jobs[0].request.id.clone();
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "run_phase"));
    let output = harness.command().args(["cancel", &id]).output().unwrap();
    assert!(output.status.success());
    harness.wait(|s| {
        s.jobs
            .iter()
            .any(|job| job.request.id == id && job.terminal())
    });
    assert!(!harness.children[0].wait().unwrap().success());
}

#[test]
fn setup_is_reversible_and_does_not_overwrite_foreign_files_implicitly() {
    let harness = Harness::new(1);
    let bin = harness.root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("bazelisk"), "original").unwrap();
    let status = harness
        .command()
        .args([
            "setup",
            "--backend",
            env!("CARGO_BIN_EXE_fixture-backend"),
            "--bin-dir",
        ])
        .arg(&bin)
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert_eq!(fs::read(bin.join("bazelisk")).unwrap(), b"original");
    let output = harness
        .command()
        .args([
            "setup",
            "--replace",
            "--backend",
            env!("CARGO_BIN_EXE_fixture-backend"),
            "--bin-dir",
        ])
        .arg(&bin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = harness.command().arg("uninstall").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(bin.join("bazelisk")).unwrap(), b"original");
    assert!(!bin.join("bazel").exists());
}

fn native_backend() -> PathBuf {
    PathBuf::from(
        std::env::var_os("BAZELQUEUE_TEST_BAZEL")
            .expect("set BAZELQUEUE_TEST_BAZEL to the real Bazel/Bazelisk executable"),
    )
}
fn native_args(harness: &Harness, command: &str) -> Vec<OsString> {
    vec![
        "--ignore_all_rc_files".into(),
        format!(
            "--output_user_root={}",
            harness.root.path().join("native-cache").display()
        )
        .into(),
        "--host_jvm_args=-Xmx256m".into(),
        "--max_idle_secs=10".into(),
        command.into(),
    ]
}
fn native_workspace(harness: &Harness, extra: &str) {
    let workspace = harness.root.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        workspace.join("MODULE.bazel"),
        "module(name=\"queue_contract\")\n",
    )
    .unwrap();
    fs::write(
        workspace.join(".bazelversion"),
        format!(
            "{}\n",
            std::env::var("BAZELQUEUE_TEST_VERSION").unwrap_or_else(|_| "8.4.2".into())
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("BUILD.bazel"),
        format!("genrule(name=\"hello\",outs=[\"hello.txt\"],cmd=\"echo hello > $@\")\n{extra}"),
    )
    .unwrap();
    let mut config = Config::load(&harness.paths).unwrap();
    config.backend = native_backend();
    config.managed = true;
    config.save(&harness.paths).unwrap();
}
fn native_command(harness: &Harness, args: Vec<OsString>) -> Command {
    let shim = harness.root.path().join("bazelisk");
    if !shim.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_bazelqueue"), &shim).unwrap();
    }
    let mut command = Command::new(shim);
    command
        .args(args)
        .env("BAZELQUEUE_HOME", &harness.paths.root)
        .env("HOME", harness.root.path())
        .current_dir(harness.root.path().join("workspace"));
    command
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real Bazel in an isolated workspace"]
fn native_build_and_authenticated_idle_probe() {
    let mut harness = Harness::new(1);
    native_workspace(&harness, "");
    let mut args = native_args(&harness, "build");
    args.extend(["//:hello".into(), "--jobs=12".into()]);
    let output = native_command(&harness, args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = harness.wait(|s| s.jobs.iter().any(|job| job.state == "finished"));
    let server = snapshot
        .jobs
        .iter()
        .find_map(|job| job.server.as_ref())
        .unwrap();
    assert_eq!(
        server.version,
        std::env::var("BAZELQUEUE_TEST_VERSION").unwrap_or_else(|_| "8.4.2".into())
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .block_on(bazelqueue::native_server::idle(server))
            .unwrap()
    );
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real Bazel in an isolated workspace"]
fn native_run_preserves_arguments_and_target_exit_nine() {
    let mut harness = Harness::new(1);
    native_workspace(
        &harness,
        r#"genrule(name="runner",outs=["runner.sh"],executable=True,cmd="echo '#!/bin/sh' > $@; echo 'printf \"%s\\n\" \"$$@\"' >> $@; echo 'exit 9' >> $@; chmod +x $@")"#,
    );
    let mut args = native_args(&harness, "run");
    args.extend([
        "//:runner".into(),
        "--".into(),
        "one".into(),
        "two words".into(),
    ]);
    let output = native_command(&harness, args).output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(9),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"one\ntwo words\n");
    harness.wait(|s| {
        s.jobs
            .iter()
            .any(|job| job.state == "finished" && job.code == Some(9))
    });
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real Bazel in an isolated workspace"]
fn native_guardian_loss_holds_capacity_until_client_and_server_finish() {
    let mut harness = Harness::new(1);
    let socket = harness.root.path().join("action.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    native_workspace(
        &harness,
        &format!(
            "genrule(name=\"held\",outs=[\"held.txt\"],tags=[\"local\"],cmd=\"{} hold {}; /usr/bin/head -c 131072 /dev/zero >&2; echo done > $@\")",
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    );
    let mut args = native_args(&harness, "build");
    args.push("//:held".into());
    let native_log = harness.root.path().join("native.log");
    let child = native_command(&harness, args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(&native_log).unwrap()))
        .spawn()
        .unwrap();
    harness.children.push(child);
    eprintln!("before native action barrier: {:?}", harness.snapshot());
    let mut action = Harness::accepted(&listener);
    let snapshot = harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    let job = &snapshot.jobs[0];
    let owner = job.request.owner.pid;
    let server = job.server.as_ref().unwrap().clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        !runtime
            .block_on(bazelqueue::native_server::idle(&server))
            .unwrap()
    );
    platform::signal_pid(owner, libc::SIGKILL);
    harness.wait(|s| s.jobs[0].state == "quarantined");
    let next = harness.spawn_hold("next");
    let waiting = harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    assert_eq!(waiting.jobs[0].state, "quarantined");
    action.write_all(b"F").unwrap();
    eprintln!(
        "released native action; before successor barrier: {:?}",
        harness.snapshot()
    );
    let mut admitted = Harness::accepted(&next);
    admitted.write_all(b"F").unwrap();
    assert!(fs::metadata(&native_log).unwrap().len() >= 131072);
    assert!(
        runtime
            .block_on(bazelqueue::native_server::idle(&server))
            .unwrap()
    );
}

fn control(harness: &Harness, operation: &str) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(matches!(
        runtime
            .block_on(coordinator::control(&harness.paths, operation, None))
            .unwrap(),
        Event::Ack
    ));
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real concurrent native actions"]
fn native_distinct_workspaces_execute_in_parallel_with_bounded_shares() {
    let mut harness = Harness::new(2);
    native_workspace(&harness, "");
    let mut config = Config::load(&harness.paths).unwrap();
    config.cpu_capacity = 7;
    config.save(&harness.paths).unwrap();
    control(&harness, "drain");
    let mut listeners = Vec::new();
    for label in ["one", "two"] {
        let workspace = harness.root.path().join(label);
        fs::create_dir(&workspace).unwrap();
        fs::copy(
            harness.root.path().join("workspace/MODULE.bazel"),
            workspace.join("MODULE.bazel"),
        )
        .unwrap();
        fs::copy(
            harness.root.path().join("workspace/.bazelversion"),
            workspace.join(".bazelversion"),
        )
        .unwrap();
        let socket = harness.root.path().join(format!("{label}.sock"));
        listeners.push(UnixListener::bind(&socket).unwrap());
        fs::write(workspace.join("BUILD.bazel"), format!("genrule(name=\"held\",outs=[\"held.txt\"],tags=[\"local\"],cmd=\"{} hold {}; echo done > $@\")\n", env!("CARGO_BIN_EXE_fixture-backend"), socket.display())).unwrap();
        let mut args = native_args(&harness, "build");
        args.push("//:held".into());
        let child = native_command(&harness, args)
            .current_dir(workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(harness.root.path().join("native-output.log"))
                    .unwrap(),
            ))
            .spawn()
            .unwrap();
        harness.children.push(child);
    }
    harness.wait(|s| s.jobs.iter().filter(|job| job.state == "queued").count() == 2);
    control(&harness, "resume");
    let mut first = Harness::accepted(&listeners[0]);
    let mut second = Harness::accepted(&listeners[1]);
    let snapshot =
        harness.wait(|s| s.jobs.iter().filter(|job| job.state == "running").count() == 2);
    assert_eq!(
        snapshot
            .jobs
            .iter()
            .filter(|job| job.state == "running")
            .map(|job| job.request.budget.cpu)
            .sum::<u32>(),
        6
    );
    assert!(
        snapshot
            .jobs
            .iter()
            .filter(|job| job.state == "running")
            .all(|job| !job.request.budget.exclusive)
    );
    first.write_all(b"F").unwrap();
    second.write_all(b"F").unwrap();
    for child in &mut harness.children {
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn preparing_hook_is_owned_after_guardian_loss_and_cancels_on_frontend_loss() {
    for guardian_loss in [true, false] {
        let mut harness = Harness::new(1);
        let socket = harness.root.path().join("hook.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let mut config = Config::load(&harness.paths).unwrap();
        config.hooks.push(bazelqueue::config::Hook {
            program: env!("CARGO_BIN_EXE_fixture-backend").into(),
            arguments: vec!["hold".into(), socket.to_string_lossy().into_owned()],
        });
        config.save(&harness.paths).unwrap();
        let shim = harness.root.path().join("bazelisk");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_bazelqueue"), &shim).unwrap();
        let child = Command::new(shim)
            .arg("build")
            .env("BAZELQUEUE_HOME", &harness.paths.root)
            .env("HOME", harness.root.path())
            .current_dir(harness.root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        harness.children.push(child);
        let mut hook = Harness::accepted(&listener);
        let snapshot = harness.wait(|s| {
            s.jobs
                .iter()
                .any(|job| job.state == "preparing" && job.child.is_some())
        });
        let job = snapshot
            .jobs
            .iter()
            .find(|job| job.state == "preparing")
            .unwrap();
        let hook_identity = job.child.clone().unwrap();
        config.hooks.clear();
        config.save(&harness.paths).unwrap();
        if guardian_loss {
            platform::signal_pid(job.request.owner.pid, libc::SIGKILL);
            harness.wait(|s| s.jobs.iter().any(|job| job.state == "quarantined"));
            let next = harness.spawn_hold("next");
            harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
            assert!(next.accept().is_err());
            hook.write_all(b"F").unwrap();
            let mut admitted = Harness::accepted(&next);
            admitted.write_all(b"F").unwrap();
        } else {
            harness.children[0].kill().unwrap();
            harness.children[0].wait().unwrap();
            harness.wait(|s| s.jobs.iter().any(|job| job.terminal()));
            assert!(!platform::alive(&hook_identity));
        }
    }
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; released run memory leaves room for a build"]
fn native_run_releases_build_capacity_while_target_remains_alive() {
    let mut harness = Harness::new(1);
    let socket = harness.root.path().join("target.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    native_workspace(
        &harness,
        &format!(
            r#"genrule(name="runner",outs=["runner.sh"],executable=True,cmd="echo '#!/bin/sh' > $@; echo 'exec {} hold {}' >> $@; chmod +x $@")"#,
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    );
    let mut args = native_args(&harness, "run");
    args.push("//:runner".into());
    let child = native_command(&harness, args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(harness.root.path().join("native-output.log"))
                .unwrap(),
        ))
        .spawn()
        .unwrap();
    harness.children.push(child);
    let mut target = Harness::accepted(&listener);
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "run_phase"));
    control(&harness, "drain");
    for _ in 0..2 {
        let mut args = native_args(&harness, "build");
        args.push("//:hello".into());
        let child = native_command(&harness, args)
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(harness.root.path().join("native-output.log"))
                    .unwrap(),
            ))
            .spawn()
            .unwrap();
        harness.children.push(child);
    }
    harness.wait(|s| s.jobs.iter().filter(|job| job.state == "queued").count() == 2);
    control(&harness, "resume");
    let snapshot = harness.wait(|s| {
        s.jobs
            .iter()
            .filter(|job| job.state == "finished" && job.request.command == "build")
            .count()
            == 2
    });
    assert!(snapshot.jobs.iter().any(|job| job.state == "run_phase"));
    assert!(harness.children[1].wait().unwrap().success());
    assert!(harness.children[2].wait().unwrap().success());
    target.write_all(b"F").unwrap();
    assert!(harness.children[0].wait().unwrap().success());
}

#[test]
fn run_footprint_includes_owned_foreground_descendants() {
    let root = tempfile::Builder::new()
        .prefix("bqm-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let socket = root.path().join("memory.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fixture-backend"))
        .arg("tree-hold")
        .arg(socket)
        .spawn()
        .unwrap();
    let mut held = Harness::accepted(&listener);
    let identity = platform::identity(child.id()).unwrap();
    let memory = platform::process_memory(&[identity]);
    held.write_all(b"F").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(
        memory >= 128,
        "descendant allocation was omitted: {memory} MiB"
    );
}

#[test]
fn hot_migration_tracks_late_legacy_calls_across_coordinator_restart() {
    let mut harness = Harness::new(1);
    let shim = harness.root.path().join("bazelisk");
    let json = serde_json::to_string(&vec![shim.clone()]).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(matches!(
        runtime
            .block_on(coordinator::control(
                &harness.paths,
                "track-legacy",
                Some(json)
            ))
            .unwrap(),
        Event::Ack
    ));
    let socket = harness.root.path().join("legacy.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::write(
        &shim,
        format!(
            "{} hold {}; result=$?; exit $result\n",
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    )
    .unwrap();
    let mut legacy = Command::new("/bin/bash")
        .args(["-euo", "pipefail"])
        .arg("bazelisk")
        .current_dir(harness.root.path())
        .spawn()
        .unwrap();
    let mut held = Harness::accepted(&listener);
    let successor = harness.spawn_hold("successor");
    let snapshot = harness.wait(|s| {
        s.jobs.iter().any(|job| job.state == "queued")
            && s.legacy.iter().any(|owner| owner.pid == legacy.id())
    });
    assert!(snapshot.jobs.iter().all(|job| job.state == "queued"));
    assert!(successor.accept().is_err());
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    harness.wait(|s| {
        s.legacy.iter().any(|owner| owner.pid == legacy.id())
            && s.jobs.iter().any(|job| job.state == "queued")
    });
    assert!(successor.accept().is_err());
    held.write_all(b"F").unwrap();
    assert!(legacy.wait().unwrap().success());
    let mut admitted = Harness::accepted(&successor);
    admitted.write_all(b"F").unwrap();
    assert!(harness.children[0].wait().unwrap().success());
}

#[test]
fn legacy_watcher_ignores_shim_paths_passed_as_unrelated_script_data() {
    let mut harness = Harness::new(1);
    let shim = harness.root.path().join("bazelisk");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(coordinator::control(
            &harness.paths,
            "track-legacy",
            Some(serde_json::to_string(&vec![shim.clone()]).unwrap()),
        ))
        .unwrap();
    let socket = harness.root.path().join("unrelated.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let other = harness.root.path().join("other.sh");
    fs::write(
        &other,
        format!(
            "{} hold {}; result=$?; exit $result\n",
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    )
    .unwrap();
    let mut unrelated = Command::new("/bin/bash")
        .arg(other)
        .arg(shim)
        .spawn()
        .unwrap();
    let mut held = Harness::accepted(&listener);
    let successor = harness.spawn_hold("successor");
    let mut admitted = Harness::accepted(&successor);
    assert!(harness.snapshot().unwrap().legacy.is_empty());
    admitted.write_all(b"F").unwrap();
    assert!(harness.children[0].wait().unwrap().success());
    held.write_all(b"F").unwrap();
    assert!(unrelated.wait().unwrap().success());
}

#[test]
fn invalid_resume_retains_coordinator_ownership_and_drained_state() {
    let mut harness = Harness::new(1);
    control(&harness, "drain");
    let successor = harness.spawn_hold("successor");
    let before = harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    let original = fs::read(&harness.paths.config).unwrap();
    fs::write(&harness.paths.config, "cpu_capacity=0\n").unwrap();
    let output = harness.command().arg("resume").output().unwrap();
    assert!(!output.status.success());
    let after = harness
        .snapshot()
        .expect("invalid resume killed coordinator");
    assert_eq!(after.daemon, before.daemon);
    assert!(after.drained);
    assert!(successor.accept().is_err());
    fs::write(&harness.paths.config, original).unwrap();
    control(&harness, "resume");
    let mut admitted = Harness::accepted(&successor);
    admitted.write_all(b"F").unwrap();
    assert!(harness.children[0].wait().unwrap().success());
}

#[test]
fn explicit_execution_keeps_helper_image_after_original_binary_is_removed() {
    let mut harness = Harness::new(1);
    let origin = harness.root.path().join("bazelqueue");
    fs::copy(env!("CARGO_BIN_EXE_bazelqueue"), &origin).unwrap();
    let socket = harness.root.path().join("owned.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let child = Command::new(&origin)
        .args(["exec", "--", env!("CARGO_BIN_EXE_fixture-backend"), "hold"])
        .arg(socket)
        .env("BAZELQUEUE_HOME", &harness.paths.root)
        .env("HOME", harness.root.path())
        .current_dir(harness.root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    harness.children.push(child);
    let mut held = Harness::accepted(&listener);
    let before = harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    let owner = before.jobs[0].request.owner.clone();
    fs::remove_file(origin).unwrap();
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    let after = harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    assert_eq!(after.jobs[0].request.owner, owner);
    assert_eq!(after.jobs[0].child, before.jobs[0].child);
    held.write_all(b"F").unwrap();
    assert!(harness.children[0].wait().unwrap().success());
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; helper retention during native run handoff"]
fn native_run_handoff_survives_original_binary_removal_and_restart() {
    let mut harness = Harness::new(1);
    let socket = harness.root.path().join("build.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    native_workspace(
        &harness,
        &format!(
            r#"genrule(name="runner",outs=["runner.sh"],executable=True,tags=["local"],cmd="{} hold {}; echo '#!/bin/sh' > $@; echo 'printf retained-target' >> $@; chmod +x $@")"#,
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    );
    let origin = harness.root.path().join("bazelisk");
    fs::copy(env!("CARGO_BIN_EXE_bazelqueue"), &origin).unwrap();
    let mut args = native_args(&harness, "run");
    args.push("//:runner".into());
    let child = Command::new(&origin)
        .args(args)
        .env("BAZELQUEUE_HOME", &harness.paths.root)
        .env("HOME", harness.root.path())
        .current_dir(harness.root.path().join("workspace"))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            fs::File::create(harness.root.path().join("native-output.log")).unwrap(),
        ))
        .spawn()
        .unwrap();
    harness.children.push(child);
    let mut build = Harness::accepted(&listener);
    let before = harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    fs::remove_file(origin).unwrap();
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    let after = harness.wait(|s| s.jobs.iter().any(|job| job.state == "running"));
    assert_eq!(after.jobs[0].request.owner, before.jobs[0].request.owner);
    assert_eq!(after.jobs[0].child, before.jobs[0].child);
    build.write_all(b"F").unwrap();
    assert!(harness.children[0].wait().unwrap().success());
    let mut output = Vec::new();
    harness.children[0]
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    assert_eq!(output, b"retained-target");
}

#[test]
fn interrupt_sent_to_frontend_is_forwarded_to_owned_backend() {
    use std::os::unix::process::ExitStatusExt;
    let mut harness = Harness::new(1);
    let listener = harness.spawn_hold("interrupted");
    let _held = Harness::accepted(&listener);
    platform::signal_pid(harness.children[0].id(), libc::SIGINT);
    harness.wait(|s| s.jobs.iter().any(|job| job.terminal()));
    assert_eq!(
        harness.children[0].wait().unwrap().signal(),
        Some(libc::SIGINT)
    );
}
