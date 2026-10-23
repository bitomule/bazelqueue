#![forbid(unsafe_code)]
//! Sizes a build or test from what earlier runs of the same invocation used, so a
//! small test run reserves a few CPUs instead of the whole machine.
use crate::{
    bazel::Invocation,
    config::Config,
    platform,
    protocol::{self, Admission, Budget, Estimate},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const SAMPLES: usize = 3;
const KEYS: usize = 512;
const HEADROOM: f64 = 1.25;
const MINIMUM_CPU: u32 = 2;
/// Bazel's own start-up burst makes a sub-second cached run look multi-core.
const SHORTEST_WALL_SECONDS: f64 = 10.0;
/// A run that kept most of its reservation busy may have been held back by it.
const SATURATED: f64 = 0.75;

#[derive(Default, Serialize, Deserialize)]
struct History {
    entries: BTreeMap<String, Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    cores: Vec<f64>,
    updated: u64,
}

/// Worktrees of one repository share a key: they build the same targets
/// against the same caches.
pub fn key(invocation: &Invocation, workspace: &Path) -> String {
    let start = invocation.command_index.map_or(0, |index| index + 1);
    let mut key = format!(
        "{}\0{}",
        repository(workspace),
        invocation.command.as_deref().unwrap_or("unknown")
    );
    let mut skip_value = false;
    for arg in &invocation.args[start..] {
        let text = arg.to_string_lossy();
        if std::mem::take(&mut skip_value) {
            continue;
        }
        if text == "--build_event_json_file" {
            skip_value = true;
            continue;
        }
        if text.starts_with("--build_event_json_file=") {
            continue;
        }
        key.push('\0');
        key.push_str(&text);
    }
    key
}

fn repository(workspace: &Path) -> String {
    for directory in workspace.ancestors() {
        let git = directory.join(".git");
        let common = if git.is_dir() {
            git
        } else if let Ok(text) = std::fs::read_to_string(&git)
            && let Some(target) = text.trim().strip_prefix("gitdir:")
        {
            let target = directory.join(target.trim());
            match target.parent() {
                Some(worktrees)
                    if worktrees
                        .file_name()
                        .is_some_and(|name| name == "worktrees") =>
                {
                    worktrees.parent().unwrap_or(worktrees).to_owned()
                }
                _ => target,
            }
        } else {
            continue;
        };
        let inside = workspace.strip_prefix(directory).unwrap_or(workspace);
        return common.join(inside).to_string_lossy().into_owned();
    }
    workspace.to_string_lossy().into_owned()
}

/// Average cores a finished run kept busy, or the whole capacity when it may
/// have been limited by a smaller reservation.
pub fn observe(cpu_seconds: f64, wall_seconds: f64, granted_cpu: u32, config: &Config) -> f64 {
    let cores = cpu_seconds.max(0.0) / wall_seconds.max(SHORTEST_WALL_SECONDS);
    if granted_cpu < config.cpu_capacity && cores >= f64::from(granted_cpu) * SATURATED {
        f64::from(config.cpu_capacity)
    } else {
        cores
    }
}

pub fn budget(config: &Config, invocation: &Invocation, cores: &[f64]) -> Option<Budget> {
    if !invocation.managed
        || protocol::admission(invocation.command.as_deref().unwrap_or("unknown"))
            != Admission::Metered
    {
        return None;
    }
    let peak = cores.iter().copied().reduce(f64::max)?;
    let cpu = ((peak * HEADROOM).ceil() as u32)
        .clamp(MINIMUM_CPU.min(config.cpu_capacity), config.cpu_capacity);
    let memory_mib = config.memory_capacity_mib * u64::from(cpu) / u64::from(config.cpu_capacity);
    Some(Budget {
        cpu,
        memory_mib,
        exclusive: false,
        estimate: Some(Estimate { cpu, memory_mib }),
    })
}

pub fn lookup(root: &Path, key: &str) -> Vec<f64> {
    load(root)
        .entries
        .remove(key)
        .map(|entry| entry.cores)
        .unwrap_or_default()
}

pub fn record(root: &Path, key: &str, cores: f64) -> Result<()> {
    platform::private_directory(root)?;
    let _lock = platform::wait_lock(&root.join("estimates.lock"))?;
    let mut history = load(root);
    let updated = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let entry = history.entries.entry(key.to_owned()).or_insert(Entry {
        cores: Vec::new(),
        updated,
    });
    entry.cores.push(cores);
    if entry.cores.len() > SAMPLES {
        entry.cores.remove(0);
    }
    entry.updated = updated;
    while history.entries.len() > KEYS {
        let oldest = history
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.updated)
            .map(|(key, _)| key.clone())
            .expect("history is not empty");
        history.entries.remove(&oldest);
    }
    platform::atomic_write(&root.join("estimates.json"), &serde_json::to_vec(&history)?)
}

