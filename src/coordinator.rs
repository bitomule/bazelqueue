#![forbid(unsafe_code)]
use crate::{
    config::{Config, Paths},
    platform,
    protocol::{self, Event, Message, Snapshot},
    resources::{Monitor, Sample},
    scheduler::{self, Capacity, Pressure},
    store::Store,
    utilization::{Observation, Utilization},
};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::mpsc,
    time::{interval, sleep},
};

enum Input {
    Message {
        connection: u64,
        message: Message,
        reply: mpsc::Sender<Event>,
    },
    Closed {
        connection: u64,
    },
    Recovered {
        id: String,
        idle: bool,
    },
}

fn sample_resources(
    paths: &Paths,
    config: &Config,
    sampler: &mut platform::ResourceSampler,
    at_ms: u64,
) -> Sample {
    #[cfg(feature = "test-fixtures")]
    {
        if let Ok(value) = fs::read_to_string(paths.root.join("resources.json")) {
            let mut sample: Sample = serde_json::from_str(&value).unwrap_or_default();
            sample.at_ms = serde_json::from_str::<serde_json::Value>(&value)
                .ok()
                .and_then(|value| value.get("at_ms").and_then(serde_json::Value::as_u64))
                .unwrap_or(at_ms);
            return sample;
        }
        if let Ok(value) = fs::read_to_string(paths.root.join("pressure")) {
            let memory = config.memory_capacity_mib
                + config
                    .memory_safety_mib
                    .max(config.memory_capacity_mib / 10);
            return Sample {
                at_ms,
                pressure: value.trim().parse().ok(),
                total_mib: memory,
                available_mib: Some(memory),
                swap_in_bytes: Some(0),
                swap_out_bytes: Some(0),
                ..Sample::default()
            };
        }
    }
    let _ = (paths, config);
    sampler.sample(at_ms)
}

async fn connection(stream: UnixStream, number: u64, send: mpsc::Sender<Input>) -> Result<()> {
    let standard = stream.into_std()?;
    if platform::peer_uid(&standard)? != platform::uid() {
        bail!("foreign socket peer");
    }
    let stream = UnixStream::from_std(standard)?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = protocol::Framed::new(reader);
    let hello: Message = protocol::receive(&mut reader).await?;
    if !matches!(
        hello,
        Message::Hello {
            protocol: protocol::PROTOCOL
        }
    ) {
        bail!("incompatible control protocol");
    }
    protocol::send(
        &mut writer,
        &Event::Hello {
            protocol: protocol::PROTOCOL,
        },
    )
    .await?;
    let (reply, mut events) = mpsc::channel(32);
    loop {
        tokio::select! {
            value=protocol::receive::<_,Message>(&mut reader)=> {
                send.send(Input::Message {connection:number,message:value?,reply:reply.clone()}).await?;
            }
            value=events.recv()=> {
                match value {Some(event)=>protocol::send(&mut writer,&event).await?,None=>break}
            }
        }
    }
    Ok(())
}

pub async fn connect(paths: &Paths) -> Result<protocol::Framed<UnixStream>> {
    Ok(connect_peer(paths).await?.0)
}

async fn connect_peer(paths: &Paths) -> Result<(protocol::Framed<UnixStream>, u32)> {
    let stream = UnixStream::connect(&paths.socket).await?;
    let standard = stream.into_std()?;
    if platform::peer_uid(&standard)? != platform::uid() {
        bail!("coordinator belongs to another user");
    }
    let peer = platform::peer_pid(&standard)?;
    let mut stream = protocol::Framed::new(UnixStream::from_std(standard)?);
    protocol::send(
        &mut stream,
        &Message::Hello {
            protocol: protocol::PROTOCOL,
        },
    )
    .await?;
    if !matches!(
        protocol::receive::<_, Event>(&mut stream).await?,
        Event::Hello {
            protocol: protocol::PROTOCOL
        }
    ) {
        bail!("incompatible coordinator protocol");
    }
    Ok((stream, peer))
}

pub async fn ensure(paths: &Paths) -> Result<protocol::Framed<UnixStream>> {
    Ok(ensure_from(paths, &launcher(paths)?).await?.0)
}

/// Guardians outlive upgrades, so they start the installed coordinator rather
/// than their own retained image.
fn launcher(paths: &Paths) -> Result<PathBuf> {
    #[cfg(feature = "test-fixtures")]
    if std::env::var_os("BAZELQUEUE_FIXTURE_COORDINATOR").is_some() {
        return Ok(std::env::current_exe()?.canonicalize()?);
    }
    match paths.root.join("current").canonicalize() {
        Ok(installed) => Ok(installed),
        Err(_) => Ok(std::env::current_exe()?.canonicalize()?),
    }
}

