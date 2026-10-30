#![cfg(feature = "test-fixtures")]
use bazelqueue::{
    config::{Config, Paths},
    coordinator,
    platform::{self, Identity},
    protocol::{Event, Snapshot},
};
use std::{
    cell::RefCell,
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
    seen: RefCell<Seen>,
}
/// Processes the harness launched indirectly (guardians, executors, relaunched
/// coordinators); it must outlive them all before the state directory goes.
#[derive(Default)]
struct Seen {
    guardians: Vec<Identity>,
    groups: Vec<Identity>,
    daemons: Vec<Identity>,
}

/// A failed assertion must not leave this test's native server/guardian stopped.
struct ResumeOnDrop(Identity);
impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        let _ = platform::signal_verified(&self.0, libc::SIGCONT);
    }
}
impl Seen {
    fn record(&mut self, snapshot: &Snapshot) {
        fn add(list: &mut Vec<Identity>, identity: &Identity) {
            if !list.contains(identity) {
                list.push(identity.clone());
            }
        }
        add(&mut self.daemons, &snapshot.daemon);
        for job in &snapshot.jobs {
            add(&mut self.guardians, &job.request.owner);
            if let Some(child) = &job.child {
                add(&mut self.groups, child);
            }
        }
    }
}
impl Harness {
    fn new(parallel: usize) -> Self {
        Self::with_coordinator(parallel, None)
    }
    fn with_coordinator(parallel: usize, fixture: Option<&str>) -> Self {
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
        let mut daemon = Command::new(env!("CARGO_BIN_EXE_bazelqueue"));
        daemon
            .args(["daemon", "run"])
            .env("BAZELQUEUE_HOME", &state)
            .env("HOME", root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(fixture) = fixture {
            daemon.env("BAZELQUEUE_FIXTURE_COORDINATOR", fixture);
        }
        let daemon = daemon.spawn().unwrap();
        let mut harness = Self {
            root,
            paths,
            daemon,
            children: Vec::new(),
            seen: RefCell::default(),
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
            Ok(Event::Snapshot { snapshot }) => {
                self.seen.borrow_mut().record(&snapshot);
                Some(snapshot)
            }
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
        self.spawn_hold_as(label, None)
    }
    fn spawn_hold_as(&mut self, label: &str, release: Option<&str>) -> UnixListener {
        let lane = self.root.path().join(label);
        fs::create_dir_all(&lane).unwrap();
        let listener = UnixListener::bind(self.root.path().join(format!("{label}.sock"))).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut command = self.command();
        if let Some(release) = release {
            command.env("BAZELQUEUE_FIXTURE_COORDINATOR", release);
        }
        let child = command
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
fn settle<'a>(processes: impl IntoIterator<Item = &'a Identity>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    for process in processes {
        while platform::alive(process) {
            if Instant::now() >= deadline {
                assert!(
                    std::thread::panicking(),
                    "process {} outlived the harness",
                    process.pid
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
fn stop(seen: &Seen) {
    for group in &seen.groups {
        if platform::alive(group) {
            let _ = platform::signal_group(group.pid, libc::SIGKILL);
        }
    }
    for guardian in &seen.guardians {
        if platform::alive(guardian) {
            platform::signal_pid(guardian.pid, libc::SIGKILL);
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.snapshot();
        stop(&self.seen.borrow());
        let deadline = Instant::now() + Duration::from_secs(5);
        for child in &mut self.children {
            if matches!(child.try_wait(), Ok(None)) {
                platform::signal_pid(child.id(), libc::SIGTERM);
            }
        }
        for child in &mut self.children {
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.snapshot();
        let seen = self.seen.take();
        stop(&seen);
        settle(seen.guardians.iter().chain(&seen.groups));
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        for daemon in &seen.daemons {
            if platform::alive(daemon) {
                platform::signal_pid(daemon.pid, libc::SIGKILL);
            }
        }
        settle(&seen.daemons);
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
fn a_failed_backend_is_terminal_and_visible_in_status() {
    let harness = Harness::new(1);
    let output = harness
        .command()
        .args([
            "exec",
            "--",
            env!("CARGO_BIN_EXE_fixture-backend"),
            "exit",
            "36",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(36));
    let output = harness.command().arg("status").output().unwrap();
    let status = String::from_utf8(output.stdout).unwrap();
    assert!(status.contains("failed exec exit=36"), "{status}");
    assert!(status.contains("backend exited with code 36"), "{status}");
    let store = bazelqueue::store::Store::open(&harness.paths).unwrap();
    let mut job = store
        .jobs()
        .unwrap()
        .into_iter()
        .find(|job| job.code == Some(36))
        .unwrap();
    assert!(
        bazelqueue::summary::load(&harness.paths, &job.request.id)
            .unwrap()
            .unwrap()
            .reason
            .is_none()
    );
    job.reason = Some("disk full writing completion receipt".into());
    store.save(&job).unwrap();
    let waited = harness
        .command()
        .args(["wait", &job.request.id])
        .output()
        .unwrap();
    assert_eq!(waited.status.code(), Some(36));
    assert!(
        String::from_utf8(waited.stdout)
            .unwrap()
            .contains("disk full writing completion receipt")
    );
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
fn critical_memory_pressure_blocks_and_recovery_resumes_the_queue() {
    let mut harness = Harness::new(1);
    fs::write(harness.paths.root.join("pressure"), "4").unwrap();
    harness.wait(|s| s.pressure == "critical" && s.constrained.is_some());
    let listener = harness.spawn_hold("a");
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    fs::write(harness.paths.root.join("pressure"), "1").unwrap();
    let mut running = Harness::accepted(&listener);
    running.write_all(b"F").unwrap();
}

#[test]
fn warning_runs_parallel_jobs_at_full_capacity() {
    let mut harness = Harness::new(2);
    harness.command().arg("drain").output().unwrap();
    fs::write(harness.paths.root.join("pressure"), "2").unwrap();
    harness.wait(|s| s.pressure == "warning" && s.constrained.is_none());
    let first = harness.spawn_hold("first");
    let second = harness.spawn_hold("second");
    harness.wait(|s| s.jobs.iter().filter(|job| job.state == "queued").count() == 2);
    harness.command().arg("resume").output().unwrap();
    let mut running = Harness::accepted(&first);
    let mut parallel = Harness::accepted(&second);
    let snapshot =
        harness.wait(|s| s.jobs.iter().filter(|job| job.state == "running").count() == 2);
    let active: Vec<_> = snapshot
        .jobs
        .iter()
        .filter(|job| job.holds_capacity())
        .collect();
    assert_eq!(active.len(), 2);
    assert!(
        active
            .iter()
            .all(|job| job.request.budget.cpu == 4 && job.request.budget.memory_mib == 2048)
    );
    running.write_all(b"F").unwrap();
    parallel.write_all(b"F").unwrap();
}

#[test]
fn critical_reduces_new_work_and_warning_recovers_without_changing_a_grant() {
    let mut harness = Harness::new(2);
    fs::write(harness.paths.root.join("pressure"), "4").unwrap();
    harness.wait(|s| s.constrained.is_some());
    let first = harness.spawn_hold("first");
    let mut running = Harness::accepted(&first);
    let second = harness.spawn_hold("second");
    let before = harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    let granted = before
        .jobs
        .iter()
        .find(|job| job.state == "running")
        .unwrap()
        .request
        .budget
        .clone();
    assert_eq!((granted.cpu, granted.memory_mib), (2, 1024));
    assert!(second.accept().is_err());
    fs::write(harness.paths.root.join("pressure"), "2").unwrap();
    let mut parallel = Harness::accepted(&second);
    let after = harness.wait(|s| s.jobs.iter().filter(|job| job.state == "running").count() == 2);
    assert!(after.constrained.is_none());
    assert!(
        after
            .jobs
            .iter()
            .any(|job| job.state == "running" && job.request.budget == granted)
    );
    running.write_all(b"F").unwrap();
    parallel.write_all(b"F").unwrap();
}

#[test]
fn injected_swap_rates_reduce_and_recover_while_warning_persists() {
    let mut harness = Harness::new(2);
    let sensor = harness.paths.root.join("resources.json");
    let sample = |counter, at_ms| serde_json::json!({"at_ms":at_ms,"pressure":2,"total_mib":5120,"available_mib":5120,"swap_in_bytes":counter,"swap_out_bytes":counter});
    platform::atomic_write(&sensor, &serde_json::to_vec(&sample(0_u64, 0)).unwrap()).unwrap();
    harness.wait(|s| s.pressure == "warning" && s.constrained.is_none());
    platform::atomic_write(
        &sensor,
        &serde_json::to_vec(&sample(1_u64 << 50, 1000)).unwrap(),
    )
    .unwrap();
    harness
        .wait(|s| s.resources.as_ref().and_then(|r| r.reason.as_deref()) == Some("swap thrashing"));
    // Virtual time stays fixed until the test has observed the reduction.
    platform::atomic_write(
        &sensor,
        &serde_json::to_vec(&sample(1_u64 << 50, 2000)).unwrap(),
    )
    .unwrap();
    harness.wait(|s| s.pressure == "warning" && s.constrained.is_none());
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

fn upgrade_setup(harness: &Harness) -> std::process::Output {
    let bin = harness.root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    harness
        .command()
        .args([
            "setup",
            "--backend",
            env!("CARGO_BIN_EXE_fixture-backend"),
            "--bin-dir",
        ])
        .arg(&bin)
        .env("SHELL", "/bin/zsh")
        .env_remove("ZDOTDIR")
        .output()
        .unwrap()
}

fn assert_upgraded(mut harness: Harness, previous: Snapshot) {
    let output = upgrade_setup(&harness);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!("replaced coordinator {}", previous.daemon.pid)),
        "{stdout}"
    );
    assert!(harness.daemon.wait().unwrap().success());
    let current = harness.snapshot().expect("no coordinator after upgrade");
    assert_ne!(current.daemon, previous.daemon);
    assert_eq!(current.version, coordinator::VERSION);
    assert_eq!(
        current.executable.unwrap(),
        harness.paths.root.join("current").canonicalize().unwrap()
    );
    assert!(!current.drained);
    let status = harness.command().arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(
        text.contains(&format!("bazelqueue {} |", coordinator::VERSION)),
        "{text}"
    );
    assert!(!text.contains("warning"), "{text}");
    let build = harness.spawn_hold("after-upgrade");
    let mut admitted = Harness::accepted(&build);
    admitted.write_all(b"F").unwrap();
    assert!(harness.children[0].wait().unwrap().success());
}

#[test]
fn upgrade_replaces_a_coordinator_that_rejects_the_new_configuration() {
    let harness = Harness::with_coordinator(1, Some("legacy"));
    let previous = harness.snapshot().unwrap();
    assert_eq!(previous.version, "");
    let status = harness.command().arg("status").output().unwrap();
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("bazelqueue unreported (older) |"), "{text}");
    assert!(
        text.contains(&format!(
            "warning: coordinator {} runs bazelqueue unreported (older), but this bazelqueue is {}; run `bazelqueue setup`",
            previous.daemon.pid,
            coordinator::VERSION
        )),
        "{text}"
    );
    let doctor = harness.command().arg("doctor").output().unwrap();
    assert!(!doctor.status.success());
    assert!(
        String::from_utf8_lossy(&doctor.stderr).contains("run `bazelqueue setup`"),
        "{doctor:?}"
    );
    assert_upgraded(harness, previous);
}

#[test]
fn upgrade_retires_a_coordinator_of_another_version() {
    let harness = Harness::with_coordinator(1, Some("0.0.1"));
    let previous = harness.snapshot().unwrap();
    assert_eq!(previous.version, "0.0.1");
    assert_upgraded(harness, previous);
}

fn queued(snapshot: &Snapshot) -> usize {
    snapshot
        .jobs
        .iter()
        .filter(|job| job.state == "queued")
        .count()
}

fn assert_not_started(listener: &UnixListener) {
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn upgrade_hands_running_and_queued_jobs_of_released_guardians_to_the_installed_coordinator() {
    let mut harness = Harness::with_coordinator(1, Some("0.0.1"));
    let previous = harness.snapshot().unwrap();
    let first = harness.spawn_hold_as("running", Some("0.0.1"));
    let mut running = Harness::accepted(&first);
    let second = harness.spawn_hold_as("second", Some("0.0.1"));
    harness.wait(|s| queued(s) == 1);
    let third = harness.spawn_hold_as("third", Some("0.0.1"));
    let before = harness.wait(|s| queued(s) == 2);
    let output = upgrade_setup(&harness);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!("replaced coordinator {}", previous.daemon.pid)),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(harness.daemon.wait().unwrap().success());
    let installed = harness.paths.root.join("current").canonicalize().unwrap();
    let current = harness.wait(|s| {
        s.jobs
            .iter()
            .filter(|job| !job.terminal())
            .all(|job| job.child.is_some() || job.state == "queued")
    });
    assert_eq!(current.version, coordinator::VERSION);
    assert_eq!(current.executable.as_deref(), Some(installed.as_path()));
    assert!(!current.drained);
    let states = |snapshot: &Snapshot| {
        snapshot
            .jobs
            .iter()
            .map(|job| (job.request.id.clone(), job.state.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(states(&current), states(&before));
    let stable = |harness: &Harness| {
        for _ in 0..20 {
            let snapshot = harness.snapshot().expect("coordinator disappeared");
            assert_eq!(
                snapshot.daemon, current.daemon,
                "another coordinator took over"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    stable(&harness);
    assert_not_started(&first);
    assert_not_started(&second);
    running.write_all(b"F").unwrap();
    let mut next = Harness::accepted(&second);
    assert_not_started(&third);
    next.write_all(b"F").unwrap();
    let mut last = Harness::accepted(&third);
    last.write_all(b"F").unwrap();
    for child in &mut harness.children {
        assert!(child.wait().unwrap().success());
    }
    stable(&harness);
    let finished = harness.snapshot().unwrap();
    assert_eq!(
        finished
            .jobs
            .iter()
            .filter(|job| job.state == "finished" && job.code == Some(0))
            .count(),
        3
    );
}

#[test]
fn a_guardian_relaunches_the_installed_coordinator_instead_of_its_own_image() {
    let mut harness = Harness::new(1);
    let installed = harness.paths.root.join("installed");
    fs::write(
        &installed,
        format!(
            "#!/bin/sh\nBAZELQUEUE_FIXTURE_COORDINATOR=9.9.9-installed exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_bazelqueue")
        ),
    )
    .unwrap();
    fs::set_permissions(
        &installed,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::os::unix::fs::symlink(&installed, harness.paths.root.join("current")).unwrap();
    let first = harness.spawn_hold("a");
    let mut running = Harness::accepted(&first);
    let second = harness.spawn_hold("b");
    harness.wait(|s| queued(s) == 1);
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    let relaunched = harness.wait(|s| s.version == "9.9.9-installed");
    assert_eq!(
        relaunched
            .jobs
            .iter()
            .map(|job| job.state.as_str())
            .collect::<Vec<_>>(),
        ["running", "queued"]
    );
    running.write_all(b"F").unwrap();
    let mut next = Harness::accepted(&second);
    next.write_all(b"F").unwrap();
    for child in &mut harness.children {
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn setup_keeps_a_current_coordinator() {
    let mut harness = Harness::new(1);
    let previous = harness.snapshot().unwrap();
    assert_eq!(previous.version, coordinator::VERSION);
    let output = upgrade_setup(&harness);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("replaced coordinator"));
    assert!(harness.daemon.try_wait().unwrap().is_none());
    assert_eq!(harness.snapshot().unwrap().daemon, previous.daemon);
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
fn native_failed_tests_and_targets_are_summarized_with_their_logs() {
    let harness = Harness::new(1);
    native_workspace(
        &harness,
        "load(\":script_test.bzl\", \"script_test\")\nscript_test(name=\"passes\", code=0)\nscript_test(name=\"fails\", code=1)\ngenrule(name=\"broken\", outs=[\"broken.txt\"], cmd=\"exit 1\")\n",
    );
    fs::write(
        harness.root.path().join("workspace/script_test.bzl"),
        "def _impl(ctx):\n    out = ctx.actions.declare_file(ctx.label.name + \".sh\")\n    ctx.actions.write(out, \"#!/bin/sh\\necho ran\\nexit %d\\n\" % ctx.attr.code, is_executable = True)\n    return [DefaultInfo(executable = out)]\n\nscript_test = rule(_impl, test = True, attrs = {\"code\": attr.int()})\n",
    )
    .unwrap();
    let mut args = native_args(&harness, "test");
    args.extend([
        "//:passes".into(),
        "//:fails".into(),
        "//:broken".into(),
        "--keep_going".into(),
    ]);
    let output = native_command(&harness, args).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let block: Vec<_> = stderr
        .lines()
        .skip_while(|line| !line.starts_with("bazelqueue: finished "))
        .collect();
    assert!(
        block[0].contains(" exit=1 BUILD_FAILURE ") && block[0].ends_with(" failed=2"),
        "{stderr}"
    );
    let failing = block
        .iter()
        .find_map(|line| line.strip_prefix("bazelqueue: FAILED //:fails FAILED log="))
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(fs::read_to_string(failing).unwrap().contains("ran"));
    assert!(
        block.contains(&"bazelqueue: FAILED //:broken (build)"),
        "{stderr}"
    );
    assert!(block.last().unwrap().starts_with("bazelqueue: summary "));
    let runs = harness.paths.root.join("runs");
    let left: Vec<_> = fs::read_dir(&runs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        left.iter().all(|name| !name.ends_with(".bep.json")),
        "{left:?}"
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
    let _resume = ResumeOnDrop(server.identity.clone());
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

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real Bazel in an isolated workspace"]
fn native_unresponsive_orphan_finishes_and_admits_its_successor() {
    let mut harness = Harness::new(1);
    native_workspace(&harness, "");
    let mut args = native_args(&harness, "build");
    args.push("//:hello".into());
    assert!(
        native_command(&harness, args)
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut job = harness
        .wait(|s| {
            s.jobs
                .iter()
                .any(|job| job.terminal() && job.server.is_some())
        })
        .jobs
        .into_iter()
        .find(|job| job.server.is_some())
        .unwrap();
    let server = job.server.as_ref().unwrap().clone();
    platform::signal_pid(server.identity.pid, libc::SIGSTOP);
    // Simulate guardian death after client completion, before its durable result.
    fs::remove_file(bazelqueue::summary::location(&harness.paths, &job.request.id).unwrap())
        .unwrap();
    job.state = "quarantined".into();
    job.code = None;
    job.reason = None;
    bazelqueue::store::Store::open(&harness.paths)
        .unwrap()
        .save(&job)
        .unwrap();
    let successor = harness.spawn_hold("successor");
    let mut admitted = Harness::accepted(&successor);
    let recovered = harness.snapshot().unwrap();
    assert_eq!(recovered.jobs[0].state, "finished");
    assert_eq!(recovered.jobs[0].code, Some(125));
    assert!(
        recovered.jobs[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("unresponsive orphan server")
    );
    assert!(!platform::alive(&server.identity));
    let output = harness.command().arg("status").output().unwrap();
    let status = String::from_utf8(output.stdout).unwrap();
    assert!(status.contains("failed build exit=125"), "{status}");
    assert!(status.contains("unresponsive orphan server"));
    let output = harness
        .command()
        .args(["wait", &job.request.id])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("unresponsive orphan server")
    );
    admitted.write_all(b"F").unwrap();
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real Bazel in an isolated workspace"]
fn native_idle_quarantine_releases_while_guardian_is_paused_and_ignores_late_finish() {
    let mut harness = Harness::new(1);
    let socket = harness.root.path().join("action.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    native_workspace(
        &harness,
        &format!(
            "genrule(name=\"held\",outs=[\"held.txt\"],tags=[\"local\"],cmd=\"{} hold {}; echo done > $@\")",
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    );
    let mut args = native_args(&harness, "build");
    args.push("//:held".into());
    let child = native_command(&harness, args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            fs::File::create(harness.root.path().join("native.log")).unwrap(),
        ))
        .spawn()
        .unwrap();
    harness.children.push(child);
    let mut action = Harness::accepted(&listener);
    let mut job = harness
        .wait(|s| s.jobs.iter().any(|job| job.state == "running"))
        .jobs[0]
        .clone();
    platform::signal_pid(job.request.owner.pid, libc::SIGSTOP);
    let _resume = ResumeOnDrop(job.request.owner.clone());
    job.state = "quarantined".into();
    bazelqueue::store::Store::open(&harness.paths)
        .unwrap()
        .save(&job)
        .unwrap();
    let next = harness.spawn_hold("next");
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    assert!(next.accept().is_err());
    assert!(platform::alive(job.child.as_ref().unwrap()));
    action.write_all(b"F").unwrap();
    let mut admitted = Harness::accepted(&next);
    assert!(platform::alive(&job.request.owner));
    let recovered = harness.snapshot().unwrap();
    assert_eq!(recovered.jobs[0].state, "finished");
    assert_eq!(recovered.jobs[0].code, Some(125));
    platform::signal_pid(job.request.owner.pid, libc::SIGCONT);
    assert!(harness.children[0].wait().unwrap().success());
    let late = harness.snapshot().unwrap();
    assert_eq!(late.jobs[0].state, "finished");
    assert_eq!(late.jobs[0].code, Some(125));
    let output = harness
        .command()
        .args(["wait", &job.request.id])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    let waited = String::from_utf8(output.stdout).unwrap();
    assert!(waited.contains("native server verified idle"));
    assert!(!waited.contains("SUCCESS"), "{waited}");
    admitted.write_all(b"F").unwrap();
}

#[test]
fn retirement_leaf_proof_refuses_a_live_child_without_signalling_it() {
    let root = tempfile::tempdir().unwrap();
    let listener = UnixListener::bind(root.path().join("child.sock")).unwrap();
    let mut parent = Command::new(env!("CARGO_BIN_EXE_fixture-backend"))
        .arg("tree-hold")
        .arg(listener.local_addr().unwrap().as_pathname().unwrap())
        .spawn()
        .unwrap();
    let mut child = Harness::accepted(&listener);
    let identity = platform::identity(parent.id()).unwrap();
    let _resume = ResumeOnDrop(identity.clone());
    platform::signal_verified(&identity, libc::SIGSTOP).unwrap();
    loop {
        if let Some(leaf) = platform::stopped_leaf(&identity).unwrap() {
            assert!(!leaf);
            break;
        }
        std::thread::yield_now();
    }
    child.write_all(b"F").unwrap();
    platform::signal_verified(&identity, libc::SIGCONT).unwrap();
    assert!(parent.wait().unwrap().success());
    let stale = platform::Identity {
        pid: std::process::id(),
        birth: "stale".into(),
    };
    assert!(!platform::signal_verified(&stale, libc::SIGKILL).unwrap());
}

#[test]
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; real Bazel in an isolated workspace"]
fn native_restart_reprobes_a_responsive_server_instead_of_replaying_retirement() {
    let mut harness = Harness::new(1);
    native_workspace(&harness, "");
    let mut args = native_args(&harness, "build");
    args.push("//:hello".into());
    assert!(
        native_command(&harness, args)
            .output()
            .unwrap()
            .status
            .success()
    );
    let mut job = harness
        .wait(|s| {
            s.jobs
                .iter()
                .any(|job| job.terminal() && job.server.is_some())
        })
        .jobs[0]
        .clone();
    let server = job.server.as_ref().unwrap().clone();
    harness.daemon.kill().unwrap();
    harness.daemon.wait().unwrap();
    fs::remove_file(bazelqueue::summary::location(&harness.paths, &job.request.id).unwrap())
        .unwrap();
    job.state = "quarantined".into();
    job.code = None;
    job.retiring_server = Some(server.identity.clone());
    bazelqueue::store::Store::open(&harness.paths)
        .unwrap()
        .save(&job)
        .unwrap();
    let next = harness.spawn_hold("next");
    let mut admitted = Harness::accepted(&next);
    let recovered = harness.snapshot().unwrap();
    assert_eq!(recovered.jobs[0].state, "finished");
    assert!(recovered.jobs[0].retiring_server.is_none());
    assert!(
        recovered.jobs[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("native server verified idle")
    );
    assert!(platform::alive(&server.identity));
    admitted.write_all(b"F").unwrap();
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
    fs::write(harness.paths.root.join("pressure"), "2").unwrap();
    harness.wait(|s| s.pressure == "warning" && s.constrained.is_none());
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
        7
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
#[ignore = "requires BAZELQUEUE_TEST_BAZEL; a hooked native build beside a running one"]
fn native_build_with_hooks_runs_beside_another_build() {
    let mut harness = Harness::new(2);
    native_workspace(&harness, "");
    let mut config = Config::load(&harness.paths).unwrap();
    config.hooks.push(bazelqueue::config::Hook {
        program: env!("CARGO_BIN_EXE_fixture-backend").into(),
        arguments: vec!["exit".into(), "0".into()],
    });
    config.save(&harness.paths).unwrap();
    let busy = harness.spawn_hold("busy");
    let mut held = Harness::accepted(&busy);
    let workspace = harness.root.path().join("workspace");
    let socket = harness.root.path().join("native.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::write(
        workspace.join("BUILD.bazel"),
        format!(
            "genrule(name=\"held\",outs=[\"held.txt\"],tags=[\"local\"],cmd=\"{} hold {}; echo done > $@\")\n",
            env!("CARGO_BIN_EXE_fixture-backend"),
            socket.display()
        ),
    )
    .unwrap();
    let mut args = native_args(&harness, "build");
    args.push("//:held".into());
    let child = native_command(&harness, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    harness.children.push(child);
    let mut build = Harness::accepted(&listener);
    let snapshot =
        harness.wait(|s| s.jobs.iter().filter(|job| job.state == "running").count() == 2);
    assert!(
        snapshot
            .jobs
            .iter()
            .filter(|job| job.state == "running")
            .all(|job| !job.request.budget.exclusive)
    );
    build.write_all(b"F").unwrap();
    held.write_all(b"F").unwrap();
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

fn shim(harness: &Harness) -> Command {
    let shim = harness.root.path().join("bazelisk");
    if !shim.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_bazelqueue"), &shim).unwrap();
    }
    let mut command = Command::new(shim);
    command
        .env("BAZELQUEUE_HOME", &harness.paths.root)
        .env("HOME", harness.root.path())
        .current_dir(harness.root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}
fn exits(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "command kept waiting in the queue"
        );
        std::thread::yield_now();
    }
}
fn stderr_lines(child: &mut Child) -> std::sync::mpsc::Receiver<String> {
    use std::io::BufRead;
    let (sender, lines) = std::sync::mpsc::channel();
    let stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    std::thread::spawn(move || {
        for line in stderr.lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    lines
}
fn wait_for_line(lines: &std::sync::mpsc::Receiver<String>, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match lines.recv_timeout(left) {
            Ok(line) if line.contains(text) => return,
            Ok(_) => {}
            Err(_) => panic!("never printed {text:?}"),
        }
    }
}

#[test]
fn read_only_commands_return_while_an_exclusive_job_runs() {
    let mut harness = Harness::new(1);
    let busy = harness.spawn_hold("busy");
    let mut held = Harness::accepted(&busy);
    for args in [
        vec!["info", "execution_root"],
        vec!["cquery", "//:x"],
        vec!["--version"],
    ] {
        let mut child = shim(&harness).args(&args).spawn().unwrap();
        assert_eq!(exits(&mut child).code(), Some(2), "{args:?}");
    }
    held.write_all(b"F").unwrap();
}

#[test]
fn clean_expunge_waits_only_for_its_own_workspace() {
    let mut harness = Harness::new(1);
    let busy = harness.spawn_hold("busy");
    let mut held = Harness::accepted(&busy);
    let other = harness.root.path().join("other");
    fs::create_dir(&other).unwrap();
    let mut elsewhere = shim(&harness)
        .args(["clean", "--expunge"])
        .current_dir(&other)
        .spawn()
        .unwrap();
    assert_eq!(exits(&mut elsewhere).code(), Some(2));
    let mut same = shim(&harness)
        .args(["clean", "--expunge"])
        .current_dir(harness.root.path().join("busy"))
        .spawn()
        .unwrap();
    let lines = stderr_lines(&mut same);
    wait_for_line(&lines, "waiting: workspace is busy");
    assert!(same.try_wait().unwrap().is_none());
    held.write_all(b"F").unwrap();
    assert_eq!(exits(&mut same).code(), Some(2));
}

#[test]
fn preflight_hooks_run_one_at_a_time_inside_their_own_reservations() {
    let mut harness = Harness::new(2);
    let socket = harness.root.path().join("hook.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut config = Config::load(&harness.paths).unwrap();
    config.hooks.push(bazelqueue::config::Hook {
        program: env!("CARGO_BIN_EXE_fixture-backend").into(),
        arguments: vec!["hold".into(), socket.to_string_lossy().into_owned()],
    });
    config.save(&harness.paths).unwrap();
    let (sender, lines) = std::sync::mpsc::channel();
    for label in ["one", "two"] {
        let lane = harness.root.path().join(label);
        fs::create_dir(&lane).unwrap();
        let mut child = harness
            .command()
            .args([
                "exec",
                "--",
                env!("CARGO_BIN_EXE_fixture-backend"),
                "exit",
                "0",
            ])
            .current_dir(lane)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = stderr_lines(&mut child);
        let sender = sender.clone();
        std::thread::spawn(move || {
            for line in output {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        harness.children.push(child);
    }
    let mut first = Harness::accepted(&listener);
    wait_for_line(&lines, "waiting for another invocation's preflight hooks");
    let snapshot = harness.snapshot().unwrap();
    assert_eq!(
        snapshot
            .jobs
            .iter()
            .filter(|job| job.holds_capacity() && !job.request.budget.exclusive)
            .count(),
        2
    );
    first.write_all(b"F").unwrap();
    let mut second = Harness::accepted(&listener);
    second.write_all(b"F").unwrap();
    for child in &mut harness.children {
        assert!(exits(child).success());
    }
}

#[test]
fn an_agent_learns_a_queued_result_from_one_backgrounded_call() {
    let mut harness = Harness::new(1);
    let first = harness.spawn_hold("a");
    let mut a = Harness::accepted(&first);
    let ahead = harness.spawn_hold("c");
    harness.wait(|s| s.jobs.iter().any(|job| job.state == "queued"));
    let lane = harness.root.path().join("b");
    fs::create_dir_all(&lane).unwrap();
    let socket = harness.root.path().join("b.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let copy = harness.root.path().join("result.json");
    let agent = harness
        .command()
        .args(["exec", "--", env!("CARGO_BIN_EXE_fixture-backend"), "hold"])
        .arg(&socket)
        .arg("3")
        .env("BAZELQUEUE_SUMMARY", &copy)
        .env_remove("BAZELQUEUE_PROGRESS")
        .current_dir(&lane)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    harness.children.push(agent);
    let lane_name = lane.canonicalize().unwrap().to_string_lossy().into_owned();
    let snapshot = harness.wait(|s| {
        s.jobs
            .iter()
            .any(|job| job.request.workspace == lane_name && job.state == "queued")
    });
    let id = snapshot
        .jobs
        .iter()
        .find(|job| job.request.workspace == lane_name)
        .unwrap()
        .request
        .id
        .clone();
    let waiter = |command: &mut Command| {
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let by_id = waiter(harness.command().args(["wait", &id]));
    let by_workspace = waiter(harness.command().arg("wait").current_dir(&lane));
    harness.children.push(by_id);
    harness.children.push(by_workspace);
    a.write_all(b"F").unwrap();
    let mut c = Harness::accepted(&ahead);
    harness.wait(|s| {
        s.jobs
            .iter()
            .any(|job| job.request.id == id && job.state == "queued")
    });
    c.write_all(b"F").unwrap();
    let mut b = Harness::accepted(&listener);
    for waiter in &mut harness.children[3..] {
        assert!(
            waiter.try_wait().unwrap().is_none(),
            "wait returned before the run ended"
        );
    }
    b.write_all(b"F").unwrap();
    let mut outputs = Vec::new();
    for child in &mut harness.children[2..] {
        let status = child.wait().unwrap();
        let mut output = String::new();
        child
            .stdout
            .take()
            .map(|mut out| out.read_to_string(&mut output).unwrap());
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        outputs.push((status.code(), output));
    }
    let (agent_code, agent_output) = &outputs[0];
    assert_eq!(*agent_code, Some(3));
    let lines: Vec<_> = agent_output.lines().collect();
    assert_eq!(lines.len(), 5, "{agent_output}");
    assert!(
        lines[0].starts_with(&format!("bazelqueue: queued {id} at position 2 (waiting: ")),
        "{agent_output}"
    );
    assert!(
        lines[1].starts_with("bazelqueue: this call exits when the build ends"),
        "{agent_output}"
    );
    assert!(lines[2].starts_with(&format!("bazelqueue: started {id} after ")));
    assert!(
        lines[3].starts_with(&format!("bazelqueue: finished {id} exit=3 queued="))
            && lines[3].ends_with(" failed=unreported"),
        "{agent_output}"
    );
    let summary_path = harness.paths.root.join("runs").join(format!("{id}.json"));
    assert_eq!(
        lines[4],
        format!("bazelqueue: summary {}", summary_path.display())
    );
    for (code, output) in &outputs[1..] {
        assert_eq!(*code, Some(3), "{output}");
        assert_eq!(output.lines().collect::<Vec<_>>(), lines[3..], "{output}");
    }
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&copy).unwrap()).unwrap();
    assert_eq!(saved["id"], id.as_str());
    assert_eq!(saved["exit_code"], 3);
    assert_eq!(fs::read(&copy).unwrap(), fs::read(&summary_path).unwrap());
}
