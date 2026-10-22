#![forbid(unsafe_code)]
use crate::{
    bazel,
    config::{Config, Paths},
    coordinator, platform,
    protocol::{self, Admission, Budget, Event, Message, NativeServer, Request},
    summary,
};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    fs,
    io::IsTerminal,
    os::{
        fd::AsRawFd,
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    signal::unix::{SignalKind, signal},
    time::{sleep, timeout},
};

pub async fn frontend(backend: PathBuf, args: Vec<OsString>, generic: bool) -> Result<ExitStatus> {
    let paths = Paths::discover()?;
    if std::env::var_os("BAZELQUEUE_ACTIVE").is_some() {
        bail!(
            "nested managed invocation while a parent holds a reservation; run it after the parent completes"
        );
    }
    if platform::same_executable(&backend, &std::env::current_exe()?.canonicalize()?) {
        bail!("backend resolves to bazelqueue itself");
    }
    let (parent, control) = std::os::unix::net::UnixStream::pair()?;
    let mut command = Command::new(crate::install::retained_executable(&paths)?);
    command
        .arg("_guardian")
        .arg(&backend)
        .arg(if generic { "generic" } else { "bazel" })
        .arg("--")
        .args(args)
        .env("BAZELQUEUE_HOME", &paths.root);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    platform::inherit_control(command.as_std_mut(), control.as_raw_fd());
    let mut child = command.spawn()?;
    drop(control);
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut continuation = signal(SignalKind::from_raw(libc::SIGCONT))?;
    let result = loop {
        tokio::select! {
            result=child.wait()=>break result?,
            _=terminate.recv()=>if let Some(pid)=child.id() {platform::signal_pid(pid,libc::SIGTERM)},
            _=interrupt.recv()=>if let Some(pid)=child.id() {platform::signal_pid(pid,libc::SIGINT)},
        _=hangup.recv()=>if let Some(pid)=child.id() {platform::signal_pid(pid,libc::SIGHUP)},
        _=continuation.recv()=>if let Some(pid)=child.id() {platform::signal_pid(pid,libc::SIGCONT)},
        }
    };
    drop(parent);
    Ok(result)
}

async fn registration(paths: &Paths, request: &Request) -> Result<protocol::Framed<UnixStream>> {
    let mut stream = coordinator::ensure(paths).await?;
    protocol::send(
        &mut stream,
        &Message::Register {
            request: request.clone(),
        },
    )
    .await?;
    Ok(stream)
}

/// Notices must never abort a guardian whose caller closed its stderr.
macro_rules! report {
    ($($argument:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), $($argument)*);
    }};
}

/// Queue notices for one request. Off a terminal (agents, redirected logs) the
/// caller sees the first queue position and the start, not every change.
struct Notices {
    id: String,
    quiet: bool,
    verbose: bool,
    hint: bool,
    queued: Option<Instant>,
}

/// Agents read this line in their captured output at the moment they would
/// otherwise start polling, whatever repository they work in.
const HINT: &str = "bazelqueue: this call exits when the build ends and then prints a `bazelqueue: finished` summary with each failure and its log; wait for it instead of polling status, pgrep or the log";

impl Notices {
    fn new(id: &str, reported: bool) -> Self {
        let mode = std::env::var("BAZELQUEUE_PROGRESS").unwrap_or_default();
        let verbose = mode == "verbose" || std::io::stderr().is_terminal();
        Self {
            id: id.to_owned(),
            quiet: mode == "quiet",
            verbose,
            hint: reported && !verbose,
            queued: None,
        }
    }
    fn lines(&mut self, event: &Event, now: Instant) -> Vec<String> {
        let line = match event {
            Event::Queued { position, reason } => match self.queued {
                None => {
                    self.queued = Some(now);
                    Some(format!(
                        "bazelqueue: queued {} at position {position} (waiting: {reason})",
                        self.id
                    ))
                }
                Some(_) if self.verbose => Some(format!(
                    "bazelqueue: position {position}; waiting: {reason}"
                )),
                Some(_) => None,
            },
            Event::Grant { .. } => Some(match self.queued {
                Some(since) => format!(
                    "bazelqueue: started {} after {} in queue",
                    self.id,
                    summary::duration(now.duration_since(since).as_millis() as u64)
                ),
                None => format!("bazelqueue: started {}", self.id),
            }),
            _ => None,
        };
        let mut lines: Vec<_> = line.filter(|_| !self.quiet).into_iter().collect();
        if !lines.is_empty() && std::mem::take(&mut self.hint) {
            lines.push(HINT.to_owned());
        }
        lines
    }
    fn show(&mut self, event: &Event) {
        for line in self.lines(event, Instant::now()) {
            report!("{line}");
        }
    }
}

