#![cfg(feature = "test-fixtures")]
use bazelqueue::{
    config::{Config, Paths},
    platform,
    protocol::{Event, Message, PROTOCOL, Snapshot},
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tempfile::TempDir;

#[test]
fn path_activation_is_owned_and_preserves_independent_profile_edits() {
    let fixture = Fixture::new();
    let profile = fixture.home.path().join(".zshenv");
    fs::write(&profile, "export EXISTING=kept\n").unwrap();
    let output = fixture
        .command()
        .args(["setup", "--replace", "--bin-dir"])
        .arg(&fixture.bin)
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    success(output);
    let text = fs::read_to_string(&profile).unwrap();
    assert!(text.contains("# bazelqueue PATH begin"));
    assert!(text.contains("export EXISTING=kept"));
    fs::write(&profile, format!("{text}export ADDED=later\n")).unwrap();
    success(fixture.command().arg("uninstall").output().unwrap());
    assert_eq!(
        fs::read_to_string(profile).unwrap(),
        "export EXISTING=kept\nexport ADDED=later\n"
    );
}

#[test]
fn path_activation_of_a_new_profile_is_removed_on_uninstall() {
    let fixture = Fixture::new();
    let profile = fixture.home.path().join(".bash_profile");
    assert!(!profile.exists());
    success(
        fixture
            .command()
            .args(["setup", "--replace", "--bin-dir"])
            .arg(&fixture.bin)
            .env("SHELL", "/bin/bash")
            .output()
            .unwrap(),
    );
    assert!(
        fs::read_to_string(&profile)
            .unwrap()
            .contains("# bazelqueue PATH begin")
    );
    success(fixture.command().arg("uninstall").output().unwrap());
    assert!(!profile.exists());
}

fn receive<T: DeserializeOwned>(stream: &mut UnixStream) -> T {
    let mut size = [0; 4];
    stream.read_exact(&mut size).unwrap();
    let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
    stream.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn send(stream: &mut UnixStream, value: &impl Serialize) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
}
struct Fixture {
    home: TempDir,
    paths: Paths,
    bin: PathBuf,
    stopped: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let home = tempfile::Builder::new()
            .prefix("bqi-")
            .tempdir_in("/private/tmp")
            .unwrap();
        let root = home.path().join("state");
        fs::create_dir(&root).unwrap();
        let paths = Paths {
            config: root.join("config.toml"),
            database: root.join("queue.sqlite3"),
            socket: root.join("control.sock"),
            root,
        };
        Config {
            backend: PathBuf::from(env!("CARGO_BIN_EXE_fixture-backend")),
            ..Config::default()
        }
        .save(&paths)
        .unwrap();
        let bin = home.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("bazel"), "original bazel").unwrap();
        fs::write(bin.join("bazelisk"), "original bazelisk").unwrap();
        let listener = UnixListener::bind(&paths.socket).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let server = thread::spawn(move || {
            let mut drained = false;
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                assert!(matches!(
                    receive::<Message>(&mut stream),
                    Message::Hello { protocol: PROTOCOL }
                ));
                send(&mut stream, &Event::Hello { protocol: PROTOCOL });
                let Message::Control { operation, .. } = receive(&mut stream) else {
                    panic!("unexpected installation control");
                };
                let event = match operation.as_str() {
                    "drain" => {
                        drained = true;
                        Event::Ack
                    }
                    "resume" => {
                        drained = false;
                        Event::Ack
                    }
                    "status" => Event::Snapshot {
                        snapshot: Snapshot {
                            daemon: platform::current_identity().unwrap(),
                            drained,
                            pressure: "healthy".into(),
                            cpu_capacity: 1,
                            memory_capacity_mib: 1024,
                            legacy: vec![],
                            jobs: vec![],
                        },
                    },
                    _ => panic!("unexpected control {operation}"),
                };
                send(&mut stream, &event);
            }
        });
        Self {
            home,
            paths,
            bin,
            stopped,
            server: Some(server),
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bazelqueue"));
        command
            .env("BAZELQUEUE_HOME", &self.paths.root)
            .env("HOME", self.home.path())
            .current_dir(self.home.path());
        command
    }
    fn setup(&self) -> Output {
        self.command()
            .args([
                "setup",
                "--replace",
                "--backend",
                env!("CARGO_BIN_EXE_fixture-backend"),
                "--bin-dir",
            ])
            .arg(&self.bin)
            .output()
            .unwrap()
    }
    fn hook(&self, file: &str, point: &str) {
        fs::write(self.paths.root.join(file), point).unwrap();
    }
    fn clear(&self, file: &str) {
        fs::remove_file(self.paths.root.join(file)).unwrap();
    }
    fn original(&self, config: &[u8]) {
        assert_eq!(fs::read(self.bin.join("bazel")).unwrap(), b"original bazel");
        assert_eq!(
            fs::read(self.bin.join("bazelisk")).unwrap(),
            b"original bazelisk"
        );
        assert_eq!(fs::read(&self.paths.config).unwrap(), config);
        for path in [
            "current",
            "installation.json",
            "installation.pending.json",
            "backups/bazel",
            "backups/bazelisk",
        ] {
            assert!(
                fs::symlink_metadata(self.paths.root.join(path)).is_err(),
                "{path} survived rollback"
            );
        }
    }
    fn installed(&self) {
        for name in ["bazel", "bazelisk"] {
            assert_eq!(
                fs::read_link(self.bin.join(name)).unwrap(),
                self.paths.root.join("current")
            );
        }
        assert!(self.paths.root.join("installation.json").exists());
        assert_eq!(
            fs::read(self.paths.root.join("backups/bazel")).unwrap(),
            b"original bazel"
        );
        assert_eq!(
            fs::read(self.paths.root.join("backups/bazelisk")).unwrap(),
            b"original bazelisk"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.paths.socket);
        self.server.take().unwrap().join().unwrap();
    }
}
fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn initial_failures_restore_every_mutated_image() {
    for point in [
        "journal", "drained", "change-0", "change-1", "change-2", "change-3", "change-4",
        "change-5", "change-6",
    ] {
        let fixture = Fixture::new();
        let config = fs::read(&fixture.paths.config).unwrap();
        fixture.hook("install-failure", point);
        let output = fixture.setup();
        assert!(!output.status.success(), "{point}");
        fixture.original(&config);
    }
}
#[test]
fn upgrade_failure_restores_previous_managed_installation_and_original_backups() {
    for point in [
        "journal", "drained", "change-0", "change-1", "change-2", "change-3", "change-4",
    ] {
        let fixture = Fixture::new();
        success(fixture.setup());
        let previous = fixture.home.path().join("previous-image");
        fs::copy(env!("CARGO_BIN_EXE_bazelqueue"), &previous).unwrap();
        fs::remove_file(fixture.paths.root.join("current")).unwrap();
        symlink(&previous, fixture.paths.root.join("current")).unwrap();
        let mut config = Config::load(&fixture.paths).unwrap();
        config.max_builds = 1;
        config.hooks = vec![bazelqueue::config::Hook {
            program: fixture.home.path().join("user-hook"),
            arguments: Vec::new(),
        }];
        config.save(&fixture.paths).unwrap();
        let config = fs::read(&fixture.paths.config).unwrap();
        let manifest = fs::read(fixture.paths.root.join("installation.json")).unwrap();
        fixture.hook("install-failure", point);
        assert!(!fixture.setup().status.success(), "{point}");
        assert_eq!(
            fs::read_link(fixture.paths.root.join("current")).unwrap(),
            previous
        );
        assert_eq!(fs::read(&fixture.paths.config).unwrap(), config);
        assert_eq!(
            fs::read(fixture.paths.root.join("installation.json")).unwrap(),
            manifest
        );
        fixture.installed();
        fixture.clear("install-failure");
        success(fixture.command().arg("uninstall").output().unwrap());
        assert_eq!(
            fs::read(fixture.bin.join("bazelisk")).unwrap(),
            b"original bazelisk"
        );
    }
}
#[test]
fn crash_recovery_and_repeated_recovery_cuts_are_idempotent() {
    for point in [
        "journal", "drained", "change-0", "change-1", "change-2", "change-3", "change-4",
        "change-5", "change-6",
    ] {
        let fixture = Fixture::new();
        let config = fs::read(&fixture.paths.config).unwrap();
        fixture.hook("install-cutpoint", point);
        assert_eq!(fixture.setup().status.code(), Some(86), "{point}");
        fixture.hook("install-cutpoint", "recover-4");
        assert_eq!(
            fixture
                .command()
                .arg("uninstall")
                .output()
                .unwrap()
                .status
                .code(),
            Some(86)
        );
        fixture.clear("install-cutpoint");
        success(fixture.command().arg("uninstall").output().unwrap());
        fixture.original(&config);
        success(fixture.command().arg("uninstall").output().unwrap());
    }
}
#[test]
fn committed_setup_is_finalized_on_reentry_without_rollback() {
    let fixture = Fixture::new();
    fixture.hook("install-cutpoint", "committed");
    assert_eq!(fixture.setup().status.code(), Some(86));
    fixture.clear("install-cutpoint");
    fixture.hook("install-failure", "journal");
    assert!(!fixture.setup().status.success());
    fixture.installed();
    assert!(
        fs::read_link(fixture.paths.root.join("current"))
            .unwrap()
            .exists()
    );
    assert!(
        !fixture
            .paths
            .root
            .join("installation.pending.json")
            .exists()
    );
}
#[test]
fn upgrade_pending_reentry_recovers_previous_image_before_new_transaction() {
    let fixture = Fixture::new();
    success(fixture.setup());
    let previous = fixture.home.path().join("previous-image");
    fs::copy(env!("CARGO_BIN_EXE_bazelqueue"), &previous).unwrap();
    fs::remove_file(fixture.paths.root.join("current")).unwrap();
    symlink(&previous, fixture.paths.root.join("current")).unwrap();
    let config = fs::read(&fixture.paths.config).unwrap();
    fixture.hook("install-cutpoint", "change-3");
    assert_eq!(fixture.setup().status.code(), Some(86));
    fixture.clear("install-cutpoint");
    fixture.hook("install-failure", "journal");
    assert!(!fixture.setup().status.success());
    assert_eq!(
        fs::read_link(fixture.paths.root.join("current")).unwrap(),
        previous
    );
    assert_eq!(fs::read(&fixture.paths.config).unwrap(), config);
    fixture.installed();
}
fn tree(path: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
    fn visit(path: &Path, result: &mut Vec<(PathBuf, Vec<u8>, u32)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), result);
            }
        } else {
            let bytes = if metadata.file_type().is_symlink() {
                use std::os::unix::ffi::OsStrExt;
                fs::read_link(path).unwrap().as_os_str().as_bytes().to_vec()
            } else {
                fs::read(path).unwrap()
            };
            result.push((path.to_owned(), bytes, metadata.permissions().mode()));
        }
    }
    let mut result = Vec::new();
    // The fixture's control socket is an inert IPC surface, not an installation image.
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().is_some_and(|name| name == "state") {
            for entry in fs::read_dir(&path).unwrap() {
                let path = entry.unwrap().path();
                if path.file_name().is_some_and(|name| name == "control.sock") {
                    continue;
                }
                visit(&path, &mut result);
            }
        } else {
            visit(&path, &mut result);
        }
    }
    result.sort();
    result
}
#[test]
fn preview_has_no_mutations_even_with_pending_recovery() {
    let fixture = Fixture::new();
    let before = tree(fixture.home.path());
    success(
        fixture
            .command()
            .args([
                "setup",
                "--preview",
                "--replace",
                "--backend",
                env!("CARGO_BIN_EXE_fixture-backend"),
                "--bin-dir",
            ])
            .arg(&fixture.bin)
            .output()
            .unwrap(),
    );
    assert_eq!(tree(fixture.home.path()), before);
    fixture.hook("install-cutpoint", "change-4");
    assert_eq!(fixture.setup().status.code(), Some(86));
    fixture.clear("install-cutpoint");
    let before = tree(fixture.home.path());
    success(
        fixture
            .command()
            .args(["setup", "--preview"])
            .output()
            .unwrap(),
    );
    assert_eq!(tree(fixture.home.path()), before);
}
#[test]
fn backend_aliases_traversing_future_shims_are_rejected() {
    for name in ["bazel", "bazelisk"] {
        let fixture = Fixture::new();
        fs::remove_file(fixture.bin.join(name)).unwrap();
        symlink(
            env!("CARGO_BIN_EXE_fixture-backend"),
            fixture.bin.join(name),
        )
        .unwrap();
        let alias = fixture.home.path().join("alias");
        symlink(fixture.bin.join(name), &alias).unwrap();
        let output = fixture
            .command()
            .args(["setup", "--replace", "--backend"])
            .arg(alias)
            .arg("--bin-dir")
            .arg(&fixture.bin)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            !fixture
                .paths
                .root
                .join("installation.pending.json")
                .exists()
        );
        assert_eq!(
            fs::read_link(fixture.bin.join(name)).unwrap(),
            PathBuf::from(env!("CARGO_BIN_EXE_fixture-backend"))
        );
    }
}
#[test]
fn relative_destinations_become_absolute_and_different_destinations_are_rejected() {
    let fixture = Fixture::new();
    success(
        fixture
            .command()
            .args([
                "setup",
                "--replace",
                "--backend",
                env!("CARGO_BIN_EXE_fixture-backend"),
                "--bin-dir",
                "bin",
            ])
            .output()
            .unwrap(),
    );
    fixture.installed();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.paths.root.join("installation.json")).unwrap())
            .unwrap();
    for link in manifest["links"].as_array().unwrap() {
        assert!(Path::new(link["path"].as_str().unwrap()).is_absolute());
    }
    let output = fixture
        .command()
        .args([
            "setup",
            "--backend",
            env!("CARGO_BIN_EXE_fixture-backend"),
            "--bin-dir",
            "other-bin",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!fixture.home.path().join("other-bin").exists());
    success(fixture.command().arg("uninstall").output().unwrap());
    assert_eq!(
        fs::read(fixture.bin.join("bazel")).unwrap(),
        b"original bazel"
    );
}
#[test]
fn uninstall_failure_and_crash_cuts_preserve_restorable_ownership() {
    for point in [
        "journal", "drained", "change-0", "change-1", "change-2", "change-3", "change-4",
    ] {
        let fixture = Fixture::new();
        success(fixture.setup());
        fixture.hook("install-failure", point);
        assert!(
            !fixture
                .command()
                .arg("uninstall")
                .output()
                .unwrap()
                .status
                .success(),
            "{point}"
        );
        fixture.installed();
        fixture.clear("install-failure");
        fixture.hook("install-cutpoint", point);
        assert_eq!(
            fixture
                .command()
                .arg("uninstall")
                .output()
                .unwrap()
                .status
                .code(),
            Some(86),
            "{point}"
        );
        fixture.clear("install-cutpoint");
        success(fixture.command().arg("uninstall").output().unwrap());
        assert_eq!(
            fs::read(fixture.bin.join("bazel")).unwrap(),
            b"original bazel"
        );
        assert_eq!(
            fs::read(fixture.bin.join("bazelisk")).unwrap(),
            b"original bazelisk"
        );
    }
}
#[test]
fn committed_uninstall_is_finalized_without_reinstalling_shims() {
    let fixture = Fixture::new();
    success(fixture.setup());
    fixture.hook("install-cutpoint", "committed");
    assert_eq!(
        fixture
            .command()
            .arg("uninstall")
            .output()
            .unwrap()
            .status
            .code(),
        Some(86)
    );
    fixture.clear("install-cutpoint");
    success(fixture.command().arg("uninstall").output().unwrap());
    assert_eq!(
        fs::read(fixture.bin.join("bazel")).unwrap(),
        b"original bazel"
    );
    assert!(!fixture.paths.root.join("installation.json").exists());
    assert!(
        !fixture
            .paths
            .root
            .join("installation.pending.json")
            .exists()
    );
}
#[test]
fn changed_original_backup_blocks_upgrade_and_uninstall() {
    let fixture = Fixture::new();
    success(fixture.setup());
    fs::write(fixture.paths.root.join("backups/bazel"), "edited backup").unwrap();
    assert!(!fixture.setup().status.success());
    assert!(
        !fixture
            .command()
            .arg("uninstall")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        fs::read_link(fixture.bin.join("bazel")).unwrap(),
        fixture.paths.root.join("current")
    );
    assert_eq!(
        fs::read(fixture.paths.root.join("backups/bazel")).unwrap(),
        b"edited backup"
    );
}

