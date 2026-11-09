use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    volume: PathBuf,
    mounts: PathBuf,
    shims: PathBuf,
    events: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("checkout");
        let volume = base.join("Externo");
        let shims = base.join("shims");
        for path in [&root, &volume, &shims] {
            fs::create_dir(path).unwrap();
        }
        let init = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&root)
            .output()
            .unwrap();
        assert!(init.status.success());
        fs::write(
            root.join(".bazelrc"),
            "try-import %workspace%/.bazelrc.local\n",
        )
        .unwrap();
        fs::write(root.join(".bazelignore"), ".bazel-output\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            "/.bazel-output/\n/.bazelrc.local\n/.bazelrc.local.*\n/.bazel-output-disk.log\n",
        )
        .unwrap();
        let mounts = base.join("mounts");
        let events = base.join("disk-fallbacks.jsonl");
        let fixture = Self {
            _dir: dir,
            root,
            volume,
            mounts,
            shims,
            events,
        };
        fixture.mount(true);
        fixture
    }

    fn mount(&self, mounted: bool) {
        let table = if mounted {
            format!(
                "/dev/disk9s1 on {} (apfs, local, nodev, nosuid)\n",
                self.volume.display()
            )
        } else {
            "/dev/disk3s1 on / (apfs, local)\n".to_owned()
        };
        fs::write(&self.mounts, table).unwrap();
    }

    fn run_with(&self, timeout: &str, path: Option<&Path>) -> Output {
        self.run_tuned(timeout, path, &[])
    }

    fn run_tuned(&self, timeout: &str, path: Option<&Path>, extra: &[(&str, &str)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/bazel-output-disk");
        let mut command = Command::new(script);
        command
            .arg(&self.volume)
            .current_dir(&self.root)
            .env("BAZEL_OUTPUT_DISK_MOUNT_TABLE", &self.mounts)
            .env("BAZEL_OUTPUT_DISK_TIMEOUT", timeout)
            .env("BAZEL_OUTPUT_DISK_RETRY_WAIT", "0")
            .env("BAZEL_OUTPUT_DISK_EVENTS", &self.events)
            .envs(extra.iter().copied())
            .env_remove("BAZEL_SHARED_CACHE_DIR")
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        if let Some(shims) = path {
            let inherited = std::env::var("PATH").unwrap();
            command.env("PATH", format!("{}:{inherited}", shims.display()));
        }
        command.output().unwrap()
    }

    fn run(&self) -> Output {
        let output = self.run_with("3", None);
        assert!(output.status.success(), "{output:?}");
        output
    }

    fn output_base(&self) -> String {
        fs::read_to_string(self.root.join(".bazelrc.local"))
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("startup --output_base="))
            .unwrap()
            .to_owned()
    }

    fn internal_base(&self) -> String {
        format!("{}/.bazel-output", self.root.display())
    }

    fn external_prefix(&self) -> String {
        format!("{}/bazel/", self.volume.display())
    }

    fn recorded(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(&self.events)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join(".bazel-output-disk.log")).unwrap_or_default()
    }
}

/// A `mkdir` that hangs for the first `slow` calls it sees and works afterwards, like a volume that
/// wakes up. `calls` counts every attempt the hook made.
fn waking_volume(fixture: &Fixture, slow: usize) -> PathBuf {
    let calls = fixture.shims.join("calls");
    fs::write(&calls, "").unwrap();
    let shim = fixture.shims.join("mkdir");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in \"-p \"*/Externo/bazel) printf x >> '{calls}'; \
             [ \"$(wc -c < '{calls}')\" -le {slow} ] && exec sleep 30;; esac\n\
             exec /bin/mkdir \"$@\"\n",
            calls = calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    calls
}

fn attempts(calls: &Path) -> usize {
    fs::read_to_string(calls).unwrap().len()
}

struct Server(Child);