/// What the end-of-run summary needs to know about a request.
struct Ledger {
    id: String,
    command: String,
    workspace: String,
    registered: Instant,
    granted: Option<Instant>,
    events: Option<(PathBuf, bool)>,
    report: bool,
}

impl Ledger {
    fn summary(&self, code: i32, now: Instant) -> summary::Summary {
        let parsed = self.events.as_ref().and_then(|(path, owned)| {
            let text = fs::read_to_string(path).ok();
            if *owned {
                let _ = fs::remove_file(path);
            }
            text.map(|text| summary::parse_events(&text))
        });
        summary::Summary {
            id: self.id.clone(),
            command: self.command.clone(),
            workspace: self.workspace.clone(),
            exit_code: code,
            targets_reported: parsed.is_some(),
            exit_name: parsed.as_ref().and_then(|events| events.exit_name.clone()),
            failures: parsed.map(|events| events.failures).unwrap_or_default(),
            queued_ms: self
                .granted
                .unwrap_or(now)
                .duration_since(self.registered)
                .as_millis() as u64,
            ran_ms: self
                .granted
                .map_or(0, |granted| now.duration_since(granted).as_millis() as u64),
        }
    }
}

async fn owned_executor(
    backend: &PathBuf,
    args: Vec<OsString>,
    capture: bool,
    active: Option<&str>,
) -> Result<(tokio::process::Child, UnixStream, platform::Identity)> {
    let (permit, child_control) = std::os::unix::net::UnixStream::pair()?;
    permit.set_nonblocking(true)?;
    let mut command = Command::new(std::env::current_exe()?.canonicalize()?);
    command
        .arg("_executor")
        .arg(backend)
        .arg("--")
        .args(args)
        .stdin(if capture {
            Stdio::null()
        } else {
            Stdio::inherit()
        })
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if capture {
            Stdio::null()
        } else {
            Stdio::inherit()
        });
    if let Some(active) = active {
        command.env("BAZELQUEUE_ACTIVE", active);
    } else {
        command.env_remove("BAZELQUEUE_ACTIVE");
    }
    command.as_std_mut().process_group(0);
    platform::inherit_control(command.as_std_mut(), child_control.as_raw_fd());
    let child = command.spawn()?;
    drop(child_control);
    let identity = platform::identity(child.id().context("executor has no PID")?)
        .context("executor identity unavailable")?;
    Ok((child, UnixStream::from_std(permit)?, identity))
}
async fn authorize(
    paths: &Paths,
    request: &Request,
    stream: &mut protocol::Framed<UnixStream>,
    child: &platform::Identity,
    state: &str,
) -> Result<bool> {
    let message = match state {
        "preparing" => Message::Preparing {
            child: child.clone(),
        },
        "run_phase" => Message::RunPhase {
            child: child.clone(),
        },
        _ => Message::Running {
            child: child.clone(),
        },
    };
    protocol::send(stream, &message).await?;
    loop {
        match protocol::receive::<_, Event>(stream).await {
            Ok(Event::Owned {
                child: ack,
                state: phase,
            }) if ack == *child && phase == state => return Ok(true),
            Ok(Event::Cancel) => return Ok(false),
            Ok(Event::Error { message }) => bail!("{message}"),
            Ok(_) => {}
            Err(_) => {
                *stream = registration(paths, request).await?;
                protocol::send(stream, &message).await?;
            }
        }
    }
}
enum Preparation {
    Ready(Option<NativeServer>),
    Busy,
    Cancelled,
}
async fn probe(
    paths: &Paths,
    request: &Request,
    stream: &mut protocol::Framed<UnixStream>,
    backend: &PathBuf,
    invocation: &bazel::Invocation,
) -> Result<Preparation> {
    let (mut child, mut permit, identity) = owned_executor(
        backend,
        bazel::probe_arguments(invocation),
        true,
        Some(&request.id),
    )
    .await?;
    if !authorize(paths, request, stream, &identity, "preparing").await? {
        drop(permit);
        child.wait().await?;
        return Ok(Preparation::Cancelled);
    }
    permit.write_all(b"G").await?;
    drop(permit);
    let mut stdout = child.stdout.take().context("probe stdout missing")?;
    let output = async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    };
    let result = timeout(Duration::from_secs(15), output).await;
    if result.is_err() {
        let _ = platform::signal_group(identity.pid, libc::SIGINT);
        if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
            let _ = platform::signal_group(identity.pid, libc::SIGKILL);
            child.wait().await?;
        }
        bail!("native preflight timed out");
    }
    let bytes = result??;
    let status = child.wait().await?;
    if status.code() == Some(9) {
        return Ok(Preparation::Busy);
    }
    if !status.success() {
        bail!("native preflight failed ({status})");
    }
    Ok(Preparation::Ready(bazel::parse_info(&bytes).and_then(
        |(base, pid)| {
            platform::identity(pid).map(|identity| NativeServer {
                identity,
                output_base: base.to_string_lossy().into_owned(),
                workspace: request.workspace.clone(),
                version: String::from_utf8_lossy(&bytes)
                    .lines()
                    .find_map(|line| line.strip_prefix("release: release "))
                    .unwrap_or("")
                    .to_owned(),
            })
        },
    )))
}

