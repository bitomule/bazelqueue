#![forbid(unsafe_code)]
use crate::{platform, protocol::Budget};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub backend: PathBuf,
    pub bazel_backend: Option<PathBuf>,
    pub cpu_capacity: u32,
    pub memory_capacity_mib: u64,
    pub max_builds: usize,
    pub jobs: u32,
    pub action_memory_mib: u64,
    pub worker_instances: u32,
    pub managed: bool,
    pub pressure_recovery_samples: u32,
    pub hooks: Vec<Hook>,
    pub legacy: Vec<platform::Identity>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub program: PathBuf,
    #[serde(default)]
    pub arguments: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get() as u32);
        let cpu = ((cores * 3) / 4).max(1);
        let memory = platform::total_memory_mib().unwrap_or(8192);
        let budget = (memory / 2).max(1024);
        Self {
            backend: PathBuf::from("/opt/homebrew/opt/bazelisk/bin/bazelisk"),
            bazel_backend: None,
            cpu_capacity: cpu,
            memory_capacity_mib: budget,
            max_builds: 1,
            jobs: cpu,
            action_memory_mib: (budget * 2 / 3).max(256),
            worker_instances: 2,
            managed: true,
            pressure_recovery_samples: 3,
            hooks: Vec::new(),
            legacy: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        match fs::read_to_string(&paths.config) {
            Ok(text) => {
                let config: Self =
                    toml::from_str(&text).context("invalid bazelqueue configuration")?;
                config.validate()?;
                Ok(config)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn validate(&self) -> Result<()> {
        if !self.backend.is_absolute()
            || self
                .bazel_backend
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
        {
            bail!("backend paths must be absolute so execution cannot search the intercepted PATH");
        }
        if self.cpu_capacity == 0
            || self.memory_capacity_mib < 256
            || self.max_builds == 0
            || self.jobs == 0
            || self.worker_instances == 0
            || self.pressure_recovery_samples == 0
        {
            bail!("resource capacities and concurrency must be positive");
        }
        if self.max_builds > usize::try_from(self.cpu_capacity)?
            || self.max_builds as u64 > self.memory_capacity_mib / 256
        {
            bail!("concurrency must fit positive CPU and at least 256 MiB per invocation");
        }
        if self.action_memory_mib > self.memory_capacity_mib || self.action_memory_mib == 0 {
            bail!("action memory must fit the invocation memory budget");
        }
        Ok(())
    }
    pub fn budget(&self, managed: bool) -> Budget {
        Budget {
            cpu: if managed {
                (self.cpu_capacity / self.max_builds as u32).max(1)
            } else {
                self.cpu_capacity
            },
            memory_mib: if managed {
                self.memory_capacity_mib / self.max_builds as u64
            } else {
                self.memory_capacity_mib
            },
            exclusive: !managed,
        }
    }
    pub fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        platform::atomic_write(&paths.config, toml::to_string_pretty(self)?.as_bytes())
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub socket: PathBuf,
    pub database: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let root = if let Some(root) = env::var_os("BAZELQUEUE_HOME") {
            PathBuf::from(root)
        } else {
            PathBuf::from(env::var_os("HOME").context("HOME is not set")?)
                .join(".local/state/bazelqueue")
        };
        platform::private_directory(&root)?;
        let socket = root.join("control.sock");
        if socket.as_os_str().len() > 100 {
            bail!(
                "BAZELQUEUE_HOME is too long for a Unix socket; choose a path shorter than 85 bytes"
            );
        }
        Ok(Self {
            config: root.join("config.toml"),
            database: root.join("queue.sqlite3"),
            socket,
            root,
        })
    }
    pub fn lease(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty()
            || id.len() > 80
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            bail!("invalid request identity");
        }
        let directory = self.root.join("leases");
        platform::private_directory(&directory)?;
        Ok(directory.join(id))
    }
}

pub fn workspace(cwd: &Path) -> PathBuf {
    for directory in cwd.ancestors() {
        if ["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"]
            .iter()
            .any(|file| directory.join(file).exists())
        {
            return directory.to_owned();
        }
    }
    cwd.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uneven_cpu_capacity_admits_every_configured_share() {
        let config = Config {
            cpu_capacity: 11,
            max_builds: 2,
            ..Config::default()
        };
        assert_eq!(config.budget(true).cpu, 5);
        assert!(config.budget(true).cpu * 2 <= config.cpu_capacity);
        assert_eq!(config.budget(false).cpu, 11);
    }
    #[test]
    fn unsafe_backend_names_and_zero_share_configurations_are_rejected() {
        assert!(
            Config {
                backend: "bazel".into(),
                ..Config::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                bazel_backend: Some("bazel".into()),
                ..Config::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                max_builds: usize::MAX,
                ..Config::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Config {
                cpu_capacity: 2048,
                memory_capacity_mib: 1024,
                action_memory_mib: 512,
                max_builds: 1025,
                ..Config::default()
            }
            .validate()
            .is_err()
        );
    }
}
