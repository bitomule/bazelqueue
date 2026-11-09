use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

const IMPORT: &str = "build -c opt\ntry-import %workspace%/.bazelrc.local\n";
const IGNORES: &str =
    "/target/\n/.bazel-output/\n/.bazelrc.local\n/.bazelrc.local.*\n/.bazel-output-disk.log\n";
const BEGIN: &str = "# BEGIN bazelqueue worktree setup\n";
const END: &str = "# END bazelqueue worktree setup\n";

struct Checkout {
    _dir: TempDir,
    root: PathBuf,
}

impl Checkout {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let checkout = Self { _dir: dir, root };
        assert!(checkout.git(&["init", "-q"]).status.success());
        checkout.write(".bazelrc", IMPORT);
        checkout.write(".gitignore", IGNORES);
        checkout.write(
            ".bazelignore",
            "# Existing exclusions\nvendor\n.bazel-output\n",
        );
        checkout.commit();
        checkout
    }

    fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .current_dir(&self.root)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap()
    }

    fn commit(&self) {
        assert!(self.git(&["add", "."]).status.success());
        assert!(
            self.git(&[
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
    }

    fn setup_from(&self, cwd: &Path, args: &[&str], shared: Option<&Path>) -> Output {
        Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/worktree-bazel-setup"))
            .current_dir(cwd)
            .args(args)
            .env_remove("BAZEL_SHARED_CACHE_DIR")
            .envs(shared.map(|path| ("BAZEL_SHARED_CACHE_DIR", path)))
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", &self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap()
    }

    fn setup(&self) -> Output {
        self.setup_from(&self.root, &[], None)
    }
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }
    fn write(&self, name: &str, body: &str) {
        fs::write(self.root.join(name), body).unwrap();
    }
    fn clean(&self) {
        let output = self.git(&["diff", "--exit-code", "HEAD", "--"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            self.git(&["ls-files", "--others", "--exclude-standard"])
                .stdout
                .is_empty()
        );
    }
    fn expected(&self) -> String {
        format!(
            "{BEGIN}startup --output_base={}/.bazel-output\n{END}",
            self.root.display()
        )
    }
}

#[test]
fn prepared_checkout_stays_clean_and_setup_is_idempotent() {
    let checkout = Checkout::new();
    for _ in 0..2 {
        let output = checkout.setup();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(checkout.read(".bazelrc.local"), checkout.expected());
        checkout.clean();
    }
}

#[test]
fn subdirectory_and_explicit_output_base_target_the_checkout() {
    let checkout = Checkout::new();
    let nested = checkout.root.join("a/b");
    fs::create_dir_all(&nested).unwrap();
    assert!(checkout.setup_from(&nested, &[], None).status.success());
    assert_eq!(checkout.read(".bazelrc.local"), checkout.expected());
    assert!(!nested.join(".bazelrc.local").exists());
    let external = checkout.root.parent().unwrap().join("external-base");
    assert!(
        checkout
            .setup_from(&nested, &[external.to_str().unwrap()], None)
            .status
            .success()
    );
    assert!(
        checkout
            .read(".bazelrc.local")
            .contains(&format!("startup --output_base={}\n", external.display()))
    );
    checkout.clean();
}

#[test]
fn shared_cache_and_custom_settings_survive_repeated_setup() {
    let checkout = Checkout::new();
    let custom = "# My checkout\nbuild --jobs=3\n";
    checkout.write(".bazelrc.local", custom);
    let shared = checkout.root.parent().unwrap().join("shared");
    for _ in 0..2 {
        let output = checkout.setup_from(&checkout.root, &[], Some(&shared));
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            checkout.read(".bazelrc.local"),
            format!(
                "{custom}{BEGIN}startup --output_base={root}/.bazel-output\ncommon --disk_cache={shared}/disk-cache\ncommon --repository_cache={shared}/repo-cache\ncommon --experimental_disk_cache_gc_max_size=12G\ncommon --experimental_disk_cache_gc_max_age=7d\n{END}",
                root = checkout.root.display(),
                shared = shared.display()
            )
        );
        checkout.clean();
    }
    assert!(checkout.setup().status.success());
    assert_eq!(
        checkout.read(".bazelrc.local"),
        format!("{custom}{}", checkout.expected())
    );
    checkout.clean();
}

#[test]
fn missing_preparation_fails_without_writing_any_configuration() {
    for (file, body, message) in [
        (
            ".bazelrc",
            "build -c opt\n",
            "try-import %workspace%/.bazelrc.local",
        ),
        (
            ".bazelignore",
            "vendor",
            "add .bazel-output to .bazelignore",
        ),
        (".gitignore", "/target/\n", "ignore /.bazel-output/"),
        (".gitignore", "/.bazel-output/\n", "ignore /.bazelrc.local"),
        (
            ".gitignore",
            "/.bazel-output/\n/.bazelrc.local\n",
            "ignore /.bazelrc.local.*",
        ),
        (
            ".gitignore",
            "/.bazel-output/\n/.bazelrc.local\n/.bazelrc.local.*\n",
            "ignore /.bazel-output-disk.log",
        ),
    ] {
        let checkout = Checkout::new();
        checkout.write(file, body);
        checkout.commit();
        checkout.write(".bazelrc.local", "# Existing local settings\n");
        for _ in 0..2 {
            let output = checkout.setup();
            assert!(!output.status.success(), "{file}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(message),
                "{output:?}"
            );
            assert_eq!(checkout.read(file), body);
            assert_eq!(
                checkout.read(".bazelrc.local"),
                "# Existing local settings\n"
            );
            assert!(!fs::read_dir(&checkout.root).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".bazelrc.local.")
            }));
            assert!(
                checkout
                    .git(&["diff", "--exit-code", "HEAD"])
                    .status
                    .success()
            );
        }
    }
}