pub async fn guardian(backend: PathBuf, args: Vec<OsString>, generic: bool) -> Result<ExitStatus> {
    let paths = Paths::discover()?;
    let config = Config::load(&paths)?;
    let owner = platform::current_identity()?;
    let id = format!(
        "{}-{}",
        owner.pid,
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let _lease = platform::lock(&paths.lease(&id)?)?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let mut invocation = bazel::inspect(args, &cwd, &config);
    let command = if generic {
        "exec".to_owned()
    } else {
        invocation
            .command
            .clone()
            .unwrap_or_else(|| "unknown".into())
    };
    let immediate = protocol::admission(&command) == Admission::Immediate;
    let mut request = Request {
        id: id.clone(),
        owner,
        lane: crate::config::workspace(&cwd)
            .to_string_lossy()
            .into_owned(),
        command,
        budget: initial_budget(&config, &invocation, generic),
        prepared: generic || immediate || invocation.command_index.is_none(),
        workspace: crate::config::workspace(&cwd)
            .to_string_lossy()
            .into_owned(),
    };
    if generic {
        #[cfg(feature = "test-fixtures")]
        if backend
            .file_name()
            .is_some_and(|name| name == "fixture-backend")
        {
            request.budget = config.budget(config.max_builds > 1);
        }
    }
    let mut frontend = UnixStream::from_std(platform::control_socket()?)?;
    let mut eof = [0_u8; 1];
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut child_changes = signal(SignalKind::from_raw(libc::SIGCHLD))?;
    let mut continuation = signal(SignalKind::from_raw(libc::SIGCONT))?;
    let mut ledger = Ledger {
        id: id.clone(),
        command: request.command.clone(),
        workspace: request.workspace.clone(),
        registered: Instant::now(),
        granted: None,
        events: None,
        report: !immediate,
    };
    let mut notices = Notices::new(&id, ledger.report);
    let mut stream = registration(&paths, &request).await?;
    let mut server = None;
    let mut hooks_ran = false;
    loop {
        let event = tokio::select! {
            event=protocol::receive::<_,Event>(&mut stream)=>event,
            _=frontend.read(&mut eof)=>{finish(&paths,&ledger,130,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGINT));},
            _=interrupt.recv()=>{finish(&paths,&ledger,130,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGINT));},
            _=terminate.recv()=>{finish(&paths,&ledger,143,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGTERM));},
            _=hangup.recv()=>{finish(&paths,&ledger,129,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGHUP));},
        };
        let event = match event {
            Ok(event) => event,
            Err(_) => {
                eprintln!("bazelqueue: reconnecting to coordinator");
                stream = registration(&paths, &request).await?;
                continue;
            }
        };
        notices.show(&event);
        match event {
            Event::Prepare => {
                if !run_hooks(
                    &config,
                    &paths,
                    &request,
                    &mut stream,
                    &mut frontend,
                    "preparing",
                )
                .await?
                {
                    finish(&paths, &ledger, 130, true, &mut stream).await?;
                    return Ok(ExitStatus::from_raw(libc::SIGINT));
                }
                hooks_ran = true;
                if !generic {
                    server = match probe(&paths, &request, &mut stream, &backend, &invocation).await
                    {
                        Ok(Preparation::Ready(server)) => server,
                        Ok(Preparation::Cancelled) => {
                            finish(&paths, &ledger, 130, true, &mut stream).await?;
                            return Ok(ExitStatus::from_raw(libc::SIGINT));
                        }
                        Ok(Preparation::Busy) => {
                            protocol::send(&mut stream, &Message::Defer).await?;
                            continue;
                        }
                        Err(_) => None,
                    };
                    if invocation.managed
                        && !server.as_ref().is_some_and(|server| {
                            matches!(server.version.as_str(), "8.4.2" | "9.2.0")
                        })
                    {
                        invocation.managed = false;
                        invocation.run = false;
                        request.budget = config.budget(false);
                    }
                    if let Some(server) = &server {
                        request.lane = server.output_base.clone();
                    }
                }
                request.prepared = true;
                request.budget = config.request_budget(request.admission(), invocation.managed);
                protocol::send(
                    &mut stream,
                    &Message::Prepared {
                        lane: request.lane.clone(),
                        server: server.clone(),
                        budget: request.budget.clone(),
                    },
                )
                .await?;
            }
            Event::Grant { budget } => {
                request.budget = budget;
                ledger.granted = Some(Instant::now());
                break;
            }
            Event::Cancel => {
                finish(&paths, &ledger, 130, true, &mut stream).await?;
                return Ok(ExitStatus::from_raw(libc::SIGINT));
            }
            Event::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
    if !hooks_ran
        && !run_hooks(
            &config,
            &paths,
            &request,
            &mut stream,
            &mut frontend,
            "running",
        )
        .await?
    {
        finish(&paths, &ledger, 130, true, &mut stream).await?;
        return Ok(ExitStatus::from_raw(libc::SIGINT));
    }
    let run_directory = paths.root.join(format!("run-{id}"));
    if invocation.run {
        platform::private_directory(&run_directory)?;
    }
    let script = run_directory.join("execute");
    if !generic {
        ledger.events = match bazel::event_file(
            &invocation,
            &cwd,
            std::env::var_os("HOME")
                .as_deref()
                .map(std::path::Path::new),
        ) {
            bazel::EventFile::Owned => summary::events_location(&paths, &id)
                .ok()
                .map(|path| (path, true)),
            bazel::EventFile::Caller(path) => Some((path, false)),
            bazel::EventFile::Unavailable => None,
        };
    }
    let backend_args = if generic {
        invocation.args.clone()
    } else {
        let arguments = bazel::arguments(
            &invocation,
            &request.budget,
            &config,
            invocation.run.then_some(script.as_path()),
        );
        match &ledger.events {
            Some((path, true)) => bazel::with_event_file(arguments, path),
            _ => arguments,
        }
    };
    let (mut child, mut permit, child_identity) =
        owned_executor(&backend, backend_args, false, Some(&id)).await?;
    if !authorize(&paths, &request, &mut stream, &child_identity, "running").await? {
        drop(permit);
        child.wait().await?;
        finish(&paths, &ledger, 130, true, &mut stream).await?;
        return Ok(ExitStatus::from_raw(libc::SIGINT));
    }
    let terminal = platform::Terminal::handoff(child_identity.pid)?;
    permit.write_all(b"G").await?;
    drop(permit);
    let mut cancelled = false;
    let mut cancellation_signal = libc::SIGINT;
    let mut cancellation_deadline = None;
    let status = loop {
        tokio::select! {
            result=child.wait()=>break result?,
            _=child_changes.recv()=>{if platform::stopped(child_identity.pid)&& let Some(terminal)=&terminal {terminal.suspend_caller();}},
            _=continuation.recv()=>{if let Some(terminal)=&terminal {if terminal.caller_foreground() {terminal.foreground(child_identity.pid);}let _=platform::signal_group(child_identity.pid,libc::SIGCONT);}},
            event=protocol::receive::<_,Event>(&mut stream)=> {
                match event {
                    Ok(Event::Cancel)=>{cancelled=true;let _=platform::signal_group(child_identity.pid,libc::SIGINT);},
                    Err(_)=>{
                        if let Ok(mut new)=registration(&paths,&request).await {
                            let _=protocol::send(&mut new,&Message::Running {child:child_identity.clone()}).await;
                            stream=new;
                        }else {sleep(Duration::from_millis(100)).await;}
                    },
                    _=>{},
                }
            }
            _=frontend.read(&mut eof),if !cancelled=>{cancelled=true;let _=platform::signal_group(child_identity.pid,libc::SIGINT);},
            _=interrupt.recv()=>{cancelled=true;let _=platform::signal_group(child_identity.pid,libc::SIGINT);},
            _=terminate.recv()=>{cancelled=true;cancellation_signal=libc::SIGTERM;let _=platform::signal_group(child_identity.pid,libc::SIGTERM);},
            _=hangup.recv()=>{cancelled=true;cancellation_signal=libc::SIGHUP;let _=platform::signal_group(child_identity.pid,libc::SIGHUP);},
            _=sleep(Duration::from_millis(100)),if cancelled=>{
                let deadline=cancellation_deadline.get_or_insert_with(||tokio::time::Instant::now()+Duration::from_secs(10));
                if tokio::time::Instant::now()>=*deadline {let _=platform::signal_group(child_identity.pid,libc::SIGKILL);}
            }
        }
    };
    drop(terminal);
    let stopped = if generic || (status.signal().is_none() && !cancelled) {
        true
    } else {
        let outcome = async {
            for server in platform::bazel_servers(&request.workspace)? {
                if !crate::native_server::idle(&server).await? {
                    return Ok::<_, anyhow::Error>(false);
                }
            }
            Ok(true)
        };
        timeout(Duration::from_secs(4), outcome)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
    };
    if invocation.run && status.success() && !cancelled {
        let (mut target, mut permit, target_identity) =
            owned_executor(&script, Vec::new(), false, None).await?;
        if !authorize(&paths, &request, &mut stream, &target_identity, "run_phase").await? {
            drop(permit);
            target.wait().await?;
            finish(&paths, &ledger, 130, true, &mut stream).await?;
            return Ok(ExitStatus::from_raw(libc::SIGINT));
        }
        let terminal = platform::Terminal::handoff(target_identity.pid)?;
        permit.write_all(b"G").await?;
        drop(permit);
        let target_status = loop {
            tokio::select! {
                result=target.wait()=>break result?,
            _=child_changes.recv()=>{if platform::stopped(target_identity.pid)&& let Some(terminal)=&terminal {terminal.suspend_caller();}},
            _=continuation.recv()=>{if let Some(terminal)=&terminal {if terminal.caller_foreground() {terminal.foreground(target_identity.pid);}let _=platform::signal_group(target_identity.pid,libc::SIGCONT);}},
                _=frontend.read(&mut eof)=>{let _=platform::signal_group(target_identity.pid,libc::SIGINT);},
                _=terminate.recv()=>{let _=platform::signal_group(target_identity.pid,libc::SIGTERM);},
                _=interrupt.recv()=>{let _=platform::signal_group(target_identity.pid,libc::SIGINT);},
                event=protocol::receive::<_,Event>(&mut stream)=>match event {
                Ok(Event::Cancel)=>{let _=platform::signal_group(target_identity.pid,libc::SIGINT);},
                Err(_)=>{if let Ok(mut new)=registration(&paths,&request).await {let _=protocol::send(&mut new,&Message::RunPhase {child:target_identity.clone()}).await;stream=new;}else{sleep(Duration::from_millis(100)).await;}},
                _=>{},
            },
            }
        };
        drop(terminal);
        let _ = fs::remove_dir_all(run_directory);
        finish(
            &paths,
            &ledger,
            target_status
                .code()
                .unwrap_or(128 + target_status.signal().unwrap_or(1)),
            stopped,
            &mut stream,
        )
        .await?;
        return Ok(target_status);
    }
    finish(
        &paths,
        &ledger,
        status.code().unwrap_or(128 + status.signal().unwrap_or(1)),
        stopped,
        &mut stream,
    )
    .await?;
    let _ = fs::remove_dir_all(run_directory);
    if cancelled {
        Ok(ExitStatus::from_raw(cancellation_signal))
    } else {
        Ok(status)
    }
}

