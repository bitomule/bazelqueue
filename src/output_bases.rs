//! Output bases that `tools/bazel-output-disk` leaves under `<volume>/bazel/`: one per checkout,
//! never removed when the checkout goes. A base is only ever listed for removal when Bazel's own
//! marker names a checkout that no longer exists, or when the hook set it aside as suspect, and
//! in both cases only while no Bazel server runs on it.
use anyhow::{Context, Result};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const WORKSPACE_MARKER: &str = "DO_NOT_BUILD_HERE";
const SERVER_PID: &str = "server/server.pid.txt";
const SUSPECT_INFIX: &str = ".suspect.";
const REMOVING_SUFFIX: &str = ".removing";

#[derive(Debug, PartialEq, Eq)]
pub enum Reason {
    CheckoutGone(PathBuf),
    Suspect,
    Interrupted,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Orphan {
    pub base: PathBuf,
    pub reason: Reason,
}

impl Orphan {
    pub fn describe(&self) -> String {
        match &self.reason {
            Reason::CheckoutGone(checkout) => {
                format!("checkout {} no longer exists", checkout.display())
            }
            Reason::Suspect => "set aside after its Bazel server died uncleanly".to_owned(),
            Reason::Interrupted => "a deletion that did not finish".to_owned(),
        }
    }
}

/// The bases under `volume` that may go. `pid_exists` says whether a process runs; a checkout under
/// `/Volumes/<name>` whose volume is not mounted counts as present, because it cannot be told
/// from a deleted one.
pub fn orphans(volume: &Path, pid_exists: impl Fn(u32) -> bool) -> Result<Vec<Orphan>> {
    let root = volume.join("bazel");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", root.display())),
    };
    let mut found = vec![];
    for entry in entries {
        let base = entry?.path();
        let name = base
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.starts_with('.') || !base.is_dir() {
            continue;
        }
        if name.ends_with(REMOVING_SUFFIX) {
            found.push(Orphan {
                base,
                reason: Reason::Interrupted,
            });
            continue;
        }
        let Some(marked) = fs::read_to_string(base.join(WORKSPACE_MARKER)).ok() else {
            continue;
        };
        let reason = if name.contains(SUSPECT_INFIX) {
            Reason::Suspect
        } else {
            let checkout = PathBuf::from(marked.trim());
            if checkout.as_os_str().is_empty() || checkout_present(&checkout) {
                continue;
            }
            Reason::CheckoutGone(checkout)
        };
        if server_running(&base, &pid_exists) {
            continue;
        }
        found.push(Orphan { base, reason });
    }
    found.sort_by(|a, b| a.base.cmp(&b.base));
    Ok(found)
}

fn checkout_present(checkout: &Path) -> bool {
    if checkout.exists() {
        return true;
    }
    let mut parts = checkout.components().skip(1);
    parts
        .next()
        .is_some_and(|first| first.as_os_str() == "Volumes")
        && parts
            .next()
            .is_some_and(|name| !Path::new("/Volumes").join(name).is_dir())
}

fn server_running(base: &Path, pid_exists: &impl Fn(u32) -> bool) -> bool {
    fs::read_to_string(base.join(SERVER_PID))
        .ok()
        .and_then(|recorded| recorded.trim().parse::<u32>().ok())
        .is_some_and(pid_exists)
}

/// Deletes the base by renaming it first, so an interrupted deletion leaves a `.removing` sibling
/// that listing reports as interrupted and a rerun finishes, never a half-deleted base that still
/// looks usable.
pub fn remove(orphan: &Orphan) -> Result<()> {
    if orphan.reason == Reason::Interrupted {
        make_writable(&orphan.base);
        return fs::remove_dir_all(&orphan.base)
            .with_context(|| format!("cannot delete {}", orphan.base.display()));
    }
    let name = orphan
        .base
        .file_name()
        .context("base has no name")?
        .to_string_lossy();
    let aside = orphan
        .base
        .with_file_name(format!("{name}{REMOVING_SUFFIX}"));
    fs::rename(&orphan.base, &aside)
        .with_context(|| format!("cannot move {} aside", orphan.base.display()))?;
    make_writable(&aside);
    fs::remove_dir_all(&aside).with_context(|| format!("cannot delete {}", aside.display()))
}

