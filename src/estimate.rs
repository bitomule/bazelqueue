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

/// Flags that change what a run reports or which tests in its targets run,
/// but barely what it costs.
const VALUED: [&str; 7] = [
    "--test_filter",
    "--test_arg",
    "--test_output",
    "--test_env",
    "--runs_per_test",
    "--build_event_json_file",
    "--execution_log_json_file",
];
const SWITCHES: [&str; 2] = ["--nocache_test_results", "--cache_test_results"];
const FAMILY_RUNS: usize = 9;

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
        let (name, value) = match text.split_once('=') {
            Some((name, _)) => (name, true),
            None => (text.as_ref(), false),
        };
        if VALUED.contains(&name) {
            skip_value = !value;
            continue;
        }
        if SWITCHES.contains(&name) {
            continue;
        }
        key.push('\0');
        key.push_str(&text);
    }
    key
}

fn family(key: &str) -> &str {
    match key.match_indices('\0').nth(1) {
        Some((end, _)) => &key[..end],
        None => key,
    }
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

/// The runs of this exact key, or else the median of the latest runs of the
/// same command in the same repository.
pub fn lookup(root: &Path, key: &str) -> Vec<f64> {
    let mut history = load(root);
    if let Some(entry) = history.entries.remove(key) {
        return entry.cores;
    }
    let wanted = family(key);
    let mut related: Vec<&Entry> = history
        .entries
        .iter()
        .filter(|(other, _)| family(other) == wanted)
        .map(|(_, entry)| entry)
        .collect();
    related.sort_by_key(|entry| std::cmp::Reverse(entry.updated));
    let mut cores: Vec<f64> = related
        .iter()
        .take(FAMILY_RUNS)
        .filter_map(|entry| entry.cores.last().copied())
        .collect();
    if cores.is_empty() {
        return cores;
    }
    cores.sort_by(f64::total_cmp);
    let middle = cores.len() / 2;
    let median = if cores.len().is_multiple_of(2) {
        (cores[middle - 1] + cores[middle]) / 2.0
    } else {
        cores[middle]
    };
    vec![median]
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

    #[test]
    fn runs_that_differ_only_in_cheap_flags_share_an_estimate() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(&root.path().join("w"));
        let filtered = key(
            &invocation(
                &[
                    "test",
                    "//:unit",
                    "--test_filter=Fast",
                    "--test_output=errors",
                ],
                &workspace,
            ),
            &workspace,
        );
        let other_filter = key(
            &invocation(
                &[
                    "test",
                    "--test_filter",
                    "Slow",
                    "//:unit",
                    "--test_arg=--verbose",
                    "--runs_per_test=3",
                    "--nocache_test_results",
                ],
                &workspace,
            ),
            &workspace,
        );
        assert_eq!(filtered, other_filter);
        assert_ne!(
            filtered,
            key(
                &invocation(&["test", "//:unit", "--config=release"], &workspace),
                &workspace
            )
        );

        let state = root.path().join("state");
        record(&state, &filtered, 0.9).unwrap();
        assert_eq!(lookup(&state, &other_filter), vec![0.9]);
    }

    #[test]
    fn a_new_run_is_sized_by_the_median_of_its_repository() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = workspace(&root.path().join("elsewhere"));
        let workspace = workspace(&root.path().join("w"));
        let state = root.path().join("state");
        for (target, cores) in [("//:a", 1.0), ("//:b", 3.0), ("//:c", 5.0)] {
            let test = invocation(&["test", target], &workspace);
            record(&state, &key(&test, &workspace), cores).unwrap();
        }
        let build = invocation(&["build", "//:a"], &workspace);
        record(&state, &key(&build, &workspace), 8.0).unwrap();
        let foreign = invocation(&["test", "//:a"], &elsewhere);
        record(&state, &key(&foreign, &elsewhere), 8.0).unwrap();

        let new = invocation(&["test", "//:new"], &workspace);
        let history = lookup(&state, &key(&new, &workspace));
        assert_eq!(history, vec![3.0]);
        let estimated = budget(&config(), &new, &history).unwrap();
        assert_eq!((estimated.cpu, estimated.memory_mib), (4, 4608));
        assert!(estimated.estimate.is_some());

        let unseen = invocation(&["test", "//:new"], &root.path().join("unseen"));
        assert!(lookup(&state, &key(&unseen, &root.path().join("unseen"))).is_empty());
    }

    #[test]
    fn without_any_history_a_run_asks_for_what_it_did_before() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(&root.path().join("w"));
        let test = invocation(&["test", "//:unit"], &workspace);
        let history = lookup(&root.path().join("state"), &key(&test, &workspace));
        assert!(history.is_empty());
        assert!(budget(&config(), &test, &history).is_none());
    }

    #[test]
    fn history_written_with_the_old_keys_still_loads() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(&root.path().join("w"));
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let repository = repository(&workspace);
        let old = serde_json::json!({"entries": {
            format!("{repository}\0test\0//:unit\0--test_filter=Fast"):
                {"cores": [2.0, 4.0], "updated": 1},
        }});
        std::fs::write(state.join("estimates.json"), old.to_string()).unwrap();

        let test = invocation(&["test", "//:unit", "--test_filter=Fast"], &workspace);
        let key = key(&test, &workspace);
        assert_eq!(lookup(&state, &key), vec![4.0]);
        record(&state, &key, 1.0).unwrap();
        assert_eq!(lookup(&state, &key), vec![1.0]);
    }
}
