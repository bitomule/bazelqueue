#![forbid(unsafe_code)]
use crate::{
    config::{Config, Paths},
    coordinator, platform,
    protocol::Event,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
struct Manifest {
    binary: PathBuf,
    links: Vec<Link>,
    #[serde(default)]
    profile: Option<PathBlock>,
    #[serde(default)]
    profiles: Vec<PathBlock>,
}
#[derive(Clone, Serialize, Deserialize)]
struct PathBlock {
    path: PathBuf,
    text: String,
    created: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Link {
    path: PathBuf,
    backup: Option<PathBuf>,
    checksum: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Image {
    Missing,
    File { bytes: Vec<u8>, mode: u32 },
    Symlink { target: PathBuf },
}
#[derive(Serialize, Deserialize)]
struct Change {
    path: PathBuf,
    before: Image,
    after: Image,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    committed: bool,
    changes: Vec<Change>,
}

fn image(path: &Path) -> Result<Image> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Image::Missing),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        Ok(Image::Symlink {
            target: fs::read_link(path)?,
        })
    } else if metadata.is_file() {
        Ok(Image::File {
            bytes: fs::read(path)?,
            mode: metadata.permissions().mode() & 0o7777,
        })
    } else {
        bail!("expected a file or link: {}", path.display())
    }
}
fn digest(path: &Path) -> Result<String> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt;
        Ok(format!(
            "link:{}",
            fs::read_link(path)?
                .as_os_str()
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    } else {
        Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
    }
}
fn owned(link: &Link, binary: &Path) -> bool {
    fs::read_link(&link.path).ok().as_deref() == Some(binary)
}
fn sync_parent(path: &Path) -> Result<()> {
    fs::File::open(path.parent().context("path has no parent")?)?.sync_all()?;
    Ok(())
}
fn put_image(path: &Path, value: &Image) -> Result<()> {
    if image(path)? == *value {
        return Ok(());
    }
    if let Image::Missing = value {
        fs::remove_file(path)?;
        return sync_parent(path);
    }
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".bazelqueue-{}-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    // A crash may leave a preparation file, but never a published partial image.
    match fs::remove_file(&temporary) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let outcome = (|| -> Result<()> {
        match value {
            Image::File { bytes, mode } => {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)?;
                file.write_all(bytes)?;
                file.set_permissions(fs::Permissions::from_mode(*mode))?;
                file.sync_all()?;
            }
            Image::Symlink { target } => symlink(target, &temporary)?,
            Image::Missing => unreachable!(),
        }
        fs::rename(&temporary, path)?;
        sync_parent(path)
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(temporary);
    }
    outcome
}
fn anchored(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    })
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let input = anchored(path)?;
    let mut output = PathBuf::new();
    for component in input.components() {
        match component {
            Component::CurDir => (),
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    Ok(output)
}
fn resolved_backend(path: &Path, targets: &[PathBuf], depth: usize) -> Result<PathBuf> {
    if depth > 40 {
        bail!("backend symlink recursion");
    }
    let mut resolved = PathBuf::new();
    let mut components = path.components();
    while let Some(component) = components.next() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                resolved.pop();
                continue;
            }
            other => resolved.push(other.as_os_str()),
        }
        if targets.contains(&resolved) {
            bail!("backend will be replaced by a shim; use the real backend's stable path");
        }
        if fs::symlink_metadata(&resolved)?.file_type().is_symlink() {
            let link = fs::read_link(&resolved)?;
            let mut next = if link.is_absolute() {
                link
            } else {
                resolved
                    .parent()
                    .context("backend link has no parent")?
                    .join(link)
            };
            next.extend(components.map(|part| part.as_os_str()));
            return resolved_backend(&next, targets, depth + 1);
        }
    }
    Ok(resolved)
}
fn validate_backend(path: &Path, binary: &Path, targets: &[PathBuf]) -> Result<()> {
    let mut destinations = Vec::new();
    for target in targets {
        destinations.push(absolute(target)?);
        if let Some(parent) = target
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
        {
            destinations.push(parent.join(target.file_name().context("shim has no filename")?));
        }
    }
    let canonical = resolved_backend(&anchored(path)?, &destinations, 0)
        .context("backend missing or unsafe")?;
    if fs::metadata(path)?.permissions().mode() & 0o111 == 0
        || platform::same_executable(&canonical, binary)
    {
        bail!("backend is not executable or resolves to bazelqueue");
    }
    Ok(())
}
fn guard_backups(manifest: &Manifest) -> Result<()> {
    for link in &manifest.links {
        if let Some(backup) = &link.backup
            && link.checksum.as_deref()
                != Some(
                    &digest(backup)
                        .with_context(|| format!("backup missing: {}", backup.display()))?,
                )
        {
            bail!("backup changed: {}", backup.display());
        }
    }
    Ok(())
}
fn change(path: PathBuf, after: Image) -> Result<Change> {
    Ok(Change {
        before: image(&path)?,
        path,
        after,
    })
}
fn json_image(value: &impl Serialize) -> Result<Image> {
    Ok(Image::File {
        bytes: serde_json::to_vec_pretty(value)?,
        mode: 0o600,
    })
}
fn save_journal(paths: &Paths, journal: &Journal) -> Result<()> {
    platform::atomic_write(
        &paths.root.join("installation.pending.json"),
        &serde_json::to_vec_pretty(journal)?,
    )
}
fn apply(journal: &Journal, after: bool, paths: &Paths, recovering: bool) -> Result<()> {
    // Validate the entire transaction before changing any independently edited file.
    for change in &journal.changes {
        let found = image(&change.path)?;
        if found != change.before && found != change.after {
            bail!(
                "transaction destination was changed independently: {}",
                change.path.display()
            );
        }
    }
    let indices: Vec<_> = if after {
        (0..journal.changes.len()).collect()
    } else {
        (0..journal.changes.len()).rev().collect()
    };
    for index in indices {
        let change = &journal.changes[index];
        if !recovering {
            checkpoint(paths, &format!("prepare-{index}"))?;
        }
        if change.path.parent() == Some(paths.root.join("backups").as_path())
            && !matches!(
                if after { &change.after } else { &change.before },
                Image::Missing
            )
        {
            platform::private_directory(change.path.parent().context("backup has no parent")?)?;
        }
        put_image(
            &change.path,
            if after { &change.after } else { &change.before },
        )?;
        if recovering {
            checkpoint(paths, &format!("recover-{index}"))?;
        } else {
            checkpoint(paths, &format!("change-{index}"))?;
        }
        #[cfg(feature = "test-fixtures")]
        if !recovering
            && after
            && matches!(&change.after, Image::Symlink { target } if target == &paths.root.join("current"))
            && change.path.file_name().is_some_and(|name| name == "bazel")
            && paths.root.join("fail-after-first-link").exists()
        {
            bail!("injected activation failure");
        }
    }
    Ok(())
}
fn checkpoint(paths: &Paths, point: &str) -> Result<()> {
    #[cfg(feature = "test-fixtures")]
    {
        if fs::read_to_string(paths.root.join("install-cutpoint"))
            .ok()
            .as_deref()
            == Some(point)
        {
            std::process::exit(86);
        }
        if fs::read_to_string(paths.root.join("install-failure"))
            .ok()
            .as_deref()
            == Some(point)
        {
            bail!("injected installation failure at {point}");
        }
    }
    let _ = (paths, point);
    Ok(())
}
async fn recover(paths: &Paths) -> Result<()> {
    let pending = paths.root.join("installation.pending.json");
    if !pending.exists() {
        return Ok(());
    }
    let journal: Journal = serde_json::from_slice(&fs::read(&pending)?)
        .context("invalid installation journal; refusing to mutate installation")?;
    apply(&journal, journal.committed, paths, true)?;
    coordinator::control(paths, "resume", None).await?;
    fs::remove_file(&pending)?;
    sync_parent(&pending)
}
async fn transact(paths: &Paths, mut journal: Journal) -> Result<()> {
    save_journal(paths, &journal)?;
    let result = async {
        checkpoint(paths, "journal")?;
        coordinator::control(paths, "drain", None).await?;
        checkpoint(paths, "drained")?;
        let Event::Snapshot { snapshot } = coordinator::control(paths, "status", None).await?
        else {
            bail!("ownership verification unavailable");
        };
        if snapshot.jobs.iter().any(|job| !job.terminal()) {
            bail!("active requests exist; drain and wait before changing installation");
        }
        apply(&journal, true, paths, false)?;
        journal.committed = true;
        save_journal(paths, &journal)?;
        checkpoint(paths, "committed")?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        // The durable commit is authoritative even if finalization failed.
        recover(paths).await?;
        return Err(error);
    }
    recover(paths).await
}
fn stage_binary(paths: &Paths) -> Result<PathBuf> {
    let current = std::env::current_exe()?.canonicalize()?;
    for prefix in ["/opt/homebrew", "/usr/local"] {
        let opt = PathBuf::from(prefix).join("opt/bazelqueue/bin/bazelqueue");
        if platform::same_executable(&current, &opt) {
            return Ok(opt);
        }
    }
    let directory = paths.root.join("executables").join(digest(&current)?);
    platform::private_directory(&directory)?;
    let destination = directory.join("bazelqueue");
    let staged = Image::File {
        bytes: fs::read(current)?,
        mode: 0o755,
    };
    if image(&destination)? != Image::Missing && image(&destination)? != staged {
        bail!("staged executable changed");
    }
    put_image(&destination, &staged)?;
    Ok(destination)
}