/// Blocks until a request ends and returns its summary. Without an id it follows
/// the newest build of the caller's workspace, preferring one still in progress.
pub async fn wait(
    paths: &Paths,
    id: Option<String>,
    cwd: &std::path::Path,
) -> Result<summary::Summary> {
    let mut target = id;
    loop {
        let Event::Snapshot { snapshot } = coordinator::control(paths, "status", None).await?
        else {
            bail!("coordinator returned no snapshot");
        };
        let id = match &target {
            Some(id) => id.clone(),
            None => {
                let workspace = crate::config::workspace(&cwd.canonicalize()?)
                    .to_string_lossy()
                    .into_owned();
                let mine: Vec<_> = snapshot
                    .jobs
                    .iter()
                    .filter(|job| {
                        job.request.workspace == workspace
                            && job.request.admission() != Admission::Immediate
                    })
                    .collect();
                let job = mine
                    .iter()
                    .filter(|job| !job.terminal())
                    .max_by_key(|job| job.sequence)
                    .or_else(|| mine.iter().max_by_key(|job| job.sequence))
                    .with_context(|| format!("no request from {workspace} in the queue"))?;
                target = Some(job.request.id.clone());
                job.request.id.clone()
            }
        };
        let job = snapshot.jobs.iter().find(|job| job.request.id == id);
        if job.is_none_or(|job| job.terminal()) {
            if let Some(summary) = summary::load(paths, &id)? {
                return Ok(summary);
            }
            let job = job.with_context(|| format!("unknown request {id}"))?;
            return Ok(summary::Summary {
                id,
                command: job.request.command.clone(),
                workspace: job.request.workspace.clone(),
                exit_code: job.code.unwrap_or(130),
                exit_name: (job.state == "cancelled").then(|| "CANCELLED".into()),
                queued_ms: 0,
                ran_ms: 0,
                targets_reported: false,
                failures: Vec::new(),
            });
        }
        sleep(Duration::from_secs(1)).await;
    }
}