async fn ensure_from(paths: &Paths, program: &Path) -> Result<(protocol::Framed<UnixStream>, u32)> {
    if let Ok(connection) = connect_peer(paths).await {
        return Ok(connection);
    }
    let log = platform::private_file(&paths.root.join("daemon.log"))?;
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(program);
    command.process_group(0);
    command
        .args(["daemon", "run"])
        .env("BAZELQUEUE_HOME", &paths.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    let mut child = command.spawn()?;
    for _ in 0..100 {
        if let Ok(connection) = connect_peer(paths).await {
            return Ok(connection);
        }
        if let Some(status) = child.try_wait()?
            && !status.success()
        {
            bail!(
                "coordinator failed to start ({status}); inspect {}",
                paths.root.join("daemon.log").display()
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
    bail!("coordinator did not become ready; no command has been started")
}

pub async fn control(paths: &Paths, operation: &str, id: Option<String>) -> Result<Event> {
    let mut stream = ensure(paths).await?;
    protocol::send(
        &mut stream,
        &Message::Control {
            operation: operation.into(),
            id,
        },
    )
    .await?;
    let event = protocol::receive(&mut stream).await?;
    if let Event::Error { message } = event {
        bail!("{message}");
    }
    Ok(event)
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn runs(snapshot: &Snapshot, executable: &Path) -> bool {
    snapshot.version == VERSION
        && snapshot
            .executable
            .as_deref()
            .is_some_and(|running| platform::same_image(running, executable))
}

pub fn describe_version(snapshot: &Snapshot) -> &str {
    if snapshot.version.is_empty() {
        "unreported (older)"
    } else {
        &snapshot.version
    }
}

pub fn skew(snapshot: &Snapshot) -> Option<String> {
    (snapshot.version != VERSION).then(|| {
        format!(
            "coordinator {} runs bazelqueue {}, but this bazelqueue is {VERSION}; run `bazelqueue setup` to finish the upgrade",
            snapshot.daemon.pid,
            describe_version(snapshot)
        )
    })
}

pub struct Replaced {
    pub previous: Snapshot,
    pub current: Snapshot,
}

/// What released coordinators answer to `retire` while requests execute.
pub const BUSY_REFUSAL: &str =
    "requests are executing; the coordinator can be replaced once they finish";
const SUCCESSOR: &str = "BAZELQUEUE_SUCCESSOR";

/// Replaces a running coordinator that is not `executable` at this version.
/// A successor started from `executable` already waits on the coordinator lock
/// when the old one exits, so guardians that relaunch their own older image
/// find the lock taken and reconnect to the successor, which recovers queued
/// and executing requests from the durable store.
pub async fn hand_over(paths: &Paths, executable: &Path) -> Result<Option<Replaced>> {
    let mut previous = None;
    for _ in 0..3 {
        let (mut stream, peer) = ensure_from(paths, executable).await?;
        let snapshot = request_status(&mut stream).await?;
        if runs(&snapshot, executable) {
            return Ok(previous.map(|previous| Replaced {
                previous,
                current: snapshot,
            }));
        }
        let mut successor = successor(paths, executable).await?;
        let outcome = match retire(&mut stream, peer, &snapshot).await {
            Ok(interrupt) => {
                drop(stream);
                retired(paths, &snapshot, interrupt, &mut successor).await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            let _ = successor.start_kill();
            return Err(error);
        }
        previous.get_or_insert(snapshot);
    }
    bail!("an older coordinator kept replacing the installed one")
}

async fn successor(paths: &Paths, program: &Path) -> Result<tokio::process::Child> {
    use std::os::unix::process::CommandExt;
    use tokio::io::AsyncBufReadExt;
    let log = platform::private_file(&paths.root.join("daemon.log"))?;
    let mut command = tokio::process::Command::new(program);
    command.as_std_mut().process_group(0);
    command
        .args(["daemon", "run"])
        .env("BAZELQUEUE_HOME", &paths.root)
        .env(SUCCESSOR, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("successor stdout missing"))?;
    let mut line = String::new();
    let announced = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::io::BufReader::new(stdout).read_line(&mut line),
    )
    .await;
    if !matches!(announced, Ok(Ok(_))) || line.trim() != "waiting" {
        let _ = child.start_kill();
        bail!(
            "the installed coordinator did not start; inspect {}",
            paths.root.join("daemon.log").display()
        );
    }
    Ok(child)
}

/// Returns whether the coordinator has to be interrupted instead.
async fn retire(
    stream: &mut protocol::Framed<UnixStream>,
    peer: u32,
    snapshot: &Snapshot,
) -> Result<bool> {
    protocol::send(
        stream,
        &Message::Control {
            operation: "retire".into(),
            id: None,
        },
    )
    .await?;
    let interrupt = match protocol::receive::<_, Event>(stream).await {
        Ok(Event::Error { message })
            if message == "unknown control operation" || message == BUSY_REFUSAL =>
        {
            true
        }
        Ok(Event::Error { message }) => bail!("{message}"),
        Ok(_) | Err(_) => false,
    };
    if interrupt && (peer != snapshot.daemon.pid || !platform::alive(&snapshot.daemon)) {
        bail!(
            "cannot verify that process {} is the coordinator; refusing to signal it",
            snapshot.daemon.pid
        );
    }
    Ok(interrupt)
}

async fn request_status(stream: &mut protocol::Framed<UnixStream>) -> Result<Snapshot> {
    protocol::send(
        stream,
        &Message::Control {
            operation: "status".into(),
            id: None,
        },
    )
    .await?;
    match protocol::receive(stream).await? {
        Event::Snapshot { snapshot } => Ok(snapshot),
        Event::Error { message } => bail!("{message}"),
        _ => bail!("coordinator returned no snapshot"),
    }
}

async fn retired(
    paths: &Paths,
    snapshot: &Snapshot,
    interrupt: bool,
    successor: &mut tokio::process::Child,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut signal_at = tokio::time::Instant::now();
    loop {
        if let Ok(Ok((_, peer))) =
            tokio::time::timeout(Duration::from_millis(500), connect_peer(paths)).await
            && peer != snapshot.daemon.pid
        {
            return Ok(());
        }
        if successor.try_wait()?.is_some() {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!(
                "coordinator {} (bazelqueue {}) did not exit",
                snapshot.daemon.pid,
                describe_version(snapshot)
            );
        }
        if interrupt && now >= signal_at {
            platform::signal_pid(snapshot.daemon.pid, libc::SIGINT);
            signal_at = now + Duration::from_millis(250);
        }
        sleep(Duration::from_millis(20)).await;
    }
}

struct Daemon {
    identity: platform::Identity,
    version: String,
    executable: Option<PathBuf>,
    #[cfg(feature = "test-fixtures")]
    legacy: bool,
    #[cfg(feature = "test-fixtures")]
    released: bool,
}

impl Daemon {
    fn current() -> Result<Self> {
        #[allow(unused_mut)]
        let mut daemon = Self {
            identity: platform::current_identity()?,
            version: VERSION.into(),
            executable: std::env::current_exe()
                .and_then(|path| path.canonicalize())
                .ok(),
            #[cfg(feature = "test-fixtures")]
            legacy: false,
            #[cfg(feature = "test-fixtures")]
            released: false,
        };
        #[cfg(feature = "test-fixtures")]
        match std::env::var("BAZELQUEUE_FIXTURE_COORDINATOR").as_deref() {
            Ok("legacy") => {
                daemon.version.clear();
                daemon.executable = None;
                daemon.legacy = true;
            }
            Ok(version) => {
                daemon.version = version.into();
                daemon.released = true;
            }
            Err(_) => {}
        }
        Ok(daemon)
    }
    fn retires(&self) -> bool {
        #[cfg(feature = "test-fixtures")]
        if self.legacy {
            return false;
        }
        true
    }
    fn refuses_busy_retire(&self) -> bool {
        #[cfg(feature = "test-fixtures")]
        if self.released {
            return true;
        }
        false
    }
    fn reload(&self, paths: &Paths) -> Result<Config> {
        #[cfg(feature = "test-fixtures")]
        if self.legacy
            && fs::read_to_string(&paths.config)
                .is_ok_and(|text| text.contains("constrained_divisor"))
        {
            bail!(
                "invalid bazelqueue configuration: TOML parse error: unknown field `constrained_divisor`"
            );
        }
        Config::load(paths)
    }
}

/// A runner writes its receipt before it reports `Finish`, so the receipt
/// outlives that message whenever the message arrives first. Receipts are
/// listed before the jobs are read: a receipt only exists for a registered job.
fn settle_receipts(paths: &Paths, store: &Store) -> Result<()> {
    let Ok(entries) = fs::read_dir(&paths.root) else {
        return Ok(());
    };
    let receipts: Vec<(String, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let id = entry
                .file_name()
                .to_str()?
                .strip_prefix("receipt-")?
                .to_owned();
            Some((id, entry.path()))
        })
        .collect();
    if receipts.is_empty() {
        return Ok(());
    }
    let mut jobs = store.jobs()?;
    for (id, receipt) in receipts {
        let outcome = fs::read(&receipt)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<(i32, bool)>(&bytes).ok());
        match jobs.iter_mut().find(|job| job.request.id == id) {
            Some(job) if !job.terminal() => {
                let Some((code, stopped)) = outcome else {
                    continue;
                };
                job.code = Some(code);
                job.state = if stopped { "finished" } else { "quarantined" }.into();
                store.save(job)?;
            }
            Some(job) => {
                if job.code.is_none()
                    && let Some((code, _)) = outcome
                {
                    job.code = Some(code);
                    store.save(job)?;
                }
            }
            None => {}
        }
        let _ = fs::remove_file(receipt);
    }
    Ok(())
}

fn notify(reply: &mpsc::Sender<Event>, event: Event) {
    let _ = reply.try_send(event);
}

fn announce() -> Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    writeln!(stdout, "waiting")?;
    stdout.flush()?;
    Ok(())
}

/// Waits for the coordinator being retired to release the lock. The successor
/// is already blocked in the kernel when that happens, so it takes the lock
/// before any process started afterwards can.
async fn succeed(paths: &Paths) -> Result<fs::File> {
    let path = paths.root.join("daemon.lock");
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(platform::wait_lock(&path));
    });
    announce()?;
    match tokio::time::timeout(Duration::from_secs(30), receiver).await {
        Ok(Ok(lock)) => lock,
        _ => bail!("the coordinator being replaced did not exit"),
    }
}

