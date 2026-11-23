#![forbid(unsafe_code)]
//! One short, stable account of how a request ended, so a caller can learn the
//! result without reading the whole Bazel log.
use crate::{config::Paths, platform};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub const PREFIX: &str = "bazelqueue:";
/// How long a summary stays; there is no cap on how many, so a busy day never
/// pushes out the runs a measurement still needs.
pub const RETAINED: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const PRINTED_FAILURES: usize = 20;
const LISTED_ARGUMENTS: usize = 100;
const SECRET_WORDS: [&str; 6] = ["header", "token", "password", "secret", "credential", "key"];
const STALE_EVENTS: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Failure {
    pub label: String,
    pub kind: String,
    pub status: String,
    pub log: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Summary {
    pub id: String,
    pub command: String,
    pub workspace: String,
    pub exit_code: i32,
    pub exit_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub queued_ms: u64,
    pub ran_ms: u64,
    /// Whether Bazel reported per-target results; false for unmanaged invocations.
    pub targets_reported: bool,
    pub failures: Vec<Failure>,
    /// Test targets that reported a result; only meaningful when `targets_reported`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tests_total: u32,
    /// Of those, the ones answered from Bazel's cache without running.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tests_cached: u32,
    /// What the caller asked Bazel to build or test.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// The options of the invocation, with secret values hidden.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// Started at once because Bazel reported nothing to rebuild.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast_lane: bool,
    /// Present when bazelqueue cut the job because it stopped progressing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall: Option<crate::stall::Stall>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Events {
    pub exit_name: Option<String>,
    pub reason: Option<String>,
    pub failures: Vec<Failure>,
    pub tests_total: u32,
    pub tests_cached: u32,
}

fn looks_like_label(argument: &str) -> bool {
    ["//", "@", ":", "-//", "-@"]
        .iter()
        .any(|prefix| argument.starts_with(prefix))
}

fn hide_secret(flag: &str, value: &str) -> String {
    let name = flag.to_ascii_lowercase();
    if name.ends_with("_env") {
        return match value.split_once('=') {
            Some((variable, _)) => format!("{variable}=<redacted>"),
            None => value.to_owned(),
        };
    }
    if SECRET_WORDS.iter().any(|word| name.contains(word)) {
        return "<redacted>".into();
    }
    value.to_owned()
}

/// Splits what follows the command into targets and options. Anything after
/// `--` belongs to the program a `bazel run` starts, so it is not kept.
pub fn split_arguments(arguments: &[std::ffi::OsString]) -> (Vec<String>, Vec<String>) {
    let mut targets = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    let mut awaiting_value = false;
    for argument in arguments.iter().map(|argument| argument.to_string_lossy()) {
        if argument == "--" {
            break;
        }
        if looks_like_label(&argument) {
            targets.push(argument.into_owned());
            awaiting_value = false;
        } else if argument.starts_with('-') {
            match argument.split_once('=') {
                Some((flag, value)) => {
                    flags.push(format!("{flag}={}", hide_secret(flag, value)));
                    awaiting_value = false;
                }
                None => {
                    awaiting_value = argument.starts_with("--");
                    flags.push(argument.into_owned());
                }
            }
        } else if awaiting_value && let Some(flag) = flags.last_mut() {
            *flag = format!("{flag} {}", hide_secret(flag, &argument));
            awaiting_value = false;
        } else {
            targets.push(argument.into_owned());
        }
    }
    targets.truncate(LISTED_ARGUMENTS);
    flags.truncate(LISTED_ARGUMENTS);
    (targets, flags)
}

fn label(id: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        id.get(key)?
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn local_path(uri: &str) -> String {
    let Some(path) = uri.strip_prefix("file://") else {
        return uri.to_owned();
    };
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = path
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn test_log(files: Option<&Value>) -> Option<String> {
    let files = files?.as_array()?;
    files
        .iter()
        .find(|file| file.get("name").and_then(Value::as_str) == Some("test.log"))
        .or_else(|| files.first())
        .and_then(|file| file.get("uri").and_then(Value::as_str))
        .map(local_path)
}

/// Reads a Build Event Protocol JSON file. A file cut short by a crash still
/// yields every complete event before the cut.
pub fn parse_events(text: &str) -> Events {
    let mut events = Events::default();
    let mut logs = std::collections::HashMap::new();
    let mut all_cached: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut summarised = std::collections::HashSet::new();
    let add = |events: &mut Events, failure: Failure| {
        if let Some(existing) = events
            .failures
            .iter_mut()
            .find(|existing| existing.label == failure.label)
        {
            if existing.log.is_none() {
                existing.log = failure.log;
            }
        } else {
            events.failures.push(failure);
        }
    };
    for event in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        let id = event.get("id").cloned().unwrap_or(Value::Null);
        // Store a fixed diagnosis, never arbitrary stderr or failure messages.
        if [
            event.pointer("/progress/stderr"),
            event.pointer("/aborted/description"),
            event.pointer("/finished/failureDetail/message"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|text| text.contains("No space left on device"))
        {
            events.reason = Some("disk full (No space left on device)".into());
        }
        if let Some(result) = event.get("testResult")
            && let Some(name) = label(&id, &["testResult"])
        {
            let cached = result.get("cachedLocally").and_then(Value::as_bool) == Some(true)
                || result
                    .pointer("/executionInfo/cachedRemotely")
                    .and_then(Value::as_bool)
                    == Some(true);
            let entry = all_cached.entry(name).or_insert(true);
            *entry &= cached;
        }
        if let Some(result) = event.get("testResult")
            && let Some(name) = label(&id, &["testResult"])
            && let Some(log) = test_log(result.get("testActionOutput"))
        {
            logs.insert(name, log);
        }
        if let Some(summary) = event.get("testSummary")
            && let Some(name) = label(&id, &["testSummary"])
        {
            summarised.insert(name.clone());
            let status = summary
                .get("overallStatus")
                .and_then(Value::as_str)
                .unwrap_or("NO_STATUS");
            if !matches!(status, "PASSED" | "FLAKY") {
                let log = test_log(summary.get("failed")).or_else(|| logs.get(&name).cloned());
                add(
                    &mut events,
                    Failure {
                        label: name,
                        kind: "test".into(),
                        status: status.into(),
                        log,
                    },
                );
            }
        }
        if let Some(completed) = event.get("completed")
            && completed.get("success").and_then(Value::as_bool) != Some(true)
            && let Some(name) = label(&id, &["targetCompleted"])
        {
            add(
                &mut events,
                Failure {
                    label: name,
                    kind: "build".into(),
                    status: "FAILED_TO_BUILD".into(),
                    log: None,
                },
            );
        }
        if let Some(aborted) = event.get("aborted")
            && let Some(reason) = aborted.get("reason").and_then(Value::as_str)
            && matches!(reason, "ANALYSIS_FAILURE" | "LOADING_FAILURE")
            && let Some(name) = label(
                &id,
                &["targetCompleted", "configuredLabel", "unconfiguredLabel"],
            )
            .or_else(|| {
                id.get("pattern")?.get("pattern")?.as_array().map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
        {
            add(
                &mut events,
                Failure {
                    label: name,
                    kind: "build".into(),
                    status: reason.into(),
                    log: None,
                },
            );
        }
        if let Some(name) = event
            .get("finished")
            .and_then(|finished| finished.get("exitCode"))
            .and_then(|code| code.get("name"))
            .and_then(Value::as_str)
        {
            events.exit_name = Some(name.to_owned());
        }
    }
    events.tests_total = summarised.len() as u32;
    events.tests_cached = summarised
        .iter()
        .filter(|name| all_cached.get(*name) == Some(&true))
        .count() as u32;
    events
}

pub fn duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    match seconds {
        0..60 => format!("{}.{}s", seconds, (milliseconds % 1000) / 100),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

impl Summary {
    pub fn lines(&self, location: Option<&Path>) -> Vec<String> {
        let mut first = format!(
            "{PREFIX} finished {} exit={}{} queued={} ran={}",
            self.id,
            self.exit_code,
            self.exit_name
                .as_deref()
                .map(|name| format!(" {name}"))
                .unwrap_or_default(),
            duration(self.queued_ms),
            duration(self.ran_ms),
        );
        if let Some(reason) = &self.reason {
            first.push_str(&format!(" reason={reason}"));
        }
        if self.targets_reported {
            first.push_str(&format!(" failed={}", self.failures.len()));
        } else {
            first.push_str(" failed=unreported");
        }
        let mut lines = vec![first];
        for failure in self.failures.iter().take(PRINTED_FAILURES) {
            lines.push(match (&failure.log, failure.kind.as_str()) {
                (Some(log), _) => format!(
                    "{PREFIX} FAILED {} {} log={log}",
                    failure.label, failure.status
                ),
                (None, "build") => format!("{PREFIX} FAILED {} (build)", failure.label),
                (None, _) => format!("{PREFIX} FAILED {} {}", failure.label, failure.status),
            });
        }
        if self.failures.len() > PRINTED_FAILURES {
            lines.push(format!(
                "{PREFIX} … and {} more failures in the summary",
                self.failures.len() - PRINTED_FAILURES
            ));
        }
        if let Some(location) = location {
            lines.push(format!("{PREFIX} summary {}", location.display()));
        }
        lines
    }
}

pub fn directory(paths: &Paths) -> Result<PathBuf> {
    let directory = paths.root.join("runs");
    platform::private_directory(&directory)?;
    Ok(directory)
}

pub fn location(paths: &Paths, id: &str) -> Result<PathBuf> {
    paths.lease(id)?;
    Ok(directory(paths)?.join(format!("{id}.json")))
}

pub fn events_location(paths: &Paths, id: &str) -> Result<PathBuf> {
    paths.lease(id)?;
    Ok(directory(paths)?.join(format!("{id}.bep.json")))
}

pub fn load(paths: &Paths, id: &str) -> Result<Option<Summary>> {
    match fs::read(location(paths, id)?) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn save(paths: &Paths, summary: &Summary) -> Result<PathBuf> {
    let path = location(paths, &summary.id)?;
    platform::atomic_write(&path, &serde_json::to_vec_pretty(summary)?)?;
    prune(&directory(paths)?, RETAINED, SystemTime::now())?;
    Ok(path)
}

/// A copy where the caller asked for it. The caller owns that directory, so
/// unlike the private state its permissions are left as they are.
pub fn copy_to(destination: &Path, summary: &Summary) -> Result<()> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = tempfile_in(parent)?;
    temporary
        .1
        .write_all(&serde_json::to_vec_pretty(summary)?)?;
    temporary.1.sync_all()?;
    fs::rename(&temporary.0, destination).inspect_err(|_| {
        let _ = fs::remove_file(&temporary.0);
    })?;
    Ok(())
}

fn tempfile_in(parent: &Path) -> Result<(PathBuf, fs::File)> {
    let path = parent.join(format!(".bazelqueue-summary-{}", std::process::id()));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("cannot write the summary copy in {}", parent.display()))?;
    Ok((path, file))
}

/// Drops summaries older than `keep` and event files nobody finished reading.
pub fn prune(directory: &Path, keep: Duration, now: SystemTime) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let modified = entry.metadata()?.modified()?;
        if name.ends_with(".bep.json") {
            if now.duration_since(modified).unwrap_or_default() > STALE_EVENTS {
                let _ = fs::remove_file(entry.path());
            }
        } else if name.ends_with(".json") && now.duration_since(modified).unwrap_or_default() > keep
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stalled_job_reads_as_a_time_failure() {
        let stalled = Summary {
            exit_code: crate::stall::EXIT_CODE,
            exit_name: Some(crate::stall::EXIT_NAME.into()),
            reason: Some("no progress for 600s".into()),
            stall: Some(crate::stall::Stall {
                silent_ms: 600_000,
                cpu_ms: 100,
                children: 0,
                clients: Some(0),
            }),
            ..summary(0)
        };
        assert!(stalled.lines(None)[0].contains("exit=124 TIMEOUT"));
        let json = serde_json::to_value(&stalled).unwrap();
        assert_eq!(json["stall"]["silent_ms"], 600_000);
    }

    #[test]
    fn explicit_disk_failure_is_reported_without_copying_stderr() {
        let events = parse_events(
            r#"{"progress":{"stderr":"private arguments then No space left on device"}}"#,
        );
        assert_eq!(
            events.reason.as_deref(),
            Some("disk full (No space left on device)")
        );
        assert!(
            parse_events(r#"{"finished":{"exitCode":{"name":"LOCAL_ENVIRONMENTAL_ERROR"}}}"#)
                .reason
                .is_none()
        );
        let mut failed = summary(0);
        failed.reason = events.reason;
        assert!(failed.lines(None)[0].contains("reason=disk full"));
        assert!(!failed.lines(None)[0].contains("private arguments"));
        let mut old = serde_json::to_value(summary(0)).unwrap();
        old.as_object_mut().unwrap().remove("reason");
        assert_eq!(serde_json::from_value::<Summary>(old).unwrap().reason, None);
    }

    fn summary(failures: usize) -> Summary {
        Summary {
            id: "7-1".into(),
            command: "test".into(),
            workspace: "/w".into(),
            exit_code: 3,
            exit_name: Some("TESTS_FAILED".into()),
            reason: None,
            queued_ms: 192_000,
            ran_ms: 65_400,
            targets_reported: true,
            failures: (0..failures)
                .map(|index| Failure {
                    label: format!("//t:{index}"),
                    kind: "test".into(),
                    status: "FAILED".into(),
                    log: Some(format!("/logs/{index}/test.log")),
                })
                .collect(),
            fast_lane: false,
            stall: None,
            ..Summary::default()
        }
    }

    #[test]
    fn failing_tests_build_failures_and_analysis_errors_are_reported_once_with_their_logs() {
        let events = [
            r#"{"id":{"started":{}},"started":{"uuid":"u"}}"#,
            r#"{"id":{"testResult":{"label":"//app:unit_tests","run":1,"shard":1,"attempt":1}},"testResult":{"status":"FAILED","testActionOutput":[{"name":"test.xml","uri":"file:///o/t/test.xml"},{"name":"test.log","uri":"file:///o/t/app/unit%20tests/test.log"}]}}"#,
            r#"{"id":{"testSummary":{"label":"//app:unit_tests"}},"testSummary":{"overallStatus":"FAILED","failed":[]}}"#,
            r#"{"id":{"testSummary":{"label":"//app:flaky"}},"testSummary":{"overallStatus":"FLAKY"}}"#,
            r#"{"id":{"testSummary":{"label":"//app:ok"}},"testSummary":{"overallStatus":"PASSED"}}"#,
            r#"{"id":{"targetCompleted":{"label":"//app:unit_tests"}},"completed":{"success":true}}"#,
            r#"{"id":{"targetCompleted":{"label":"//lib:core"}},"completed":{"failureDetail":{"message":"x"}}}"#,
            r#"{"id":{"testSummary":{"label":"//lib:core_test"}},"testSummary":{"overallStatus":"FAILED_TO_BUILD"}}"#,
            r#"{"id":{"configuredLabel":{"label":"//bad:target"}},"aborted":{"reason":"ANALYSIS_FAILURE","description":"no such target"}}"#,
            r#"{"id":{"targetCompleted":{"label":"//skipped:one"}},"aborted":{"reason":"SKIPPED"}}"#,
            r#"{"id":{"buildFinished":{}},"finished":{"exitCode":{"name":"TESTS_FAILED","code":3}}}"#,
            r#"{"id":{"testSummary":{"label":"//cut:off"}},"testSumm"#,
        ]
        .join("\n");
        let parsed = parse_events(&events);
        assert_eq!(parsed.exit_name.as_deref(), Some("TESTS_FAILED"));
        assert_eq!(
            parsed.failures,
            vec![
                Failure {
                    label: "//app:unit_tests".into(),
                    kind: "test".into(),
                    status: "FAILED".into(),
                    log: Some("/o/t/app/unit tests/test.log".into()),
                },
                Failure {
                    label: "//lib:core".into(),
                    kind: "build".into(),
                    status: "FAILED_TO_BUILD".into(),
                    log: None,
                },
                Failure {
                    label: "//lib:core_test".into(),
                    kind: "test".into(),
                    status: "FAILED_TO_BUILD".into(),
                    log: None,
                },
                Failure {
                    label: "//bad:target".into(),
                    kind: "build".into(),
                    status: "ANALYSIS_FAILURE".into(),
                    log: None,
                },
            ]
        );
    }

    #[test]
    fn the_printed_block_is_stable_and_bounded() {
        let lines = summary(23).lines(Some(Path::new("/s/7-1.json")));
        assert_eq!(
            lines[0],
            "bazelqueue: finished 7-1 exit=3 TESTS_FAILED queued=3m12s ran=1m05s failed=23"
        );
        assert_eq!(
            lines[1],
            "bazelqueue: FAILED //t:0 FAILED log=/logs/0/test.log"
        );
        assert_eq!(lines.len(), 1 + 20 + 2);
        assert_eq!(
            lines[21],
            "bazelqueue: … and 3 more failures in the summary"
        );
        assert_eq!(lines[22], "bazelqueue: summary /s/7-1.json");
        let mut unmanaged = summary(0);
        unmanaged.targets_reported = false;
        unmanaged.exit_name = None;
        unmanaged.exit_code = 0;
        unmanaged.queued_ms = 400;
        assert_eq!(
            unmanaged.lines(None),
            ["bazelqueue: finished 7-1 exit=0 queued=0.4s ran=1m05s failed=unreported"]
        );
    }

    fn aged(path: &Path, now: SystemTime, age: Duration) {
        fs::write(path, "{}").unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(now - age)
            .unwrap();
    }

    #[test]
    fn pruning_keeps_a_week_of_summaries_however_many_and_drops_abandoned_event_files() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for index in 0..300_u64 {
            aged(
                &root.path().join(format!("{index}.json")),
                now,
                Duration::from_secs(index),
            );
        }
        aged(
            &root.path().join("week.json"),
            now,
            Duration::from_secs(7 * 24 * 60 * 60 - 1),
        );
        aged(
            &root.path().join("older.json"),
            now,
            Duration::from_secs(7 * 24 * 60 * 60 + 1),
        );
        let stale = root.path().join("old.bep.json");
        aged(&stale, now, STALE_EVENTS + Duration::from_secs(1));
        let live = root.path().join("live.bep.json");
        fs::write(&live, "").unwrap();
        prune(root.path(), RETAINED, now).unwrap();
        let left: std::collections::HashSet<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left.len(), 300 + 1 + 1);
        assert!(left.contains("week.json") && left.contains("live.bep.json"));
        assert!(!left.contains("older.json") && !left.contains("old.bep.json"));
    }

    fn arguments(values: &[&str]) -> (Vec<String>, Vec<String>) {
        split_arguments(
            &values
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn arguments_split_into_targets_and_flags_without_the_programs_own_arguments() {
        let (targets, flags) = arguments(&[
            "--config",
            "ci",
            "--keep_going",
            "//a/...",
            "-//a:slow",
            "@dep//b:c",
            "--test_env=TOKEN=abc",
            "--remote_header=Authorization=Bearer x",
            "--",
            "--program-flag",
            "//not/a/target",
        ]);
        assert_eq!(targets, ["//a/...", "-//a:slow", "@dep//b:c"]);
        assert_eq!(
            flags,
            [
                "--config ci",
                "--keep_going",
                "--test_env=TOKEN=<redacted>",
                "--remote_header=<redacted>",
            ]
        );
        let (_, secret) = arguments(&["--remote_header", "Authorization=Bearer x"]);
        assert_eq!(secret, ["--remote_header <redacted>"]);
        let many: Vec<String> = (0..150).map(|index| format!("//t:{index}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(arguments(&many).0.len(), LISTED_ARGUMENTS);
    }

    #[test]
    fn a_record_with_targets_flags_and_cache_counts_round_trips_and_old_records_still_load() {
        let recorded = Summary {
            targets: vec!["//app:tests".into()],
            flags: vec!["--config ci".into()],
            tests_total: 3,
            tests_cached: 2,
            ..summary(0)
        };
        let json = serde_json::to_value(&recorded).unwrap();
        assert_eq!(json["tests_cached"], 2);
        assert_eq!(serde_json::from_value::<Summary>(json).unwrap(), recorded);
        let mut old = serde_json::to_value(summary(0)).unwrap();
        for field in ["targets", "flags", "tests_total", "tests_cached"] {
            assert!(old.as_object_mut().unwrap().remove(field).is_none());
        }
        assert_eq!(serde_json::from_value::<Summary>(old).unwrap(), summary(0));
    }

    #[test]
    fn tests_answered_from_the_cache_are_counted_once_per_target() {
        let result = |label: &str, cache: &str| {
            format!(
                r#"{{"id":{{"testResult":{{"label":"{label}"}}}},"testResult":{{{cache}"status":"PASSED"}}}}"#
            )
        };
        let summary = |label: &str| {
            format!(
                r#"{{"id":{{"testSummary":{{"label":"{label}"}}}},"testSummary":{{"overallStatus":"PASSED"}}}}"#
            )
        };
        let events = [
            result("//a:local", r#""cachedLocally":true,"#),
            summary("//a:local"),
            result("//a:remote", r#""executionInfo":{"cachedRemotely":true},"#),
            summary("//a:remote"),
            result("//a:ran", ""),
            summary("//a:ran"),
            result("//a:ran_too", ""),
            summary("//a:ran_too"),
            result("//a:shards", r#""cachedLocally":true,"#),
            result("//a:shards", ""),
            summary("//a:shards"),
        ]
        .join("\n");
        let parsed = parse_events(&events);
        assert_eq!((parsed.tests_total, parsed.tests_cached), (5, 2));
    }

    #[test]
    fn a_copy_lands_in_a_caller_directory_without_changing_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let destination = root.path().join("result.json");
        copy_to(&destination, &summary(1)).unwrap();
        let copied: Summary = serde_json::from_slice(&fs::read(&destination).unwrap()).unwrap();
        assert_eq!(copied, summary(1));
        assert_eq!(
            fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