pub async fn setup(
    paths: &Paths,
    backend: Option<PathBuf>,
    bin_dir: Option<PathBuf>,
    replace: bool,
    migrate: bool,
    preview: bool,
) -> Result<()> {
    let _lock = if preview {
        None
    } else {
        Some(platform::lock(&paths.root.join("installation.lock"))?)
    };
    if preview && paths.root.join("installation.pending.json").exists() {
        let journal: Journal =
            serde_json::from_slice(&fs::read(paths.root.join("installation.pending.json"))?)?;
        println!(
            "Pending installation transaction: {} (recovery required before setup)",
            if journal.committed {
                "committed"
            } else {
                "uncommitted"
            }
        );
        return Ok(());
    }
    if !preview {
        recover(paths).await?;
    }
    let mut config = Config::load(paths)?;
    if let Some(backend) = backend {
        config.backend = anchored(&backend)?;
    }
    config.validate()?;
    let current = std::env::current_exe()?.canonicalize()?;
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME missing")?);
    let bin_dir = absolute(&bin_dir.unwrap_or_else(|| home.join(".local/bin")))?;
    let targets: Vec<_> = ["bazel", "bazelisk"]
        .iter()
        .map(|name| bin_dir.join(name))
        .collect();
    validate_backend(&config.backend, &current, &targets)?;
    if let Some(backend) = &config.bazel_backend {
        validate_backend(backend, &current, &targets)?;
    }
    let manifest_path = paths.root.join("installation.json");
    let previous = if manifest_path.exists() {
        Some(serde_json::from_slice::<Manifest>(&fs::read(
            &manifest_path,
        )?)?)
    } else {
        None
    };
    if let Some(previous) = &previous {
        if previous
            .links
            .iter()
            .map(|link| &link.path)
            .collect::<Vec<_>>()
            != targets.iter().collect::<Vec<_>>()
        {
            bail!("requested shim destinations differ from the installed destinations");
        }
        if !previous
            .links
            .iter()
            .all(|link| owned(link, &previous.binary))
        {
            bail!("installed links have been changed; refusing to overwrite them");
        }
        guard_backups(previous)?;
    }
    let backup_dir = paths.root.join("backups");
    if let Ok(metadata) = fs::symlink_metadata(&backup_dir)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        bail!("backup directory is not a real directory");
    }
    let mut changes = Vec::new();
    let binary = paths.root.join("current");
    let mut manifest = if let Some(previous) = previous {
        Manifest {
            binary: binary.clone(),
            links: previous.links,
            profile: previous.profile,
            profiles: previous.profiles,
        }
    } else {
        let mut links = Vec::new();
        for path in &targets {
            let original = image(path)?;
            if original != Image::Missing && !replace {
                bail!(
                    "{} exists; inspect setup --preview --replace, then use setup --replace",
                    path.display()
                );
            }
            let backup = backup_dir.join(path.file_name().context("shim has no filename")?);
            if image(&backup)? != Image::Missing {
                bail!("backup already exists; refusing to overwrite it");
            }
            let (backup, checksum) = if original != Image::Missing {
                let checksum = digest(path)?;
                changes.push(change(backup.clone(), original)?);
                (Some(backup), Some(checksum))
            } else {
                (None, None)
            };
            links.push(Link {
                path: path.clone(),
                backup,
                checksum,
            });
        }
        Manifest {
            binary: binary.clone(),
            links,
            profile: None,
            profiles: Vec::new(),
        }
    };
    let in_path = std::env::var_os("PATH").is_some_and(|path| {
        for entry in std::env::split_paths(&path) {
            if let Ok(entry) = absolute(&entry) {
                if entry == bin_dir {
                    return true;
                }
                if entry.join("bazel").exists() || entry.join("bazelisk").exists() {
                    return false;
                }
            }
        }
        false
    });
    if !in_path && manifest.profile.is_none() && manifest.profiles.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let (directory, files): (PathBuf, &[&str]) = if shell.ends_with("zsh") {
            (
                absolute(
                    &std::env::var_os("ZDOTDIR")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| home.clone()),
                )?,
                &[".zshenv", ".zshrc", ".zlogin"],
            )
        } else if shell.ends_with("bash") {
            let login = [".bash_profile", ".bash_login", ".profile"]
                .into_iter()
                .find(|name| home.join(name).exists())
                .unwrap_or(".bash_profile");
            (home.clone(), &[login, ".bashrc"])
        } else {
            bail!("unsupported shell; prepend the shim directory to PATH before setup");
        };
        for file in files {
            let path = directory.join(file);
            let before = image(&path)?;
            let (mut bytes, mode) = match &before {
                Image::Missing => (Vec::new(), 0o644),
                Image::File { bytes, mode } => (bytes.clone(), *mode),
                Image::Symlink { .. } => {
                    bail!("shell profile is a symlink; configure PATH explicitly before setup")
                }
            };
            if !bytes.is_empty() && !bytes.ends_with(b"\n") {
                bytes.push(b'\n');
            }
            let quoted = bin_dir
                .to_str()
                .context("shell activation path must be UTF-8")?
                .replace('\'', "'\"'\"'");
            let text = format!(
                "# bazelqueue PATH begin\nexport PATH='{quoted}':\"$PATH\"\n# bazelqueue PATH end\n"
            );
            bytes.extend_from_slice(text.as_bytes());
            changes.push(change(path.clone(), Image::File { bytes, mode })?);
            manifest.profiles.push(PathBlock {
                path,
                text,
                created: before == Image::Missing,
            });
        }
    }
    println!("Backend: {}", config.backend.display());
    println!("Shims: {}/{{bazel,bazelisk}}", bin_dir.display());
    if migrate {
        for (name, arguments) in [
            ("externo-bazelrc-sync", Vec::new()),
            ("bazelcache", vec!["guard".into()]),
        ] {
            let program = home.join(".local/bin").join(name);
            if program.is_file() && !config.hooks.iter().any(|hook| hook.program == program) {
                config
                    .hooks
                    .push(crate::config::Hook { program, arguments });
            }
        }
        config
            .legacy
            .extend(platform::legacy_invocations(&bin_dir.join("bazelisk"))?);
        config
            .legacy
            .extend(platform::legacy_invocations(&bin_dir.join("bazel"))?);
        config.legacy.sort_by_key(|identity| identity.pid);
        config.legacy.dedup();
    }
    println!("Legacy invocations to drain: {}", config.legacy.len());
    if preview {
        return Ok(());
    }
    let destination = stage_binary(paths)?;
    changes.push(change(
        paths.config.clone(),
        Image::File {
            bytes: toml::to_string_pretty(&config)?.into_bytes(),
            mode: 0o600,
        },
    )?);
    changes.push(change(
        binary.clone(),
        Image::Symlink {
            target: destination,
        },
    )?);
    for link in &manifest.links {
        changes.push(change(
            link.path.clone(),
            Image::Symlink {
                target: binary.clone(),
            },
        )?);
    }
    changes.push(change(manifest_path, json_image(&manifest)?)?);
    // Upgrade journals also guard the immutable backups without consuming them.
    for link in &manifest.links {
        if let Some(backup) = &link.backup
            && !changes.iter().any(|change| change.path == *backup)
        {
            changes.push(change(backup.clone(), image(backup)?)?);
        }
    }
    if migrate {
        coordinator::control(
            paths,
            "track-legacy",
            Some(serde_json::to_string(&targets)?),
        )
        .await?;
    }
    transact(
        paths,
        Journal {
            committed: false,
            changes,
        },
    )
    .await?;
    println!(
        "bazelqueue: installed; ensure {} precedes the backend in PATH",
        bin_dir.display()
    );
    Ok(())
}