#[test]
fn completely_unprepared_checkout_is_not_silently_initialized() {
    let checkout = Checkout::new();
    for file in [".bazelrc", ".bazelignore", ".gitignore"] {
        fs::remove_file(checkout.root.join(file)).unwrap();
    }
    assert!(checkout.git(&["add", "-u"]).status.success());
    assert!(
        checkout
            .git(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "unprepared"
            ])
            .status
            .success()
    );
    let output = checkout.setup();
    assert!(!output.status.success());
    for file in [".bazelrc", ".bazelignore", ".gitignore", ".bazelrc.local"] {
        assert!(!checkout.root.join(file).exists());
    }
    checkout.clean();
}

#[test]
fn git_local_excludes_are_accepted_without_editing_gitignore() {
    let checkout = Checkout::new();
    checkout.write(".gitignore", "/target/\n");
    checkout.write(".git/info/exclude", IGNORES);
    checkout.commit();
    for _ in 0..2 {
        assert!(checkout.setup().status.success());
        checkout.clean();
    }
    assert_eq!(checkout.read(".git/info/exclude"), IGNORES);
}

#[test]
fn legacy_generated_files_are_migrated_but_mixed_options_are_rejected() {
    for legacy in [
        "startup --output_base=/old/base\n",
        "startup --output_base=/old/base\ncommon --disk_cache=/shared/disk-cache\ncommon --repository_cache=/shared/repo-cache\ncommon --experimental_disk_cache_gc_max_size=12G\ncommon --experimental_disk_cache_gc_max_age=7d\n",
    ] {
        let checkout = Checkout::new();
        checkout.write(".bazelrc.local", legacy);
        assert!(checkout.setup().status.success());
        assert_eq!(checkout.read(".bazelrc.local"), checkout.expected());
        checkout.clean();
    }
    for ambiguous in [
        "# Custom\nstartup --output_base=/mine\n",
        "build --disk_cache=/mine\n",
        "# BEGIN bazelqueue worktree setup\nbuild --jobs=3\n",
        "# END bazelqueue worktree setup\n",
    ] {
        let checkout = Checkout::new();
        checkout.write(".bazelrc.local", ambiguous);
        let output = checkout.setup();
        assert!(!output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous .bazelrc.local"));
        assert_eq!(checkout.read(".bazelrc.local"), ambiguous);
        checkout.clean();
    }
}

#[test]
fn check_mode_is_read_only_and_tracked_local_settings_are_rejected() {
    let checkout = Checkout::new();
    assert!(
        checkout
            .setup_from(&checkout.root, &["--check"], None)
            .status
            .success()
    );
    assert!(!checkout.root.join(".bazelrc.local").exists());
    checkout.write(".bazelrc.local", "# Tracked settings\n");
    assert!(
        checkout
            .git(&["add", "-f", ".bazelrc.local"])
            .status
            .success()
    );
    checkout.commit();
    assert!(!checkout.setup().status.success());
    assert_eq!(checkout.read(".bazelrc.local"), "# Tracked settings\n");
    checkout.clean();
}

#[test]
fn own_bazelignore_excludes_the_output_base_and_nested_worktrees() {
    let ignore =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(".bazelignore")).unwrap();
    let entries: Vec<&str> = ignore.lines().collect();
    assert!(entries.contains(&".bazel-output"));
    assert!(entries.contains(&".claude/worktrees"));
}