impl Server {
    fn start(output_base: &str) -> Self {
        Self(
            Command::new("sh")
                .args([
                    "-c",
                    "trap 'exit 0' TERM; i=0; while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done",
                    "bazel-server",
                ])
                .arg(format!("--output_base={output_base}"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn stopped(&mut self) -> bool {
        self.0.try_wait().unwrap().is_some()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Unlocked<'a>(&'a Path);

impl Drop for Unlocked<'_> {
    fn drop(&mut self) {
        fs::set_permissions(self.0, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn a_mounted_writable_volume_hosts_the_output_base() {
    let fixture = Fixture::new();
    fixture.run();
    let base = fixture.output_base();
    let hash = base.strip_prefix(&fixture.external_prefix()).unwrap();
    assert_eq!(hash.len(), 16);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(fixture.volume.join("bazel").is_dir());
    assert!(
        fs::read_dir(fixture.volume.join("bazel"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".write-probe")),
        "the write probe must clean up after itself"
    );
    assert!(fixture.log().is_empty());
    assert!(fixture.recorded().is_empty());
}

#[test]
fn the_same_checkout_always_gets_the_same_base_and_each_checkout_its_own() {
    let first = Fixture::new();
    first.run();
    let base = first.output_base();
    first.run();
    assert_eq!(first.output_base(), base);

    let second = Fixture::new();
    second.run();
    assert_ne!(second.output_base(), base);
}

#[test]
fn an_unmounted_volume_leaves_the_output_base_inside_the_checkout() {
    let fixture = Fixture::new();
    fixture.mount(false);
    fixture.run();
    assert_eq!(fixture.output_base(), fixture.internal_base());
    assert!(!fixture.volume.join("bazel").exists());
}

#[test]
fn a_refused_write_falls_back_and_says_how_to_grant_access() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.volume, fs::Permissions::from_mode(0o555)).unwrap();
    let _restore = Unlocked(&fixture.volume);
    let output = fixture.run();
    assert_eq!(fixture.output_base(), fixture.internal_base());
    let log = fixture.log();
    assert!(log.contains("writing to"), "{log}");
    assert!(log.contains("Removable Volumes"), "{log}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Full Disk Access"),
        "{output:?}"
    );
}

#[test]
fn a_volume_that_stops_answering_falls_back_without_hanging() {
    let fixture = Fixture::new();
    let hang = fixture.shims.join("mkdir");
    fs::write(&hang, "#!/bin/sh\nexec sleep 30\n").unwrap();
    fs::set_permissions(&hang, fs::Permissions::from_mode(0o755)).unwrap();
    let output = fixture.run_with("1", Some(&fixture.shims));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.output_base(), fixture.internal_base());
    assert!(fixture.log().contains("does not answer within 1s"));
}

#[test]
fn a_volume_that_answers_on_a_later_attempt_is_still_used() {
    let fixture = Fixture::new();
    let calls = waking_volume(&fixture, 1);
    let output = fixture.run_with("1", Some(&fixture.shims));
    assert!(output.status.success(), "{output:?}");
    assert!(
        fixture
            .output_base()
            .starts_with(&fixture.external_prefix())
    );
    assert_eq!(attempts(&calls), 2);
    assert!(fixture.recorded().is_empty());
    assert!(fixture.log().is_empty());
}

#[test]
fn a_volume_that_stays_slow_falls_back_after_every_attempt_and_records_why() {
    let fixture = Fixture::new();
    let calls = waking_volume(&fixture, 99);
    let output = fixture.run_tuned(
        "1",
        Some(&fixture.shims),
        &[("BAZEL_OUTPUT_DISK_RETRIES", "3")],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.output_base(), fixture.internal_base());
    assert_eq!(attempts(&calls), 3);
    let recorded = fixture.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["kind"], "slow");
    assert_eq!(recorded[0]["volume"], fixture.volume.to_str().unwrap());
    assert_eq!(recorded[0]["checkout"], fixture.root.to_str().unwrap());
    assert!(
        recorded[0]["reason"]
            .as_str()
            .unwrap()
            .contains("3 attempt(s)")
    );
    let at = recorded[0]["at"].as_str().unwrap();
    assert!(at.len() == 20 && at.ends_with('Z'), "{at}");
}

#[test]
fn a_slow_volume_never_pushes_a_nearly_full_internal_disk_over_the_edge() {
    let fixture = Fixture::new();
    fixture.run();
    let external = fixture.output_base();
    waking_volume(&fixture, 99);
    let output = fixture.run_tuned(
        "1",
        Some(&fixture.shims),
        &[("BAZEL_OUTPUT_DISK_INTERNAL_MIN_FREE_GB", "999999999")],
    );
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(fixture.output_base(), external);
    assert!(!Path::new(&fixture.internal_base()).exists());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("below the 999999999 GB margin"), "{stderr}");
    assert!(stderr.contains("instead of filling it"), "{stderr}");
    let recorded = fixture.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["kind"], "blocked_low_disk");
}

#[test]
fn a_refused_write_is_not_retried_and_is_recorded() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.volume, fs::Permissions::from_mode(0o555)).unwrap();
    let _restore = Unlocked(&fixture.volume);
    let calls = waking_volume(&fixture, 0);
    fixture.run_with("1", Some(&fixture.shims));
    assert_eq!(attempts(&calls), 1);
    assert_eq!(fixture.recorded()[0]["kind"], "refused");
}