/// Bazel makes parts of an output base read-only.
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_symlink() {
        return;
    }
    let mut permissions = meta.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    let _ = fs::set_permissions(path, permissions);
    if meta.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Volume {
        dir: TempDir,
    }

    impl Volume {
        fn new() -> Self {
            Self {
                dir: TempDir::new().unwrap(),
            }
        }

        fn base(&self, name: &str, checkout: &Path) -> PathBuf {
            let base = self.dir.path().join("bazel").join(name);
            fs::create_dir_all(base.join("server")).unwrap();
            fs::write(base.join(WORKSPACE_MARKER), checkout.display().to_string()).unwrap();
            fs::write(base.join("external-file"), "x").unwrap();
            base
        }

        fn orphans(&self, running: &[u32]) -> Vec<Orphan> {
            orphans(self.dir.path(), |pid| running.contains(&pid)).unwrap()
        }
    }

    fn checkout(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("checkout");
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_base_of_a_checkout_that_is_gone_is_listed_and_one_of_a_present_checkout_is_not() {
        let volume = Volume::new();
        let workspaces = TempDir::new().unwrap();
        let present = checkout(&workspaces);
        volume.base("aaaa", &present);
        let gone = workspaces.path().join("deleted");
        volume.base("bbbb", &gone);
        let listed = volume.orphans(&[]);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].base.file_name().unwrap(), "bbbb");
        assert_eq!(listed[0].reason, Reason::CheckoutGone(gone));
    }

    #[test]
    fn a_base_whose_server_still_runs_is_never_listed() {
        let volume = Volume::new();
        let base = volume.base("aaaa", Path::new("/nowhere/deleted"));
        fs::write(base.join(SERVER_PID), "4242\n").unwrap();
        assert!(volume.orphans(&[4242]).is_empty());
        assert_eq!(
            volume.orphans(&[]).len(),
            1,
            "a recorded pid with no process is not a server"
        );
    }

    #[test]
    fn a_folder_without_bazels_marker_is_never_listed() {
        let volume = Volume::new();
        let stranger = volume.dir.path().join("bazel/photos");
        fs::create_dir_all(&stranger).unwrap();
        fs::write(volume.dir.path().join("bazel/loose-file"), "x").unwrap();
        assert!(volume.orphans(&[]).is_empty());
    }

    #[test]
    fn a_checkout_on_a_volume_that_is_not_mounted_counts_as_present() {
        let volume = Volume::new();
        volume.base("aaaa", Path::new("/Volumes/NoSuchVolumeForTests/checkout"));
        assert!(volume.orphans(&[]).is_empty());
    }

    #[test]
    fn a_base_set_aside_as_suspect_is_listed_even_though_its_checkout_exists() {
        let volume = Volume::new();
        let workspaces = TempDir::new().unwrap();
        let present = checkout(&workspaces);
        volume.base("aaaa.suspect.1700000000", &present);
        let listed = volume.orphans(&[]);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].reason, Reason::Suspect);
    }

    #[test]
    fn removing_deletes_read_only_files_and_nothing_next_to_it() {
        use std::os::unix::fs::PermissionsExt;
        let volume = Volume::new();
        let gone = volume.base("aaaa", Path::new("/nowhere/deleted"));
        let neighbour = volume.base("bbbb", Path::new("/nowhere/other"));
        fs::create_dir_all(gone.join("install")).unwrap();
        fs::write(gone.join("install/tool"), "x").unwrap();
        fs::set_permissions(gone.join("install/tool"), fs::Permissions::from_mode(0o444)).unwrap();
        fs::set_permissions(gone.join("install"), fs::Permissions::from_mode(0o555)).unwrap();
        let listed = volume.orphans(&[]);
        let first = listed.iter().find(|orphan| orphan.base == gone).unwrap();
        remove(first).unwrap();
        assert!(!gone.exists());
        assert!(!gone.with_file_name("aaaa.removing").exists());
        assert!(neighbour.exists());
    }

    #[test]
    fn a_deletion_that_was_interrupted_is_listed_and_a_rerun_finishes_it() {
        let volume = Volume::new();
        let half = volume.base("aaaa.removing", Path::new("/nowhere/deleted"));
        let listed = volume.orphans(&[]);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].reason, Reason::Interrupted);
        remove(&listed[0]).unwrap();
        assert!(!half.exists());
    }

    #[test]
    fn a_volume_without_a_bazel_folder_has_nothing_to_list() {
        let volume = Volume::new();
        assert!(volume.orphans(&[]).is_empty());
    }
}
