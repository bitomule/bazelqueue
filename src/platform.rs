use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::Path,
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    pub birth: String,
}

pub fn identity(pid: u32) -> Option<Identity> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info);
        let read = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size as i32,
            )
        };
        if read != size as i32 || info.pbi_status == 5 {
            return None;
        }
        Some(Identity {
            pid,
            birth: format!(
                "{}:{}:{}",
                System::boot_time(),
                info.pbi_start_tvsec,
                info.pbi_start_tvusec
            ),
        })
    }
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, tail) = stat.rsplit_once(')')?;
        let fields: Vec<_> = tail.split_whitespace().collect();
        if fields.first() == Some(&"Z") {
            return None;
        }
        Some(Identity {
            pid,
            birth: format!(
                "{}:{}",
                fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .ok()?
                    .trim(),
                fields.get(19)?
            ),
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

pub fn current_identity() -> Result<Identity> {
    identity(std::process::id()).context("cannot verify own process identity")
}
pub fn alive(owner: &Identity) -> bool {
    identity(owner.pid).as_ref() == Some(owner)
}
pub fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn private_directory(path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.uid() != uid() {
            bail!("state directory is not privately owned: {}", path.display());
        }
    } else {
        fs::create_dir_all(path)?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != uid() {
        bail!("file is not owned by this user");
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub fn lock(path: &Path) -> Result<File> {
    let file = private_file(path)?;
    file.try_lock_exclusive()?;
    Ok(file)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    private_directory(parent)?;
    let temp = parent.join(format!(".write-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    let outcome = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temp);
    }
    outcome
}

pub fn same_executable(a: &Path, b: &Path) -> bool {
    matches!((fs::metadata(a),fs::metadata(b)),(Ok(x),Ok(y)) if x.dev()==y.dev() && x.ino()==y.ino())
}

pub fn total_memory_mib() -> Option<u64> {
    let mut system = System::new();
    system.refresh_memory();
    let amount = system.total_memory() / 1024 / 1024;
    (amount > 0).then_some(amount)
}

pub fn pressure() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let name = c"kern.memorystatus_vm_pressure_level";
        let mut value: u32 = 0;
        let mut length = std::mem::size_of_val(&value);
        let code = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut value as *mut u32).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        (code == 0).then_some(value)
    }
    #[cfg(target_os = "linux")]
    {
        let mut system = System::new();
        system.refresh_memory();
        if system.total_memory() == 0 {
            None
        } else {
            Some(if system.available_memory() * 10 < system.total_memory() {
                4
            } else {
                1
            })
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

pub fn process_memory(owners: &[Identity]) -> u64 {
    if owners.is_empty() {
        return 0;
    }
    let mut system = System::new();
    let mut pids: std::collections::HashSet<_> = owners
        .iter()
        .filter(|owner| alive(owner))
        .map(|owner| Pid::from_u32(owner.pid))
        .collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    loop {
        let before = pids.len();
        for (pid, process) in system.processes() {
            if process
                .parent()
                .is_some_and(|parent| pids.contains(&parent))
            {
                pids.insert(*pid);
            }
        }
        if pids.len() == before {
            break;
        }
    }
    pids.iter()
        .filter_map(|pid| system.process(*pid))
        .map(|process| process.memory() / 1024 / 1024)
        .sum()
}

pub fn bazel_servers(workspace: &str) -> Result<Vec<crate::protocol::NativeServer>> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_user(UpdateKind::OnlyIfNotSet),
    );
    if system.process(Pid::from_u32(std::process::id())).is_none() {
        bail!("process discovery unavailable");
    }
    let mut servers = Vec::new();
    for (pid, process) in system.processes() {
        if process.user_id().is_none_or(|uid| **uid != self::uid()) {
            continue;
        }
        let mut directory = None;
        let mut base = None;
        for arg in process.cmd() {
            let text = arg.to_string_lossy();
            if let Some(value) = text.strip_prefix("--workspace_directory=") {
                directory = Some(value.to_owned());
            }
            if let Some(value) = text.strip_prefix("--output_base=") {
                base = Some(value.to_owned());
            }
        }
        if directory.as_deref() == Some(workspace)
            && let (Some(base), Some(identity)) = (base, identity(pid.as_u32()))
        {
            servers.push(crate::protocol::NativeServer {
                identity,
                output_base: base,
                workspace: workspace.into(),
                version: "unverified".into(),
            });
        }
        if process.cmd().is_empty()
            && (process.name().to_string_lossy().contains("bazel(") || process.name() == "java")
        {
            bail!("native server arguments unavailable");
        }
    }
    Ok(servers)
}

pub fn legacy_invocations(shim: &Path) -> Result<Vec<Identity>> {
    legacy_invocations_for(&[shim.to_owned()])
}
pub fn legacy_invocations_for(shims: &[std::path::PathBuf]) -> Result<Vec<Identity>> {
    if shims.is_empty() {
        return Ok(Vec::new());
    }
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_user(UpdateKind::OnlyIfNotSet),
    );
    if system.process(Pid::from_u32(std::process::id())).is_none() {
        bail!("legacy process discovery unavailable");
    }
    let mut owners = Vec::new();
    for (pid, process) in system.processes() {
        if process.user_id().is_none_or(|user| **user != uid())
            || !["bash", "sh", "zsh"]
                .iter()
                .any(|name| process.name() == std::ffi::OsStr::new(name))
        {
            continue;
        }
        let Some(owner) = identity(pid.as_u32()) else {
            continue;
        };
        if process.cmd().is_empty() {
            bail!("legacy invocation arguments unavailable");
        }
        if let Some(script) = shell_script(process.cmd(), process.cwd())?
            && shims.contains(&script)
            && owner.pid != std::process::id()
        {
            owners.push(owner);
        }
    }
    Ok(owners)
}

fn shell_script(
    args: &[std::ffi::OsString],
    cwd: Option<&Path>,
) -> Result<Option<std::path::PathBuf>> {
    let mut index = 1;
    while let Some(argument) = args.get(index) {
        let bytes = argument.as_bytes();
        if bytes == b"--" {
            index += 1;
            break;
        }
        if bytes.starts_with(b"--") {
            index += if [b"--rcfile".as_slice(), b"--init-file"].contains(&bytes) {
                2
            } else {
                1
            };
            continue;
        }
        if bytes.starts_with(b"-") || bytes.starts_with(b"+") {
            if bytes[1..].contains(&b'c') || bytes[1..].contains(&b's') {
                return Ok(None);
            }
            index += if bytes
                .last()
                .is_some_and(|byte| *byte == b'o' || *byte == b'O')
            {
                2
            } else {
                1
            };
            continue;
        }
        break;
    }
    let Some(argument) = args.get(index) else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(argument);
    let input = if path.is_absolute() {
        path
    } else {
        cwd.context("legacy script working directory unavailable")?
            .join(path)
    };
    let mut output = std::path::PathBuf::new();
    for component in input.components() {
        match component {
            std::path::Component::CurDir => (),
            std::path::Component::ParentDir => {
                output.pop();
            }
            component => output.push(component.as_os_str()),
        }
    }
    Ok(Some(output))
}

pub fn signal_group(pid: u32, signal: i32) -> Result<()> {
    let code = unsafe { libc::kill(-(pid as i32), signal) };
    if code != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn set_foreground(fd: RawFd, group: i32) -> Result<()> {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous);
    }
    let result = unsafe { libc::tcsetpgrp(fd, group) };
    unsafe {
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
    }
    if result < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub struct Terminal {
    file: File,
    previous: i32,
}
impl Terminal {
    pub fn handoff(pid: u32) -> Result<Option<Self>> {
        let file = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };
        let previous = unsafe { libc::tcgetpgrp(file.as_raw_fd()) };
        if previous < 0 || previous != unsafe { libc::getpgrp() } {
            return Ok(None);
        }
        set_foreground(file.as_raw_fd(), pid as i32)?;
        Ok(Some(Self { file, previous }))
    }
    pub fn restore(&self) {
        let _ = set_foreground(self.file.as_raw_fd(), self.previous);
    }
    pub fn foreground(&self, pid: u32) {
        let _ = set_foreground(self.file.as_raw_fd(), pid as i32);
    }
    pub fn caller_foreground(&self) -> bool {
        unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) == self.previous }
    }
    pub fn suspend_caller(&self) {
        self.restore();
        unsafe {
            libc::kill(-libc::getpgrp(), libc::SIGSTOP);
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let mut uid = 0;
        let mut gid = 0;
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(uid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&credentials) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(credentials.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        bail!("unsupported platform")
    }
}

pub fn signal_exit(signal: i32) -> ! {
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::kill(libc::getpid(), signal);
    }
    std::process::exit(128 + signal)
}