/// Hooks never widen a reservation: they run inside the request's own budget.
pub fn initial_budget(config: &Config, invocation: &bazel::Invocation, generic: bool) -> Budget {
    if generic {
        return config.budget(false);
    }
    config.request_budget(
        protocol::admission(invocation.command.as_deref().unwrap_or("unknown")),
        invocation.managed,
    )
}

/// Hooks are not written to run concurrently with each other (a bazelrc mirror
/// sweeps its peers' staging files), so they take a host-wide hook lock. Only
/// the hooks wait for it; the reservation stays the request's own.
async fn hook_lock(
    paths: &Paths,
    request: &Request,
    stream: &mut protocol::Framed<UnixStream>,
    frontend: &mut UnixStream,
) -> Result<Option<fs::File>> {
    let path = paths.root.join("hooks.lock");
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut eof = [0];
    let mut announced = false;
    loop {
        match platform::lock(&path) {
            Ok(file) => return Ok(Some(file)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) => {}
            Err(error) => return Err(error),
        }
        if !announced {
            announced = true;
            if std::env::var("BAZELQUEUE_PROGRESS").as_deref() != Ok("quiet") {
                eprintln!("bazelqueue: waiting for another invocation's preflight hooks");
            }
        }
        tokio::select! {
            _=sleep(Duration::from_millis(25))=>{},
            _=frontend.read(&mut eof)=>return Ok(None),
            _=interrupt.recv()=>return Ok(None),
            _=terminate.recv()=>return Ok(None),
            _=hangup.recv()=>return Ok(None),
            event=protocol::receive::<_,Event>(stream)=>match event {
                Ok(Event::Cancel)=>return Ok(None),
                Err(_)=>*stream=registration(paths, request).await?,
                _=>{},
            },
        }
    }
}