#[test]
fn an_unmounted_volume_is_recorded_without_blocking_the_internal_disk() {
    let fixture = Fixture::new();
    fixture.mount(false);
    let output = fixture.run_tuned(
        "1",
        None,
        &[("BAZEL_OUTPUT_DISK_INTERNAL_MIN_FREE_GB", "999999999")],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.output_base(), fixture.internal_base());
    assert_eq!(fixture.recorded()[0]["kind"], "not_mounted");
}

#[test]
fn the_probe_waits_twenty_seconds_by_default() {
    let script =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/bazel-output-disk"))
            .unwrap();
    assert!(script.contains("BAZEL_OUTPUT_DISK_TIMEOUT:-20}"));
}

#[test]
fn going_back_to_the_internal_disk_stops_the_external_server_only() {
    let fixture = Fixture::new();
    fixture.run();
    let external = fixture.output_base();
    let mut previous = Server::start(&external);
    let mut other_checkout = Server::start("/tmp/bazelqueue-other-checkout-base");
    let mut prefix_sibling = Server::start(&format!("{external}-copy"));

    fixture.mount(false);
    fixture.run();

    assert_eq!(fixture.output_base(), fixture.internal_base());
    assert!(previous.stopped());
    assert!(!other_checkout.stopped());
    assert!(!prefix_sibling.stopped());
}

#[test]
fn moving_to_the_external_disk_stops_the_internal_server() {
    let fixture = Fixture::new();
    fixture.mount(false);
    fixture.run();
    let mut previous = Server::start(&fixture.internal_base());
    let mut other_checkout = Server::start("/tmp/bazelqueue-other-checkout-base");

    fixture.mount(true);
    fixture.run();

    assert!(
        fixture
            .output_base()
            .starts_with(&fixture.external_prefix())
    );
    assert!(previous.stopped());
    assert!(!other_checkout.stopped());
}

#[test]
fn staying_on_the_same_disk_leaves_its_server_running() {
    let fixture = Fixture::new();
    fixture.run();
    let mut server = Server::start(&fixture.output_base());
    fixture.run();
    assert!(!server.stopped());
}

#[test]
fn missing_preparation_fails_before_probe_log_or_server_switch() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join(".bazelrc.local"),
        "startup --output_base=/previous/base\n",
    )
    .unwrap();
    fs::remove_file(fixture.root.join(".bazelignore")).unwrap();
    let output = fixture.run_with("3", None);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("add .bazel-output to .bazelignore"));
    assert!(!fixture.volume.join("bazel").exists());
    assert!(!fixture.root.join(".bazel-output-disk.log").exists());
    assert_eq!(fixture.output_base(), "/previous/base");
    assert!(!fixture.root.join(".bazelignore").exists());
}

#[test]
fn repeated_hook_with_shared_cache_keeps_tracked_files_clean() {
    let fixture = Fixture::new();
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&fixture.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap()
    };
    assert!(git(&["add", "."]).status.success());
    assert!(
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture"
        ])
        .status
        .success()
    );
    fs::write(
        fixture.root.join(".bazelrc.local"),
        "# Personal settings\nbuild --jobs=3\n",
    )
    .unwrap();
    let shared = fixture.volume.join("shared");
    for mounted in [true, true, false, false] {
        fixture.mount(mounted);
        let output =
            Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/bazel-output-disk"))
                .arg(&fixture.volume)
                .current_dir(&fixture.root)
                .env("BAZEL_OUTPUT_DISK_MOUNT_TABLE", &fixture.mounts)
                .env("BAZEL_OUTPUT_DISK_EVENTS", &fixture.events)
                .env("BAZEL_SHARED_CACHE_DIR", &shared)
                .output()
                .unwrap();
        assert!(output.status.success(), "{output:?}");
        let local = fs::read_to_string(fixture.root.join(".bazelrc.local")).unwrap();
        assert!(local.starts_with("# Personal settings\nbuild --jobs=3\n"));
        assert!(local.contains(&format!(
            "common --disk_cache={}/disk-cache",
            shared.display()
        )));
        let diff = git(&["diff", "--exit-code", "HEAD"]);
        assert!(diff.status.success(), "{diff:?}");
        assert!(
            git(&["ls-files", "--others", "--exclude-standard"])
                .stdout
                .is_empty()
        );
        assert_eq!(
            fixture
                .output_base()
                .starts_with(&fixture.external_prefix()),
            mounted
        );
    }
}
