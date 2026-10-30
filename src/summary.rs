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
pub const RETAINED: usize = 200;
const PRINTED_FAILURES: usize = 20;
const STALE_EVENTS: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Failure {
    pub label: String,
    pub kind: String,
    pub status: String,
    pub log: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Events {
    pub exit_name: Option<String>,
    pub reason: Option<String>,
    pub failures: Vec<Failure>,
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
            && let Some(log) = test_log(result.get("testActionOutput"))
        {
            logs.insert(name, log);
        }
        if let Some(summary) = event.get("testSummary")
            && let Some(name) = label(&id, &["testSummary"])
        {
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

/// Keeps the newest `keep` summaries and drops event files nobody finished reading.
pub fn prune(directory: &Path, keep: usize, now: SystemTime) -> Result<()> {
    let mut summaries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let modified = entry.metadata()?.modified()?;
        if name.ends_with(".bep.json") {
            if now.duration_since(modified).unwrap_or_default() > STALE_EVENTS {
                let _ = fs::remove_file(entry.path());
            }
        } else if name.ends_with(".json") {
            summaries.push((modified, entry.path()));
        }
    }
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.0));
    for (_, path) in summaries.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn pruning_keeps_the_newest_summaries_and_drops_abandoned_event_files() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for index in 0..5_u64 {
            let path = root.path().join(format!("{index}.json"));
            fs::write(&path, "{}").unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(now - Duration::from_secs(100 - index))
                .unwrap();
        }
        let stale = root.path().join("old.bep.json");
        fs::write(&stale, "").unwrap();
        fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(now - STALE_EVENTS - Duration::from_secs(1))
            .unwrap();
        let live = root.path().join("live.bep.json");
        fs::write(&live, "").unwrap();
        prune(root.path(), 2, now).unwrap();
        let mut left: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["3.json", "4.json", "live.bep.json"]);
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