fn load(root: &Path) -> History {
    std::fs::read(root.join("estimates.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn config() -> Config {
        Config {
            cpu_capacity: 8,
            memory_capacity_mib: 9216,
            max_builds: 1,
            ..Config::default()
        }
    }
    fn invocation(args: &[&str], workspace: &Path) -> Invocation {
        crate::bazel::inspect(
            args.iter().map(OsString::from).collect(),
            workspace,
            &config(),
        )
    }
    fn workspace(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("MODULE.bazel"), "").unwrap();
        std::fs::write(root.join(".bazelversion"), "8.4.2").unwrap();
        root.to_owned()
    }

    #[test]
    fn a_small_test_run_is_estimated_below_the_whole_machine() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(&root.path().join("w"));
        let test = invocation(&["test", "//:unit"], &workspace);
        let config = config();
        let whole = config.request_budget(Admission::Metered, true);
        assert_eq!((whole.cpu, whole.memory_mib), (8, 9216));

        let cached = observe(4.0, 30.0, whole.cpu, &config);
        let estimated = budget(&config, &test, &[cached]).unwrap();
        assert_eq!((estimated.cpu, estimated.memory_mib), (2, 2304));
        assert!(estimated.cpu < whole.cpu && estimated.memory_mib < whole.memory_mib);
        assert_eq!(
            estimated.estimate,
            Some(Estimate {
                cpu: 2,
                memory_mib: 2304
            })
        );

        let compiling = observe(7.5 * 300.0, 300.0, whole.cpu, &config);
        assert_eq!(budget(&config, &test, &[cached, compiling]).unwrap().cpu, 8);
        assert!(budget(&config, &test, &[]).is_none());
        assert!(budget(&config, &invocation(&["info"], &workspace), &[cached]).is_none());
    }

    #[test]
    fn a_run_held_back_by_its_estimate_asks_for_the_machine_next_time() {
        let config = config();
        assert_eq!(observe(1.8 * 60.0, 60.0, 2, &config), 8.0);
        assert!(observe(0.5 * 60.0, 60.0, 2, &config) < 1.0);
        assert!(observe(1.1, 0.28, 8, &config) < 0.2);
    }

    #[test]
    fn worktrees_of_one_repository_share_history_and_targets_do_not() {
        let root = tempfile::tempdir().unwrap();
        let main = workspace(&root.path().join("repo"));
        std::fs::create_dir_all(main.join(".git/worktrees/feature")).unwrap();
        let worktree = workspace(&main.join(".claude/worktrees/feature"));
        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git/worktrees/feature").display()
            ),
        )
        .unwrap();
        let in_main = key(&invocation(&["test", "//:unit"], &main), &main);
        let in_worktree = key(
            &invocation(
                &["test", "--build_event_json_file=/tmp/x.json", "//:unit"],
                &worktree,
            ),
            &worktree,
        );
        assert_eq!(in_main, in_worktree);
        assert_ne!(
            in_main,
            key(&invocation(&["test", "//:other"], &main), &main)
        );
        assert_ne!(
            in_main,
            key(&invocation(&["build", "//:unit"], &main), &main)
        );

        let state = root.path().join("state");
        for cores in [0.4, 0.6, 0.8, 1.0] {
            record(&state, &in_main, cores).unwrap();
        }
        assert_eq!(lookup(&state, &in_worktree), vec![0.6, 0.8, 1.0]);
        assert!(lookup(&state, "unknown").is_empty());
    }
}