pub fn inherit_control(command: &mut std::process::Command, fd: RawFd) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(move || {
            if fd != 3 && libc::dup2(fd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn control_socket() -> Result<std::os::unix::net::UnixStream> {
    use std::os::fd::FromRawFd;
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    stream.set_nonblocking(true)?;
    unsafe {
        libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    Ok(stream)
}

pub fn signal_pid(pid: u32, signal: i32) {
    unsafe {
        libc::kill(pid as i32, signal);
    }
}

pub fn stopped(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info);
        unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size as i32,
            ) == size as i32
                && info.pbi_status == libc::SSTOP
        }
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|text| {
                text.rsplit_once(')')
                    .map(|(_, tail)| tail.trim_start().starts_with('T'))
            })
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod legacy_tests {
    use super::*;
    #[test]
    fn shell_script_operand_is_distinct_from_data_and_command_arguments() {
        let args = |parts: &[&str]| {
            parts
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shell_script(
                &args(&["bash", "-euo", "pipefail", "/user/bin/bazelisk"]),
                None
            )
            .unwrap(),
            Some("/user/bin/bazelisk".into())
        );
        assert_eq!(
            shell_script(
                &args(&["bash", "--norc", "-eu", "./bazelisk", "build"]),
                Some(Path::new("/user/bin"))
            )
            .unwrap(),
            Some("/user/bin/bazelisk".into())
        );
        assert_eq!(
            shell_script(
                &args(&["bash", "/tmp/other.sh", "/user/bin/bazelisk"]),
                None
            )
            .unwrap(),
            Some("/tmp/other.sh".into())
        );
        assert_eq!(
            shell_script(
                &args(&["bash", "-c", "echo ok", "/user/bin/bazelisk"]),
                None
            )
            .unwrap(),
            None
        );
        assert_eq!(
            shell_script(&args(&["bash", "-s", "/user/bin/bazelisk"]), None).unwrap(),
            None
        );
        assert!(shell_script(&args(&["bash", "./bazelisk"]), None).is_err());
    }
}