#[test]
fn first_install_rollback_removes_new_configuration() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.paths.config).unwrap();
    fixture.hook("install-failure", "change-4");
    assert!(!fixture.setup().status.success());
    assert!(!fixture.paths.config.exists());
    assert!(fs::symlink_metadata(fixture.paths.root.join("current")).is_err());
    assert_eq!(
        fs::read(fixture.bin.join("bazel")).unwrap(),
        b"original bazel"
    );
    assert_eq!(
        fs::read(fixture.bin.join("bazelisk")).unwrap(),
        b"original bazelisk"
    );
    assert!(
        !fixture
            .paths
            .root
            .join("installation.pending.json")
            .exists()
    );
}

#[test]
fn zsh_activation_survives_later_homebrew_precedence_and_honors_zdotdir() {
    let fixture = Fixture::new();
    let profiles = fixture.home.path().join("zsh");
    fs::create_dir(&profiles).unwrap();
    let unmanaged = fixture.home.path().join("unmanaged");
    fs::create_dir(&unmanaged).unwrap();
    for name in ["bazel", "bazelisk"] {
        let path = unmanaged.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let override_path = format!("export PATH='{}':\"$PATH\"\n", unmanaged.display());
    fs::write(profiles.join(".zprofile"), &override_path).unwrap();
    fs::write(profiles.join(".zshrc"), &override_path).unwrap();
    success(
        fixture
            .command()
            .args(["setup", "--replace", "--bin-dir"])
            .arg(&fixture.bin)
            .env("SHELL", "/bin/zsh")
            .env("ZDOTDIR", &profiles)
            .output()
            .unwrap(),
    );
    for mode in ["-lic", "-lc", "-ic", "-c"] {
        let output = Command::new("/bin/zsh")
            .args([mode, "command -v bazel; command -v bazelisk"])
            .env("HOME", fixture.home.path())
            .env("ZDOTDIR", &profiles)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "{}/bazel\n{}/bazelisk\n",
                fixture.bin.display(),
                fixture.bin.display()
            )
        );
    }
    success(fixture.command().arg("uninstall").output().unwrap());
    assert_eq!(
        fs::read_to_string(profiles.join(".zshrc")).unwrap(),
        override_path
    );
    assert!(!profiles.join(".zshenv").exists());
    assert!(!profiles.join(".zlogin").exists());
    assert!(!fixture.home.path().join(".zshenv").exists());
}

