use anyhow::{Context, Result, bail};
use bazelqueue::{
    config::{Config, Paths},
    coordinator, install, platform,
    protocol::Event,
    runner,
};
use clap::{Parser, Subcommand};
use std::{ffi::OsString, os::unix::process::ExitStatusExt, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Queue Bazel invocations until machine capacity is available"
)]
struct Cli {
    #[command(subcommand)]
    command: Control,
}
#[derive(Subcommand)]
enum Control {
    Exec {
        #[arg(last = true, required = true)]
        arguments: Vec<OsString>,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Watch,
    Cancel {
        id: String,
    },
    Drain,
    Resume,
    Doctor,
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
    Setup {
        #[arg(long)]
        backend: Option<PathBuf>,
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        migrate: bool,
        #[arg(long)]
        preview: bool,
    },
    Uninstall,
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}
#[derive(Subcommand)]
enum ConfigCommand {
    Show,
}
#[derive(Subcommand)]
enum DaemonCommand {
    Run,
}
#[derive(Subcommand)]
enum PackageCommand {
    Formula {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        release_version: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = entry().await {
        eprintln!("bazelqueue: {error:#}");
        std::process::exit(125);
    }
}

async fn entry() -> Result<()> {
    let mut args = std::env::args_os();
    let name = PathBuf::from(args.next().unwrap_or_default());
    let rest: Vec<_> = args.collect();
    if rest.first().is_some_and(|arg| arg == "_executor") {
        use std::os::unix::process::CommandExt;
        use tokio::io::AsyncReadExt;
        let mut control = tokio::net::UnixStream::from_std(platform::control_socket()?)?;
        let mut permit = [0_u8; 1];
        control
            .read_exact(&mut permit)
            .await
            .context("guardian disappeared before execution was authorized")?;
        if permit != [b'G'] {
            bail!("invalid execution permit");
        }
        drop(control);
        let mut command =
            std::process::Command::new(rest.get(1).context("executor backend missing")?);
        command.args(rest.get(3..).context("executor arguments missing")?);
        return Err(command.exec().into());
    }
    if name
        .file_name()
        .is_some_and(|name| name == "bazel" || name == "bazelisk")
    {
        let paths = Paths::discover()?;
        let config = Config::load(&paths)?;
        let backend = if name.file_name().is_some_and(|name| name == "bazel") {
            config.bazel_backend.clone().unwrap_or(config.backend)
        } else {
            config.backend
        };
        exit(runner::frontend(backend, rest, false).await?);
    }
    if rest.first().is_some_and(|arg| arg == "_guardian") {
        let backend = PathBuf::from(rest.get(1).context("guardian backend is missing")?);
        let generic = rest.get(2).is_some_and(|arg| arg == "generic");
        exit(
            runner::guardian(
                backend,
                rest.get(4..)
                    .context("guardian arguments are missing")?
                    .to_vec(),
                generic,
            )
            .await?,
        );
    }
    let cli = Cli::parse();
    let paths = Paths::discover()?;
    match cli.command {
        Control::Exec { arguments } => {
            let mut args = arguments.into_iter();
            let backend = PathBuf::from(args.next().context("backend is required")?);
            let generic = !backend
                .file_name()
                .is_some_and(|name| name == "bazel" || name == "bazelisk");
            exit(runner::frontend(backend, args.collect(), generic).await?);
        }
        Control::Daemon {
            command: DaemonCommand::Run,
        } => coordinator::run(paths).await?,
        Control::Status { json } => {
            show(coordinator::control(&paths, "status", None).await?, json)?
        }
        Control::Watch => loop {
            show(coordinator::control(&paths, "status", None).await?, false)?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        },
        Control::Cancel { id } => {
            coordinator::control(&paths, "cancel", Some(id)).await?;
        }
        Control::Drain => {
            coordinator::control(&paths, "drain", None).await?;
        }
        Control::Resume => {
            coordinator::control(&paths, "resume", None).await?;
        }
        Control::Config {
            command: ConfigCommand::Show,
        } => println!("{}", toml::to_string_pretty(&Config::load(&paths)?)?),
        Control::Setup {
            backend,
            bin_dir,
            replace,
            migrate,
            preview,
        } => install::setup(&paths, backend, bin_dir, replace, migrate, preview).await?,
        Control::Uninstall => install::uninstall(&paths).await?,
        Control::Doctor => install::doctor(&paths).await?,
        Control::Package {
            command:
                PackageCommand::Formula {
                    archive,
                    release_version,
                    output,
                },
        } => {
            use sha2::{Digest, Sha256};
            let numeric = release_version.split('-').next().unwrap_or_default();
            if numeric.split('.').count() != 3
                || !numeric
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                || !release_version
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
            {
                bail!("invalid release version");
            }
            let checksum = format!("{:x}", Sha256::digest(std::fs::read(archive)?));
            let formula = include_str!("../packaging/homebrew/bazelqueue.rb.in")
                .replace("__VERSION__", &release_version)
                .replace("__CHECKSUM__", &checksum);
            std::fs::write(output, formula)?;
        }
    }
    Ok(())
}
fn exit(status: std::process::ExitStatus) -> ! {
    if let Some(signal) = status.signal() {
        platform::signal_exit(signal);
    }
    std::process::exit(status.code().unwrap_or(125))
}
fn show(event: Event, json: bool) -> Result<()> {
    let Event::Snapshot { snapshot } = event else {
        bail!("coordinator returned no snapshot");
    };
    if json {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else {
        println!(
            "Coordinator {} | pressure {} | capacity {} CPU / {} MiB | {}",
            snapshot.daemon.pid,
            snapshot.pressure,
            snapshot.cpu_capacity,
            snapshot.memory_capacity_mib,
            if snapshot.drained {
                "draining"
            } else {
                "accepting"
            }
        );
        for owner in snapshot.legacy {
            println!("legacy {} draining", owner.pid);
        }
        for job in snapshot.jobs.iter().filter(|job| !job.terminal()) {
            println!(
                "{} {} {} {} CPU {} MiB {}",
                job.request.id,
                job.state,
                job.request.command,
                job.request.budget.cpu,
                job.request.budget.memory_mib,
                job.request.lane
            );
        }
    }
    Ok(())
}