pub async fn run(paths: Paths) -> Result<()> {
    let successor = std::env::var_os(SUCCESSOR).is_some();
    let _lock = match platform::lock(&paths.root.join("daemon.lock")) {
        Ok(lock) if successor => {
            announce()?;
            lock
        }
        Ok(lock) => lock,
        Err(_) if successor => succeed(&paths).await?,
        Err(_) => return Ok(()),
    };
    let store = Store::open(&paths)?;
    let mut config = Config::load(&paths)?;
    if fs::symlink_metadata(&paths.socket).is_ok() {
        fs::remove_file(&paths.socket)?;
    }
    let listener = UnixListener::bind(&paths.socket)?;
    let (tx, mut rx) = mpsc::channel(128);
    let mut timer = interval(Duration::from_millis(250));
    let mut sessions: HashMap<String, (u64, mpsc::Sender<Event>)> = HashMap::new();
    let mut connection_ids: HashMap<u64, String> = HashMap::new();
    let mut notices: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut count = 0_u64;
    let daemon = Daemon::current()?;
    let started = Instant::now();
    let mut sampler = platform::ResourceSampler::default();
    let mut monitor = Monitor::default();
    monitor.update(sample_resources(&paths, &config, &mut sampler, 0), &config);
    let mut usage = store.utilization()?;
    let mut last_flush = Instant::now();
    let mut resident = HashMap::<String, u64>::new();
    let mut tick = 0;
    let mut probing = std::collections::HashSet::new();
    let mut deferred: HashMap<String, tokio::time::Instant> = HashMap::new();
    loop {
        let mut reschedule = false;
        tokio::select! {
            incoming=listener.accept()=> {
                let (stream,_)=incoming?;count+=1;let number=count;let sender=tx.clone();
                tokio::spawn(async move {let _=connection(stream,number,sender.clone()).await;let _=sender.send(Input::Closed {connection:number}).await;});
            }
            input=rx.recv()=> {
                reschedule = !matches!(&input,Some(Input::Message {message:Message::Control {operation,..},..}) if operation=="status");
                match input {
                    Some(Input::Closed {connection})=> {
                        reschedule=connection_ids.contains_key(&connection);
                        if let Some(id)=connection_ids.remove(&connection)
                            && sessions.get(&id).is_some_and(|(current,_)|*current==connection) {sessions.remove(&id);notices.remove(&id);}
                    }
                    Some(Input::Recovered {id,idle})=>{
                        probing.remove(&id);
                        if idle&& let Some(mut job)=store.jobs()?.into_iter().find(|job|job.request.id==id && job.state=="quarantined" && !platform::alive(&job.request.owner)) {job.state="cancelled".into();store.save(&job)?;}
                    }
                    Some(Input::Message {connection,message:Message::Defer,reply})=> {
                        if let Some(id)=connection_ids.get(&connection).filter(|id|sessions.get(*id).is_some_and(|(current,_)|*current==connection))&& let Some(mut job)=store.jobs()?.into_iter().find(|job|&job.request.id==id && job.state=="preparing") {job.state="queued".into();store.save(&job)?;deferred.insert(id.clone(),tokio::time::Instant::now()+Duration::from_secs(1));notify(&reply,Event::Queued {position:store.jobs()?.iter().filter(|other|other.state=="queued" && other.sequence<=job.sequence).count(),reason:"native server is busy".into()});}
                    }
                    Some(Input::Message {connection,message,reply})=> {
                        if matches!(&message, Message::Control {operation,..} if operation=="retire") && daemon.retires() {
                            if daemon.refuses_busy_retire() && store.jobs()?.iter().any(|job|!job.terminal() && job.state!="queued") {
                                notify(&reply,Event::Error {message:BUSY_REFUSAL.into()});
                                continue;
                            }
                            notify(&reply,Event::Ack);
                            break;
                        }
                        if matches!(&message, Message::Control {operation,..} if operation=="resume") {
                            match daemon.reload(&paths) {
                                Ok(updated)=>config=updated,
                                Err(error)=>{notify(&reply,Event::Error {message:error.to_string()});continue;},
                            }
                        }
                        let result=handle(&paths,&store,&config,&daemon,&monitor,&usage,connection,message,&reply,&mut sessions,&mut connection_ids);
                        if let Err(error)=result {notify(&reply,Event::Error {message:error.to_string()});}
                    }
                    None=>break,
                }
            }
            _=timer.tick()=> {
                reschedule=true;
                tick+=1;
                if tick%4==0 {
                    if let Ok(updated)=Config::load(&paths) {config=updated;}
                    let sample=sample_resources(&paths, &config, &mut sampler, started.elapsed().as_millis() as u64);
                    let sampled_jobs=store.jobs()?;
                    crate::resources::refresh_resident(&sample,&mut resident,||sampled_jobs.iter().filter(|job|job.holds_build_capacity()).map(|job| {
                        let owners:Vec<_>=job.child.iter().cloned().chain(job.server.iter().map(|server|server.identity.clone())).collect();
                        (job.request.id.clone(),platform::process_memory(&owners))
                    }).collect());
                    monitor.update(sample, &config);
                }

            }
            _=tokio::signal::ctrl_c()=>break,
        }
        if reschedule {
            let mode = monitor.mode();
            settle_receipts(&paths, &store)?;
            let mut jobs = store.jobs()?;
            let previous_budgets: HashMap<_, _> = jobs
                .iter()
                .map(|job| (job.sequence, job.request.budget.clone()))
                .collect();
            for job in &mut jobs {
                if job.terminal() {
                    continue;
                }
                if job.state == "queued"
                    && job.metered()
                    && (job.request.budget.cpu > config.cpu_capacity
                        || job.request.budget.memory_mib > config.memory_capacity_mib)
                {
                    job.request.budget = config.budget(!job.request.budget.exclusive);
                }
                if job.state == "queued" && job.metered() {
                    scheduler::fit_queued(&mut job.request.budget, &config, mode);
                }
                if !platform::alive(&job.request.owner) {
                    if job.state == "queued" {
                        job.state = "cancelled".into();
                    } else if job.state == "run_phase" {
                        if let Some(child) =
                            job.child.as_ref().filter(|child| platform::alive(child))
                        {
                            let _ = platform::signal_group(child.pid, libc::SIGINT);
                        } else {
                            job.state = "cancelled".into();
                        }
                    } else if job
                        .server
                        .as_ref()
                        .is_some_and(|server| platform::alive(&server.identity))
                        || job.child.as_ref().is_some_and(platform::alive)
                    {
                        job.state = "quarantined".into();
                    } else if ((job.state == "starting" || job.state == "preparing")
                        && job.child.is_none())
                        || job.request.command == "exec"
                    {
                        job.state = "cancelled".into();
                    } else {
                        job.state = "quarantined".into();
                    }
                    store.save(job)?;
                }
                if tick % 4 == 0
                    && job.state == "quarantined"
                    && !platform::alive(&job.request.owner)
                    && job
                        .child
                        .as_ref()
                        .is_none_or(|child| !platform::alive(child))
                    && !probing.contains(&job.request.id)
                {
                    let id = job.request.id.clone();
                    let workspace = job.request.workspace.clone();
                    probing.insert(id.clone());
                    let sender = tx.clone();
                    tokio::spawn(async move {
                        let outcome = async {
                            let servers = platform::bazel_servers(&workspace)?;
                            for server in servers {
                                if !crate::native_server::idle(&server).await? {
                                    return Ok::<_, anyhow::Error>(false);
                                }
                            }
                            Ok(true)
                        };
                        let idle = tokio::time::timeout(Duration::from_secs(4), outcome)
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or(false);
                        let _ = sender.send(Input::Recovered { id, idle }).await;
                    });
                }
            }
            let run_owners: Vec<_> = jobs
                .iter()
                .filter(|job| job.state == "run_phase")
                .filter_map(|job| job.child.clone())
                .collect();
            let capacity = Capacity {
                pressure: mode,
                legacy_active: jobs.iter().any(|job| job.state == "queued")
                    && legacy_owners(&store, &config).map_or(true, |owners| !owners.is_empty()),
                drained: store.drained()?,
                run_memory_mib: platform::process_memory(&run_owners),
                new_memory_mib: monitor
                    .headroom(&config)
                    .saturating_sub(crate::resources::promised_growth(&jobs, &resident)),
                reason: monitor.signals.reason.clone(),
            };
            let limit = scheduler::admission_budget(&config, mode);
            let remaining_memory = limit.memory_mib.saturating_sub(capacity.run_memory_mib);
            if remaining_memory >= 256 && !jobs.iter().any(|job| job.holds_build_capacity()) {
                for job in jobs.iter_mut().filter(|job| {
                    job.state == "queued"
                        && job.metered()
                        && (!job.request.prepared
                            || (!job.request.budget.exclusive && job.request.command != "exec"))
                }) {
                    job.request.budget.memory_mib =
                        job.request.budget.memory_mib.min(remaining_memory);
                }
            }
            let candidates: Vec<_> = jobs
                .iter()
                .filter(|job| {
                    job.state != "queued"
                        || (sessions.contains_key(&job.request.id)
                            && deferred
                                .get(&job.request.id)
                                .is_none_or(|time| tokio::time::Instant::now() >= *time))
                })
                .cloned()
                .collect();
            let mut profiled = candidates;
            scheduler::allocate_queued(&mut profiled, &config, &capacity);
            for updated in profiled.iter().filter(|job| job.state == "queued") {
                if let Some(job) = jobs.iter_mut().find(|job| job.sequence == updated.sequence)
                    && job.request.budget != updated.request.budget
                {
                    job.request.budget = updated.request.budget.clone();
                }
            }
            for job in jobs.iter().filter(|job| {
                job.state == "queued"
                    && previous_budgets.get(&job.sequence) != Some(&job.request.budget)
            }) {
                store.save(job)?;
            }
            let candidates: Vec<_> = jobs
                .iter()
                .filter(|job| {
                    job.state != "queued"
                        || (sessions.contains_key(&job.request.id)
                            && deferred
                                .get(&job.request.id)
                                .is_none_or(|time| tokio::time::Instant::now() >= *time))
                })
                .cloned()
                .collect();
            for sequence in scheduler::eligible(&candidates, &config, &capacity) {
                let job = jobs
                    .iter_mut()
                    .find(|job| job.sequence == sequence)
                    .expect("scheduler returned an existing sequence");
                job.state = if job.request.prepared {
                    "starting"
                } else {
                    "preparing"
                }
                .into();
                store.save(job)?;
                if let Some((_, reply)) = sessions.get(&job.request.id) {
                    notify(
                        reply,
                        if job.request.prepared {
                            Event::Grant {
                                budget: job.request.budget.clone(),
                            }
                        } else {
                            Event::Prepare
                        },
                    );
                }
                notices.remove(&job.request.id);
            }
            let waiting: Vec<_> = jobs.iter().filter(|job| job.state == "queued").collect();
            for (index, job) in waiting.iter().enumerate() {
                let notice = (
                    index + 1,
                    scheduler::reason(job, &jobs, &capacity).to_owned(),
                );
                if notices.get(&job.request.id) != Some(&notice)
                    && let Some((_, reply)) = sessions.get(&job.request.id)
                {
                    notify(
                        reply,
                        Event::Queued {
                            position: notice.0,
                            reason: notice.1.clone(),
                        },
                    );
                    notices.insert(job.request.id.clone(), notice);
                }
            }
            for job in jobs.iter().filter(|job| job.cancel && !job.terminal()) {
                if let Some((_, reply)) = sessions.get(&job.request.id) {
                    notify(reply, Event::Cancel);
                }
            }
            store.trim()?;
            usage.observe(Observation {
                monotonic_ms: started.elapsed().as_millis() as u64,
                unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
                busy: jobs
                    .iter()
                    .any(|job| job.metered() && job.state == "running"),
                reserved_cpu_percent: jobs
                    .iter()
                    .filter(|job| job.holds_build_capacity())
                    .map(|job| f64::from(job.request.budget.cpu))
                    .sum::<f64>()
                    * 100.0
                    / f64::from(config.cpu_capacity),
                reserved_memory_percent: jobs
                    .iter()
                    .filter(|job| job.holds_build_capacity())
                    .map(|job| job.request.budget.memory_mib as f64)
                    .sum::<f64>()
                    * 100.0
                    / config.memory_capacity_mib as f64,
                host_cpu_percent: monitor.signals.sample.cpu_busy_percent,
            });
            if last_flush.elapsed() >= Duration::from_secs(1) {
                store.save_utilization(&usage)?;
                last_flush = Instant::now();
            }
        }
    }
    store.save_utilization(&usage)?;
    fs::remove_file(&paths.socket)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle(
    paths: &Paths,
    store: &Store,
    config: &Config,
    daemon: &Daemon,
    monitor: &Monitor,
    usage: &Utilization,
    connection: u64,
    message: Message,
    reply: &mpsc::Sender<Event>,
    sessions: &mut HashMap<String, (u64, mpsc::Sender<Event>)>,
    connection_ids: &mut HashMap<u64, String>,
) -> Result<()> {
    match message {
        Message::Register { request } => {
            let _ = paths.lease(&request.id)?;
            if !platform::alive(&request.owner) {
                bail!("guardian is not alive");
            }
            let existing = store
                .jobs()?
                .into_iter()
                .find(|job| job.request.id == request.id);
            if existing.is_none()
                && (request.budget.cpu == 0
                    || request.budget.memory_mib == 0
                    || request.budget.cpu > config.cpu_capacity
                    || request.budget.memory_mib > config.memory_capacity_mib)
            {
                bail!("request budget does not fit coordinator capacity");
            }
            let job = store.register(request)?;
            connection_ids.insert(connection, job.request.id.clone());
            sessions.insert(job.request.id.clone(), (connection, reply.clone()));
            match job.state.as_str() {
                "preparing" => notify(reply, Event::Prepare),
                "starting" => notify(
                    reply,
                    Event::Grant {
                        budget: job.request.budget.clone(),
                    },
                ),
                "running" | "run_phase" => notify(reply, Event::Resume { state: job.state }),
                "finished" | "cancelled" => notify(
                    reply,
                    Event::Error {
                        message: "request has already finished".into(),
                    },
                ),
                // A queued request hears its position and real reason from the next tick.
                _ => {}
            }
        }
        Message::Control { operation, id } => match operation.as_str() {
            "status" => notify(
                reply,
                Event::Snapshot {
                    snapshot: Snapshot {
                        daemon: daemon.identity.clone(),
                        version: daemon.version.clone(),
                        executable: daemon.executable.clone(),
                        drained: store.drained()?,
                        pressure: monitor.signals.sample.pressure.map_or(
                            "unavailable".into(),
                            |value| {
                                match value {
                                    1 => "normal",
                                    2 => "warning",
                                    _ => "critical",
                                }
                                .into()
                            },
                        ),
                        constrained: (monitor.mode() == Pressure::Constrained)
                            .then(|| config.constrained_budget()),
                        held: monitor.mode() == Pressure::Held,
                        cpu_capacity: config.cpu_capacity,
                        memory_capacity_mib: config.memory_capacity_mib,
                        resources: Some(Box::new(monitor.signals.clone())),
                        utilization: usage.visible_hours(
                            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
                        ),
                        legacy: legacy_owners(store, config)?,
                        jobs: {
                            let jobs = store.jobs()?;
                            let cutoff = jobs
                                .iter()
                                .filter(|job| job.terminal())
                                .rev()
                                .nth(19)
                                .map_or(0, |job| job.sequence);
                            jobs.into_iter()
                                .filter(|job| !job.terminal() || job.sequence >= cutoff)
                                .collect()
                        },
                    },
                },
            ),
            "drain" => {
                store.set_drained(true)?;
                notify(reply, Event::Ack);
            }
            "track-legacy" => {
                let paths = serde_json::from_str(
                    id.as_deref()
                        .ok_or_else(|| anyhow::anyhow!("legacy paths required"))?,
                )?;
                store.track_legacy_shims(paths)?;
                notify(reply, Event::Ack);
            }
            "resume" => {
                store.set_drained(false)?;
                notify(reply, Event::Ack);
            }
            "cancel" => {
                let id = id.ok_or_else(|| anyhow::anyhow!("request ID is required"))?;
                let mut job = store
                    .jobs()?
                    .into_iter()
                    .find(|job| job.request.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown request"))?;
                job.cancel = true;
                store.save(&job)?;
                notify(reply, Event::Ack);
            }
            _ => bail!("unknown control operation"),
        },
        other => {
            let id = connection_ids
                .get(&connection)
                .ok_or_else(|| anyhow::anyhow!("register a request first"))?;
            if sessions
                .get(id)
                .is_none_or(|(current, _)| *current != connection)
            {
                bail!("connection has been superseded");
            }
            let mut job = store
                .jobs()?
                .into_iter()
                .find(|job| &job.request.id == id)
                .ok_or_else(|| anyhow::anyhow!("unknown request"))?;
            if job.cancel
                && matches!(
                    other,
                    Message::Running { .. } | Message::Preparing { .. } | Message::RunPhase { .. }
                )
            {
                notify(reply, Event::Cancel);
                return Ok(());
            }
            let ownership = match &other {
                Message::Running { child }
                | Message::Preparing { child }
                | Message::RunPhase { child } => Some(child.clone()),
                _ => None,
            };
            match other {
                Message::Prepared {
                    lane,
                    server,
                    budget,
                } if job.state == "preparing" => {
                    if budget.cpu == 0
                        || budget.memory_mib == 0
                        || budget.cpu > config.cpu_capacity
                        || budget.memory_mib > config.memory_capacity_mib
                    {
                        bail!("prepared budget cannot fit");
                    }
                    job.request.budget = budget;
                    job.request.lane = lane;
                    job.request.prepared = true;
                    job.server = server;
                    job.state = "queued".into();
                }
                Message::Preparing { child } if job.state == "preparing" => {
                    job.child = Some(child);
                }
                Message::Running { child }
                    if matches!(job.state.as_str(), "starting" | "running") =>
                {
                    job.child = Some(child);
                    job.state = "running".into();
                }
                Message::RunPhase { child }
                    if job.state == "running"
                        || (job.state == "run_phase" && job.child.as_ref() == Some(&child)) =>
                {
                    job.child = Some(child);
                    job.state = "run_phase".into();
                }
                Message::Finish { code, stopped } => {
                    job.code = Some(code);
                    job.state = if stopped { "finished" } else { "quarantined" }.into();
                }
                _ => bail!("invalid lifecycle transition from {}", job.state),
            }
            store.save(&job)?;
            if let Some(child) = ownership {
                notify(
                    reply,
                    Event::Owned {
                        child,
                        state: job.state.clone(),
                    },
                );
            } else {
                notify(reply, Event::Ack);
            }
        }
    }
    Ok(())
}

fn legacy_owners(store: &Store, config: &Config) -> Result<Vec<platform::Identity>> {
    let mut owners = platform::legacy_invocations_for(&store.legacy_shims()?)?;
    owners.extend(
        config
            .legacy
            .iter()
            .filter(|owner| platform::alive(owner))
            .cloned(),
    );
    owners.sort_by_key(|owner| owner.pid);
    owners.dedup();
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Budget, Job, Request};

    struct Fixture {
        _root: tempfile::TempDir,
        paths: Paths,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let paths = Paths {
                root: root.path().into(),
                config: root.path().join("config.toml"),
                socket: root.path().join("control.sock"),
                database: root.path().join("queue.sqlite3"),
            };
            let store = Store::open(&paths).unwrap();
            Self {
                _root: root,
                paths,
                store,
            }
        }
        fn job(&self, id: &str, state: &str) -> Job {
            let mut job = self
                .store
                .register(Request {
                    id: id.into(),
                    owner: platform::Identity {
                        pid: 1,
                        birth: "test".into(),
                    },
                    lane: "lane".into(),
                    command: "build".into(),
                    budget: Budget {
                        cpu: 1,
                        memory_mib: 512,
                        exclusive: false,
                        estimate: None,
                    },
                    prepared: true,
                    workspace: "workspace".into(),
                })
                .unwrap();
            job.state = state.into();
            self.store.save(&job).unwrap();
            job
        }
        fn receipt(&self, id: &str) -> PathBuf {
            self.paths.root.join(format!("receipt-{id}"))
        }
        fn write_receipt(&self, id: &str, code: i32, stopped: bool) {
            platform::atomic_write(
                &self.receipt(id),
                &serde_json::to_vec(&(code, stopped)).unwrap(),
            )
            .unwrap();
        }
        fn stored(&self, id: &str) -> Job {
            self.store
                .jobs()
                .unwrap()
                .into_iter()
                .find(|job| job.request.id == id)
                .unwrap()
        }
        fn settle(&self) {
            settle_receipts(&self.paths, &self.store).unwrap();
        }
    }

    #[test]
    fn a_receipt_is_removed_after_finish_already_recorded_the_job() {
        let fixture = Fixture::new();
        let mut job = fixture.job("done", "running");
        fixture.write_receipt("done", 0, true);
        job.code = Some(0);
        job.state = "finished".into();
        fixture.store.save(&job).unwrap();
        fixture.settle();
        assert!(!fixture.receipt("done").exists());
        assert_eq!(fixture.stored("done").state, "finished");
        assert_eq!(fixture.stored("done").code, Some(0));
    }

    #[test]
    fn a_failed_job_receipt_quarantines_the_job_and_is_removed() {
        let fixture = Fixture::new();
        fixture.job("failed", "running");
        fixture.write_receipt("failed", 1, false);
        fixture.settle();
        assert!(!fixture.receipt("failed").exists());
        assert_eq!(fixture.stored("failed").state, "quarantined");
        assert_eq!(fixture.stored("failed").code, Some(1));
    }

    #[test]
    fn a_cancelled_job_keeps_its_state_and_its_late_receipt_is_removed() {
        let fixture = Fixture::new();
        fixture.job("cancelled", "cancelled");
        fixture.write_receipt("cancelled", 130, true);
        fixture.settle();
        assert!(!fixture.receipt("cancelled").exists());
        assert_eq!(fixture.stored("cancelled").state, "cancelled");
        assert_eq!(fixture.stored("cancelled").code, Some(130));
    }

    #[test]
    fn a_runner_that_died_before_finish_is_recorded_from_its_receipt() {
        let fixture = Fixture::new();
        fixture.job("crashed", "run_phase");
        fixture.write_receipt("crashed", 3, true);
        fixture.settle();
        assert!(!fixture.receipt("crashed").exists());
        assert_eq!(fixture.stored("crashed").state, "finished");
        assert_eq!(fixture.stored("crashed").code, Some(3));
    }

    #[test]
    fn a_restarted_coordinator_records_and_removes_pending_receipts() {
        let fixture = Fixture::new();
        fixture.job("running", "running");
        let mut finished = fixture.job("finished", "running");
        finished.state = "finished".into();
        finished.code = Some(0);
        fixture.store.save(&finished).unwrap();
        fixture.write_receipt("running", 0, true);
        fixture.write_receipt("finished", 0, true);
        let restarted = Store::open(&fixture.paths).unwrap();
        settle_receipts(&fixture.paths, &restarted).unwrap();
        assert!(!fixture.receipt("running").exists());
        assert!(!fixture.receipt("finished").exists());
        assert_eq!(fixture.stored("running").state, "finished");
    }

    #[test]
    fn receipts_of_jobs_the_store_no_longer_knows_are_pruned() {
        let fixture = Fixture::new();
        for index in 0..3 {
            fixture.write_receipt(&format!("trimmed-{index}"), 0, true);
        }
        fs::write(fixture.paths.root.join(".write-1"), b"partial").unwrap();
        fixture.settle();
        for index in 0..3 {
            assert!(!fixture.receipt(&format!("trimmed-{index}")).exists());
        }
        assert!(fixture.paths.root.join(".write-1").exists());
    }

    #[test]
    fn an_unreadable_receipt_of_a_running_job_is_kept() {
        let fixture = Fixture::new();
        fixture.job("running", "running");
        fs::write(fixture.receipt("running"), b"{").unwrap();
        fixture.settle();
        assert!(fixture.receipt("running").exists());
        assert_eq!(fixture.stored("running").state, "running");
    }
}