pub async fn uninstall(paths: &Paths) -> Result<()> {
    let _lock = platform::lock(&paths.root.join("installation.lock"))?;
    recover(paths).await?;
    let manifest_path = paths.root.join("installation.json");
    if !manifest_path.exists() {
        println!("bazelqueue: no user shims installed");
        return Ok(());
    }
    let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    guard_backups(&manifest)?;
    let mut changes = Vec::new();
    for link in &manifest.links {
        if !owned(link, &manifest.binary) {
            bail!(
                "{} was changed; leaving all links intact",
                link.path.display()
            );
        }
        changes.push(change(
            link.path.clone(),
            if let Some(backup) = &link.backup {
                image(backup)?
            } else {
                Image::Missing
            },
        )?);
    }
    for profile in manifest.profile.iter().chain(&manifest.profiles) {
        if let Image::File { mut bytes, mode } = image(&profile.path)? {
            let block = profile.text.as_bytes();
            let positions: Vec<_> = bytes
                .windows(block.len())
                .enumerate()
                .filter_map(|(index, value)| (value == block).then_some(index))
                .collect();
            if let [index] = positions.as_slice() {
                bytes.drain(*index..*index + block.len());
                changes.push(change(
                    profile.path.clone(),
                    if profile.created && bytes.is_empty() {
                        Image::Missing
                    } else {
                        Image::File { bytes, mode }
                    },
                )?);
            }
        }
    }
    changes.push(change(manifest_path, Image::Missing)?);
    for link in &manifest.links {
        if let Some(backup) = &link.backup {
            changes.push(change(backup.clone(), Image::Missing)?);
        }
    }
    transact(
        paths,
        Journal {
            committed: false,
            changes,
        },
    )
    .await?;
    println!("bazelqueue: user shims restored");
    Ok(())
}

pub async fn doctor(paths: &Paths) -> Result<()> {
    let config = Config::load(paths)?;
    let binary = std::env::current_exe()?;
    validate_backend(&config.backend, &binary, &[])?;
    if let Some(backend) = &config.bazel_backend {
        validate_backend(backend, &binary, &[])?;
    }
    println!("Backend: {}", config.backend.display());
    println!("State: {}", paths.root.display());
    println!("Pressure sensor: {:?}", platform::pressure());
    println!("Configured concurrency: {}", config.max_builds);
    if let Event::Snapshot { snapshot } = coordinator::control(paths, "status", None).await? {
        println!("Coordinator: {}", snapshot.daemon.pid);
        let count = snapshot
            .jobs
            .iter()
            .filter(|job| job.state == "quarantined")
            .count();
        if count > 0 {
            bail!(
                "{count} quarantined reservations; supported servers are probed automatically without restart"
            );
        }
    }
    Ok(())
}
