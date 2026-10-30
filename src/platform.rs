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

/// Signal only the process whose birth identity was recorded, never a PID
/// supplied by stale server metadata. Completion still requires observing exit.
pub fn signal_verified(owner: &Identity, signal: i32) -> Result<bool> {
    if !alive(owner) {
        return Ok(false);
    }
    if unsafe { libc::kill(owner.pid as i32, signal) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(true)
}

#[cfg(test)]
pub fn fixture_fifo(path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: path is NUL-terminated; the private test directory owns this FIFO.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// Match only the recorded process, not a global scan of unrelated JVMs.
pub fn server_matches(server: &crate::protocol::NativeServer) -> bool {
    let mut system = System::new();
    let pid = Pid::from_u32(server.identity.pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    let Some(process) = system.process(pid) else {
        return false;
    };
    process.user_id().is_some_and(|user| **user == uid())
        && process.cmd().iter().any(|arg| {
            arg == std::ffi::OsStr::new(&format!("--workspace_directory={}", server.workspace))
        })
        && process.cmd().iter().any(|arg| {
            arg == std::ffi::OsStr::new(&format!("--output_base={}", server.output_base))
        })
        && alive(&server.identity)
}

/// None: still stopping. Some(false): a child exists; no descendant may be
/// signalled from a process-table snapshot. Some(true): the stopped server is
/// a leaf and cannot fork between this proof and its identity-checked signal.
pub fn stopped_leaf(owner: &Identity) -> Result<Option<bool>> {
    if !alive(owner) {
        bail!("server identity changed during retirement");
    }
    if !stopped(owner.pid) {
        return Ok(None);
    }
    #[cfg(target_os = "macos")]
    {
        let mut child: libc::pid_t = 0;
        // SAFETY: libproc receives exactly one writable pid_t. We only ask
        // whether any child exists, so a nonempty/truncated list means refuse.
        let (count, error) = unsafe {
            *libc::__error() = 0;
            let count = libc::proc_listchildpids(
                owner.pid as i32,
                (&raw mut child).cast(),
                std::mem::size_of_val(&child) as i32,
            );
            (count, *libc::__error())
        };
        if count < 0 || error != 0 {
            bail!("native child enumeration unavailable");
        }
        if !alive(owner) || !stopped(owner.pid) {
            bail!("server changed during leaf verification");
        }
        Ok(Some(count == 0))
    }
    #[cfg(target_os = "linux")]
    {
        let tasks = || -> Result<Vec<std::path::PathBuf>> {
            let mut tasks = fs::read_dir(format!("/proc/{}/task", owner.pid))?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?;
            tasks.sort();
            Ok(tasks)
        };
        let before = tasks()?;
        for task in &before {
            let stat = fs::read_to_string(task.join("stat"))?;
            let state = stat
                .rsplit_once(')')
                .context("invalid thread status")?
                .1
                .trim_start();
            if !state.starts_with('T') {
                return Ok(None);
            }
            if !fs::read_to_string(task.join("children"))?.trim().is_empty() {
                return Ok(Some(false));
            }
        }
        if before != tasks()? || !alive(owner) {
            return Ok(None);
        }
        Ok(Some(true))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("native leaf verification unavailable");
    }
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

pub fn wait_lock(path: &Path) -> Result<File> {
    let file = private_file(path)?;
    file.lock_exclusive()?;
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

/// Stateful CPU sampling stays at the native boundary; the scheduler receives values only.
#[derive(Default)]
pub struct ResourceSampler {
    system: System,
    cpu_ready: bool,
}

impl ResourceSampler {
    pub fn sample(&mut self, at_ms: u64) -> crate::resources::Sample {
        self.system.refresh_memory();
        self.system.refresh_cpu_usage();
        let total_mib = self.system.total_memory() / 1_048_576;
        let mut sample = crate::resources::Sample {
            at_ms,
            pressure: pressure(),
            total_mib,
            available_mib: (total_mib > 0).then(|| self.system.available_memory() / 1_048_576),
            cpu_busy_percent: self
                .cpu_ready
                .then(|| f64::from(self.system.global_cpu_usage())),
            load_per_cpu: Some(System::load_average().one / self.system.cpus().len().max(1) as f64),
            ..Default::default()
        };
        self.cpu_ready = true;
        #[cfg(target_os = "macos")]
        {
            if let Some(percent) =
                sysctl_u32(c"kern.memorystatus_level").filter(|value| *value <= 100)
            {
                sample.available_mib = Some(total_mib * u64::from(percent) / 100);
            }
            let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
            let mut count = libc::HOST_VM_INFO64_COUNT;
            static HOST: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
            #[allow(deprecated)]
            let host = *HOST.get_or_init(|| unsafe { libc::mach_host_self() });
            // SAFETY: stats has HOST_VM_INFO64_COUNT integer slots and host is a valid send right.
            let result = unsafe {
                libc::host_statistics64(
                    host,
                    libc::HOST_VM_INFO64,
                    (&raw mut stats).cast(),
                    &raw mut count,
                )
            };
            if result == libc::KERN_SUCCESS && count == libc::HOST_VM_INFO64_COUNT {
                // SAFETY: sysconf reads a system constant.
                if let Ok(page) = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }) {
                    sample.swap_in_bytes = Some(stats.swapins.saturating_mul(page));
                    sample.swap_out_bytes = Some(stats.swapouts.saturating_mul(page));
                    sample.compressor_mib =
                        Some(u64::from(stats.compressor_page_count) * page / 1_048_576);
                }
            }
            let mut swap: libc::xsw_usage = unsafe { std::mem::zeroed() };
            let mut length = std::mem::size_of_val(&swap);
            // SAFETY: swap has exactly length bytes; sysctl only writes that buffer.
            if unsafe {
                libc::sysctlbyname(
                    c"vm.swapusage".as_ptr(),
                    (&raw mut swap).cast(),
                    &raw mut length,
                    std::ptr::null_mut(),
                    0,
                )
            } == 0
                && length == std::mem::size_of_val(&swap)
            {
                sample.swap_used_mib = Some(swap.xsu_used / 1_048_576);
            }
        }
        #[cfg(target_os = "linux")]
        if let Ok(vm) = fs::read_to_string("/proc/vmstat") {
            let counter = |name: &str| {
                vm.lines().find_map(|line| {
                    let (key, value) = line.split_once(' ')?;
                    (key == name)
                        .then(|| value.trim().parse::<u64>().ok())
                        .flatten()
                })
            };
            // SAFETY: sysconf reads a system constant.
            if let Ok(page) = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }) {
                sample.swap_in_bytes = counter("pswpin").map(|value| value.saturating_mul(page));
                sample.swap_out_bytes = counter("pswpout").map(|value| value.saturating_mul(page));
            }
        }
        sample
    }
}

#[cfg(target_os = "macos")]
fn sysctl_u32(name: &std::ffi::CStr) -> Option<u32> {
    let mut value = 0_u32;
    let mut length = std::mem::size_of_val(&value);
    // SAFETY: value has exactly length bytes; name is NUL terminated.
    let result = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && length == std::mem::size_of_val(&value)).then_some(value)
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

/// CPU seconds a process tree has used: every live member's own time plus what
/// its reaped children used, so the total does not drop when a child exits.
/// Persistent Bazel workers are live members, actions and tests reaped ones.
pub fn tree_cpu_seconds(root: &Identity) -> Option<f64> {
    if !alive(root) {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing(),
        );
        let mut pids = std::collections::HashSet::from([Pid::from_u32(root.pid)]);
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
        let mut ticks = 0_u64;
        for pid in pids {
            let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
            let read = unsafe {
                libc::proc_pid_rusage(
                    pid.as_u32() as i32,
                    libc::RUSAGE_INFO_V4,
                    (&mut info as *mut libc::rusage_info_v4).cast(),
                )
            };
            if read != 0 {
                if pid.as_u32() == root.pid {
                    return None;
                }
                continue;
            }
            ticks += info.ri_user_time
                + info.ri_system_time
                + info.ri_child_user_time
                + info.ri_child_system_time;
        }
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
        }
        let mut timebase = Timebase { numer: 0, denom: 0 };
        if unsafe { mach_timebase_info(&mut timebase) } != 0 || timebase.denom == 0 {
            return None;
        }
        Some(ticks as f64 * f64::from(timebase.numer) / f64::from(timebase.denom) / 1e9)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
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
    Ok(peer(stream)?.0)
}

