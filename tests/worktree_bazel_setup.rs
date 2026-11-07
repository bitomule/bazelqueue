use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

struct Checkout {
    dir: TempDir,
    root: PathBuf,
    shared: Option<PathBuf>,
}

impl Checkout {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let init = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&root)
            .output()
            .unwrap();
        assert!(init.status.success());
        Self {
            dir,
            root,
            shared: None,
        }
    }

    fn setup_from(&self, cwd: &Path) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools/worktree-bazel-setup");
        let mut command = Command::new(script);
        command.current_dir(cwd);
        command.env_remove("BAZEL_SHARED_CACHE_DIR");
        if let Some(shared) = &self.shared {
            command.env("BAZEL_SHARED_CACHE_DIR", shared);
        }
        command.output().unwrap()
    }

    fn setup(&self) -> Output {
        self.setup_from(&self.root)
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }

    fn ignored(&self, name: &str) -> bool {
        Command::new("git")
            .current_dir(&self.root)
            .args(["check-ignore", "-q", name])
            .status()
            .unwrap()
            .success()
    }
}

#[test]
fn output_base_is_an_absolute_path_inside_the_checkout() {
    let checkout = Checkout::new();
    assert!(checkout.setup().status.success());
    assert_eq!(
        checkout.read(".bazelrc.local"),
        format!(
            "startup --output_base={}/.bazel-output\n",
            checkout.root.display()
        )
    );
    assert!(
        checkout
            .read(".bazelrc")
            .contains("try-import %workspace%/.bazelrc.local")
    );
    assert!(checkout.ignored(".bazelrc.local"));
    assert!(checkout.ignored(".bazel-output/anything"));
    drop(checkout.dir);
}

#[test]
fn running_it_from_a_subdirectory_targets_the_checkout_root() {
    let checkout = Checkout::new();
    let nested = checkout.root.join("a/b");
    fs::create_dir_all(&nested).unwrap();
    assert!(checkout.setup_from(&nested).status.success());
    assert!(
        checkout
            .read(".bazelrc.local")
            .contains(&format!("{}/.bazel-output", checkout.root.display()))
    );
    assert!(!nested.join(".bazelrc.local").exists());
    assert_eq!(checkout.read(".bazelignore"), ".bazel-output\n");
    assert!(!nested.join(".bazelignore").exists());
}

#[test]
fn running_it_twice_changes_nothing_and_keeps_existing_settings() {
    let checkout = Checkout::new();
    fs::write(checkout.root.join(".bazelrc"), "build -c opt\n").unwrap();
    fs::write(checkout.root.join(".gitignore"), "/target/\n").unwrap();
    assert!(checkout.setup().status.success());
    let first = (
        checkout.read(".bazelrc"),
        checkout.read(".gitignore"),
        checkout.read(".bazelrc.local"),
    );
    assert!(checkout.setup().status.success());
    assert_eq!(
        first,
        (
            checkout.read(".bazelrc"),
            checkout.read(".gitignore"),
            checkout.read(".bazelrc.local")
        )
    );
    assert!(first.0.starts_with("build -c opt\n"));
    assert!(first.1.starts_with("/target/\n"));
    assert_eq!(first.0.matches("try-import").count(), 1);
}

#[test]
fn a_shared_cache_dir_adds_the_cache_and_trim_flags() {
    let mut checkout = Checkout::new();
    let shared = checkout.root.join("volume");
    checkout.shared = Some(shared.clone());
    assert!(checkout.setup().status.success());
    let shared = shared.display();
    assert_eq!(
        checkout.read(".bazelrc.local"),
        format!(
            "startup --output_base={root}/.bazel-output\n\
             common --disk_cache={shared}/disk-cache\n\
             common --repository_cache={shared}/repo-cache\n\
             common --experimental_disk_cache_gc_max_size=12G\n\
             common --experimental_disk_cache_gc_max_age=7d\n",
            root = checkout.root.display()
        )
    );
}

#[test]
fn without_a_shared_cache_dir_only_the_output_base_is_set() {
    let checkout = Checkout::new();
    assert!(checkout.setup().status.success());
    assert!(!checkout.read(".bazelrc.local").contains("disk_cache"));
}

#[test]
fn bazelignore_preserves_existing_content_and_excludes_the_output_base_once() {
    for (existing, expected) in [
        (None, ".bazel-output\n"),
        (Some(""), ".bazel-output\n"),
        (
            Some("# Keep generated sources out\nvendor\n\n"),
            "# Keep generated sources out\nvendor\n\n.bazel-output\n",
        ),
        (Some("vendor"), "vendor\n.bazel-output\n"),
        (
            Some("# .bazel-output\n.bazel-output-old\n"),
            "# .bazel-output\n.bazel-output-old\n.bazel-output\n",
        ),
        (Some("vendor\n.bazel-output\n"), "vendor\n.bazel-output\n"),
        (Some("vendor\n.bazel-output"), "vendor\n.bazel-output"),
    ] {
        let checkout = Checkout::new();
        if let Some(existing) = existing {
            fs::write(checkout.root.join(".bazelignore"), existing).unwrap();
        }
        for _ in 0..2 {
            let output = checkout.setup();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(checkout.read(".bazelignore"), expected, "{existing:?}");
        }
    }
}