async fn run_hooks(
    config: &Config,
    paths: &Paths,
    request: &Request,
    stream: &mut protocol::Framed<UnixStream>,
    frontend: &mut UnixStream,
    state: &str,
) -> Result<bool> {
    if config.hooks.is_empty() {
        return Ok(true);
    }
    let Some(_serial) = hook_lock(paths, request, stream, frontend).await? else {
        return Ok(false);
    };
    for hook in &config.hooks {
        let args = hook.arguments.iter().map(OsString::from).collect();
        let (mut child, mut permit, identity) =
            owned_executor(&hook.program, args, true, Some(&request.id)).await?;
        if !authorize(paths, request, stream, &identity, state).await? {
            drop(permit);
            child.wait().await?;
            return Ok(false);
        }
        permit.write_all(b"G").await?;
        drop(permit);
        let mut stdout = child.stdout.take().context("hook stdout missing")?;
        let output =
            tokio::spawn(async move { tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await });
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        let mut eof = [0];
        let mut cancelled = false;
        let mut deadline = None;
        let status = loop {
            tokio::select! {
                result=child.wait()=>break result?,
                _=frontend.read(&mut eof), if !cancelled=>cancelled=true,
                _=interrupt.recv()=>cancelled=true,
                _=terminate.recv()=>cancelled=true,
                _=hangup.recv()=>cancelled=true,
                event=protocol::receive::<_,Event>(stream)=>match event {
                    Ok(Event::Cancel)=>cancelled=true,
                    Err(_)=>{*stream=registration(paths, request).await?; let message=if state=="preparing" {Message::Preparing {child:identity.clone()}} else {Message::Running {child:identity.clone()}}; protocol::send(stream,&message).await?;},
                    _=>{},
                },
                _=sleep(Duration::from_millis(100)), if cancelled=>{},
            }
            if cancelled {
                let until = deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(10));
                let signal = if tokio::time::Instant::now() >= *until {
                    libc::SIGKILL
                } else {
                    libc::SIGINT
                };
                let _ = platform::signal_group(identity.pid, signal);
            }
        };
        output.abort();
        if cancelled {
            let _ = platform::signal_group(identity.pid, libc::SIGKILL);
            return Ok(false);
        }
        if !status.success() {
            eprintln!(
                "bazelqueue: preflight hook failed: {}",
                hook.program.display()
            );
        }
    }
    Ok(true)
}