pub fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Result<u32> {
    Ok(peer(stream)?.1)
}

fn peer(stream: &std::os::unix::net::UnixStream) -> Result<(u32, u32)> {
    #[cfg(target_os = "macos")]
    {
        let mut uid = 0;
        let mut gid = 0;
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok((uid, pid as u32))
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
        Ok((credentials.uid, credentials.pid as u32))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        bail!("unsupported platform")
    }
}

pub fn same_image(a: &Path, b: &Path) -> bool {
    same_executable(a, b) || matches!((fs::read(a), fs::read(b)), (Ok(x), Ok(y)) if x == y)
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

#[cfg(all(test, target_os = "macos"))]
mod cpu_tests {
    use super::*;
    struct Reaped(std::process::Child);
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    const WORK: &str = "i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done";

    fn work_cpu_seconds() -> f64 {
        let output = std::process::Command::new("/usr/bin/time")
            .args(["-p", "/bin/sh", "-c", WORK])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| line.strip_prefix("user ").or(line.strip_prefix("sys ")))
            .map(|seconds| seconds.trim().parse::<f64>().unwrap())
            .sum()
    }

    #[test]
    fn a_tree_keeps_the_cpu_its_exited_children_used() {
        let directory = tempfile::tempdir().unwrap();
        let reaped = directory.path().join("reaped");
        let root = Reaped(
            std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    &format!("({WORK}) & wait $!; : > \"$1\"; sleep 30"),
                    "sh",
                ])
                .arg(&reaped)
                .spawn()
                .unwrap(),
        );
        let identity = identity(root.0.id()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !reaped.exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let used = tree_cpu_seconds(&identity).unwrap();
        let expected = work_cpu_seconds();
        assert!(
            (expected * 0.5..expected * 4.0).contains(&used),
            "{used} for {expected}"
        );
    }
}