#[test]
fn uninstall_preserves_non_utf8_profile_bytes() {
    let fixture = Fixture::new();
    let profile = fixture.home.path().join(".zshenv");
    let original = b"# existing comment \xff\n";
    fs::write(&profile, original).unwrap();
    success(
        fixture
            .command()
            .args(["setup", "--replace", "--bin-dir"])
            .arg(&fixture.bin)
            .env("SHELL", "/bin/zsh")
            .output()
            .unwrap(),
    );
    success(fixture.command().arg("uninstall").output().unwrap());
    assert_eq!(fs::read(&profile).unwrap(), original);
}

#[test]
fn bash_activation_preserves_the_existing_login_profile() {
    let fixture = Fixture::new();
    let profile = fixture.home.path().join(".profile");
    fs::write(&profile, "export EXISTING_LOGIN=kept\n").unwrap();
    success(
        fixture
            .command()
            .args(["setup", "--replace", "--bin-dir"])
            .arg(&fixture.bin)
            .env("SHELL", "/bin/bash")
            .output()
            .unwrap(),
    );
    assert!(!fixture.home.path().join(".bash_profile").exists());
    let output = Command::new("/bin/bash")
        .args([
            "-lc",
            "printf '%s\\n' \"$EXISTING_LOGIN\"; command -v bazel",
        ])
        .env("HOME", fixture.home.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("kept\n{}/bazel\n", fixture.bin.display())
    );
    success(fixture.command().arg("uninstall").output().unwrap());
    assert_eq!(
        fs::read_to_string(profile).unwrap(),
        "export EXISTING_LOGIN=kept\n"
    );
}
