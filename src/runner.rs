#![forbid(unsafe_code)]
use crate::{
    bazel,
    config::{Config, Paths},
    coordinator, platform,
    protocol::{self, Event, Message, NativeServer, Request},
};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    fs,
    os::{
        fd::AsRawFd,
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    let mut command = Command::new(std::env::current_exe()?.canonicalize()?);
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
            _=interrupt.recv()=>{},
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

fn progress(event: &Event) {
    if std::env::var("BAZELQUEUE_PROGRESS").as_deref() == Ok("quiet") {
        return;
    }
    match event {
        Event::Queued { position, reason } => {
            eprintln!("bazelqueue: position {position}; waiting: {reason}")
        }
        Event::Grant { .. } => eprintln!("bazelqueue: admitted; starting command"),
        _ => {}
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
    let mut request = Request {
        id: id.clone(),
        owner,
        lane: crate::config::workspace(&cwd)
            .to_string_lossy()
            .into_owned(),
        command: if generic {
            "exec".into()
        } else {
            invocation
                .command
                .clone()
                .unwrap_or_else(|| "unknown".into())
        },
        budget: config.budget(invocation.managed && !generic),
        prepared: generic || invocation.command_index.is_none(),
        workspace: crate::config::workspace(&cwd)
            .to_string_lossy()
            .into_owned(),
    };
    if !config.hooks.is_empty() && !generic {
        request.budget = config.budget(false);
    }
    if generic {
        request.budget = config.budget(false);
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
    let mut stream = registration(&paths, &request).await?;
    let mut server = None;
    let mut hooks_ran = false;
    loop {
        let event = tokio::select! {
            event=protocol::receive::<_,Event>(&mut stream)=>event,
            _=frontend.read(&mut eof)=>{finish(&paths,&id,130,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGINT));},
            _=interrupt.recv()=>{finish(&paths,&id,130,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGINT));},
            _=terminate.recv()=>{finish(&paths,&id,143,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGTERM));},
            _=hangup.recv()=>{finish(&paths,&id,129,true,&mut stream).await?;return Ok(ExitStatus::from_raw(libc::SIGHUP));},
        };
        let event = match event {
            Ok(event) => event,
            Err(_) => {
                eprintln!("bazelqueue: reconnecting to coordinator");
                stream = registration(&paths, &request).await?;
                continue;
            }
        };
        progress(&event);
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
                    finish(&paths, &id, 130, true, &mut stream).await?;
                    return Ok(ExitStatus::from_raw(libc::SIGINT));
                }
                hooks_ran = true;
                if !generic {
                    server = match probe(&paths, &request, &mut stream, &backend, &invocation).await
                    {
                        Ok(Preparation::Ready(server)) => server,
                        Ok(Preparation::Cancelled) => {
                            finish(&paths, &id, 130, true, &mut stream).await?;
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
                request.budget = config.budget(invocation.managed);
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
                break;
            }
            Event::Cancel => {
                finish(&paths, &id, 130, true, &mut stream).await?;
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
        finish(&paths, &id, 130, true, &mut stream).await?;
        return Ok(ExitStatus::from_raw(libc::SIGINT));
    }
    let run_directory = paths.root.join(format!("run-{id}"));
    if invocation.run {
        platform::private_directory(&run_directory)?;
    }
    let script = run_directory.join("execute");
    let backend_args = if generic {
        invocation.args.clone()
    } else {
        bazel::arguments(
            &invocation,
            &request.budget,
            &config,
            invocation.run.then_some(script.as_path()),
        )
    };
    let (mut child, mut permit, child_identity) =
        owned_executor(&backend, backend_args, false, Some(&id)).await?;
    if !authorize(&paths, &request, &mut stream, &child_identity, "running").await? {
        drop(permit);
        child.wait().await?;
        finish(&paths, &id, 130, true, &mut stream).await?;
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
            finish(&paths, &id, 130, true, &mut stream).await?;
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
            &id,
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
        &id,
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

async fn run_hooks(
    config: &Config,
    paths: &Paths,
    request: &Request,
    stream: &mut protocol::Framed<UnixStream>,
    frontend: &mut UnixStream,
    state: &str,
) -> Result<bool> {
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
    id: &str,
    code: i32,
    stopped: bool,
    stream: &mut protocol::Framed<UnixStream>,
) -> Result<()> {
    platform::atomic_write(
        &paths.root.join(format!("receipt-{id}")),
        &serde_json::to_vec(&(code, stopped))?,
    )?;
    let _ = protocol::send(stream, &Message::Finish { code, stopped }).await;
    Ok(())
}
