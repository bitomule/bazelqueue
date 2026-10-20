#![forbid(unsafe_code)]
use crate::{
    config::{Config, Paths},
    platform,
    protocol::{self, Event, Message, Snapshot},
    scheduler::{self, Capacity, Pressure},
    store::Store,
};
use anyhow::{Result, bail};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
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

fn sample_pressure(paths: &Paths) -> Option<u32> {
    #[cfg(feature = "test-fixtures")]
    if let Ok(value) = std::fs::read_to_string(paths.root.join("pressure")) {
        return value.trim().parse().ok();
    }
    let _ = paths;
    platform::pressure()
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
    Ok(
        ensure_from(paths, &std::env::current_exe()?.canonicalize()?)
            .await?
            .0,
    )
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

/// Replaces a running coordinator that is not `executable` at this version.
/// The caller must have drained the queue; only queued requests may exist,
/// and their guardians reconnect to the successor.
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
        if snapshot
            .jobs
            .iter()
            .any(|job| !job.terminal() && job.state != "queued")
        {
            bail!(
                "coordinator {} (bazelqueue {}) is executing requests and cannot be replaced yet",
                snapshot.daemon.pid,
                describe_version(&snapshot)
            );
        }
        protocol::send(
            &mut stream,
            &Message::Control {
                operation: "retire".into(),
                id: None,
            },
        )
        .await?;
        let interrupt = match protocol::receive::<_, Event>(&mut stream).await {
            Ok(Event::Error { message }) if message == "unknown control operation" => true,
            Ok(Event::Error { message }) => bail!("{message}"),
            Ok(_) | Err(_) => false,
        };
        if interrupt && (peer != snapshot.daemon.pid || !platform::alive(&snapshot.daemon)) {
            bail!(
                "cannot verify that process {} is the coordinator; refusing to signal it",
                snapshot.daemon.pid
            );
        }
        drop(stream);
        retired(paths, &snapshot, interrupt).await?;
        previous.get_or_insert(snapshot);
    }
    bail!("an older coordinator kept replacing the installed one")
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

async fn retired(paths: &Paths, snapshot: &Snapshot, interrupt: bool) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut signal_at = tokio::time::Instant::now();
    loop {
        if !platform::alive(&snapshot.daemon)
            || platform::lock(&paths.root.join("daemon.lock")).is_ok()
        {
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
        };
        #[cfg(feature = "test-fixtures")]
        match std::env::var("BAZELQUEUE_FIXTURE_COORDINATOR").as_deref() {
            Ok("legacy") => {
                daemon.version.clear();
                daemon.executable = None;
                daemon.legacy = true;
            }
            Ok(version) => daemon.version = version.into(),
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

fn notify(reply: &mpsc::Sender<Event>, event: Event) {
    let _ = reply.try_send(event);
}

pub async fn run(paths: Paths) -> Result<()> {
    let _lock = match platform::lock(&paths.root.join("daemon.lock")) {
        Ok(lock) => lock,
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
    let mut healthy_samples = 0_u32;
    let mut pressure = sample_pressure(&paths);
    let mut last_known = pressure;
    let mut mode = Pressure::from_samples(pressure, None, 0, config.pressure_recovery_samples);
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
                            if store.jobs()?.iter().any(|job|!job.terminal() && job.state!="queued") {
                                notify(&reply,Event::Error {message:"requests are executing; the coordinator can be replaced once they finish".into()});
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
                        let result=handle(&paths,&store,&config,&daemon,pressure,mode,connection,message,&reply,&mut sessions,&mut connection_ids);
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
                    pressure=sample_pressure(&paths);
                    if pressure==Some(1) {healthy_samples=healthy_samples.saturating_add(1);} else {healthy_samples=0;}
                    mode=Pressure::from_samples(pressure,last_known,healthy_samples,config.pressure_recovery_samples);
                    if pressure.is_some() {last_known=pressure;}
                }

            }
            _=tokio::signal::ctrl_c()=>break,
        }
        if reschedule {
            let mut jobs = store.jobs()?;
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
                    store.save(job)?;
                }
                if job.state == "queued"
                    && job.metered()
                    && scheduler::fit_queued(&mut job.request.budget, &config, mode)
                {
                    store.save(job)?;
                }
                let receipt = paths.root.join(format!("receipt-{}", job.request.id));
                if let Ok(bytes) = fs::read(&receipt)
                    && let Ok((code, stopped)) = serde_json::from_slice::<(i32, bool)>(&bytes)
                {
                    job.code = Some(code);
                    job.state = if stopped { "finished" } else { "quarantined" }.into();
                    store.save(job)?;
                    let _ = fs::remove_file(receipt);
                    continue;
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
                    store.save(job)?;
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
            if let Some(job) =
                scheduler::raise_lone_job(&candidates, &mut jobs, &limit, remaining_memory)
            {
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
        }
    }
    fs::remove_file(&paths.socket)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle(
    paths: &Paths,
    store: &Store,
    config: &Config,
    daemon: &Daemon,
    pressure: Option<u32>,
    mode: Pressure,
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
                "queued" => {
                    let position = store
                        .jobs()?
                        .iter()
                        .filter(|other| other.state == "queued" && other.sequence <= job.sequence)
                        .count();
                    notify(
                        reply,
                        Event::Queued {
                            position,
                            reason: "resource capacity".into(),
                        },
                    );
                }
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
                        pressure: pressure.map_or("unavailable".into(), |value| {
                            match value {
                                1 => "normal",
                                2 => "warning",
                                _ => "critical",
                            }
                            .into()
                        }),
                        constrained: (mode == Pressure::Constrained)
                            .then(|| config.constrained_budget()),
                        held: mode == Pressure::Held,
                        cpu_capacity: config.cpu_capacity,
                        memory_capacity_mib: config.memory_capacity_mib,
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