async fn finish(
    paths: &Paths,
    ledger: &Ledger,
    code: i32,
    stopped: bool,
    stream: &mut protocol::Framed<UnixStream>,
) -> Result<()> {
    let summary = ledger.summary(code, Instant::now());
    let location = summary::save(paths, &summary)
        .inspect_err(|error| report!("bazelqueue: summary not saved: {error:#}"))
        .ok();
    if let Some(destination) = std::env::var_os("BAZELQUEUE_SUMMARY")
        && let Err(error) = summary::copy_to(std::path::Path::new(&destination), &summary)
    {
        report!("bazelqueue: summary copy not written: {error:#}");
    }
    if ledger.report {
        for line in summary.lines(location.as_deref()) {
            report!("{line}");
        }
    }
    platform::atomic_write(
        &paths.root.join(format!("receipt-{}", ledger.id)),
        &serde_json::to_vec(&(code, stopped))?,
    )?;
    let _ = protocol::send(stream, &Message::Finish { code, stopped }).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Hook;
    #[test]
    fn off_a_terminal_a_queued_request_prints_its_first_position_and_its_start_only() {
        let queued = |position| Event::Queued {
            position,
            reason: "resource capacity".into(),
        };
        let grant = Event::Grant {
            budget: Config::default().budget(true),
        };
        let start = Instant::now();
        let mut agent = Notices {
            id: "7-1".into(),
            quiet: false,
            verbose: false,
            hint: true,
            queued: None,
        };
        let mut printed = Vec::new();
        for (offset, event) in [queued(3), queued(2), queued(1), grant.clone()]
            .iter()
            .enumerate()
        {
            printed.extend(agent.lines(event, start + Duration::from_secs(64 * offset as u64)));
        }
        assert_eq!(
            printed,
            [
                "bazelqueue: queued 7-1 at position 3 (waiting: resource capacity)",
                HINT,
                "bazelqueue: started 7-1 after 3m12s in queue",
            ]
        );
        let mut terminal = Notices {
            verbose: true,
            queued: None,
            ..agent
        };
        assert_eq!(terminal.lines(&queued(2), start).len(), 1);
        assert_eq!(
            terminal.lines(&queued(1), start),
            ["bazelqueue: position 1; waiting: resource capacity"]
        );
        let mut quiet = Notices {
            quiet: true,
            queued: None,
            ..terminal
        };
        assert!(quiet.lines(&queued(1), start).is_empty());
        assert!(quiet.lines(&grant, start).is_empty());
        let mut immediate = Notices {
            quiet: false,
            queued: None,
            ..quiet
        };
        assert_eq!(immediate.lines(&grant, start), ["bazelqueue: started 7-1"]);
        let mut unqueued_agent = Notices {
            verbose: false,
            hint: true,
            queued: None,
            ..immediate
        };
        assert_eq!(
            unqueued_agent.lines(&grant, start),
            ["bazelqueue: started 7-1", HINT]
        );
    }
    #[test]
    fn hooks_do_not_turn_requests_into_whole_machine_reservations() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".bazelversion"), "8.4.2").unwrap();
        let config = Config {
            cpu_capacity: 8,
            memory_capacity_mib: 8192,
            action_memory_mib: 2048,
            max_builds: 2,
            hooks: vec![Hook {
                program: "/usr/bin/true".into(),
                arguments: Vec::new(),
            }],
            ..Config::default()
        };
        for (args, cpu) in [
            (vec!["test", "//:x"], 4),
            (vec!["info", "execution_root"], 1),
            (vec!["clean", "--expunge"], 1),
        ] {
            let invocation = bazel::inspect(
                args.into_iter().map(OsString::from).collect(),
                root.path(),
                &config,
            );
            let budget = initial_budget(&config, &invocation, false);
            assert!(!budget.exclusive, "{:?}", invocation.command);
            assert_eq!(budget.cpu, cpu, "{:?}", invocation.command);
        }
    }
}
