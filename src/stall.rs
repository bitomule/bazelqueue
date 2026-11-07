#![forbid(unsafe_code)]
//! Stall policy has no OS, store, spawning or wall-clock dependencies.
use serde::{Deserialize, Serialize};

/// The convention of `timeout(1)`, so a caller reads a cut job as a time failure.
pub const EXIT_CODE: i32 = 124;
pub const EXIT_NAME: &str = "TIMEOUT";

/// A hung Bazel server used at most 2.4% of a core and a healthy one running a
/// sleeping test 1.1%, so CPU alone proves nothing below this share of a core.
const IDLE_CPU_PERCENT: u64 = 5;

/// What a file says about its writer without opening it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub bytes: u64,
    pub modified_seconds: i64,
    pub modified_nanos: i64,
}

/// One look at a Bazel server. `None` means the signal could not be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub at_ms: u64,
    pub events: Option<Fingerprint>,
    pub log: Option<Fingerprint>,
    pub children: Option<u32>,
    pub cpu_ms: Option<u64>,
    pub clients: Option<u32>,
}

/// The typed proof that a job stopped progressing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stall {
    pub silent_ms: u64,
    pub cpu_ms: u64,
    pub children: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<u32>,
}

impl Stall {
    /// Nobody else can be using a server without a connection to its command port.
    pub fn orphaned(&self) -> bool {
        self.clients == Some(0)
    }
    pub fn reason(&self) -> String {
        format!(
            "no progress for {}s: Bazel events and command.log unchanged, {} child processes, {} ms of CPU",
            self.silent_ms / 1000,
            self.children,
            self.cpu_ms
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Quiet {
    since_ms: u64,
    cpu_ms: u64,
}

/// Follows one job's readings. Time only counts while every reading is known
/// and silent, so elapsed time alone can never produce a stall.
#[derive(Clone, Copy, Debug, Default)]
pub struct Watch {
    events: Option<Fingerprint>,
    log: Option<Fingerprint>,
    quiet: Option<Quiet>,
    stall: Option<Stall>,
}

impl Watch {
    pub fn observe(&mut self, reading: Reading, window_ms: u64) -> Option<Stall> {
        let wrote = changed(&mut self.events, reading.events) | changed(&mut self.log, reading.log);
        self.stall = None;
        let (Some(cpu_ms), Some(children)) = (reading.cpu_ms, reading.children) else {
            self.quiet = None;
            return None;
        };
        let budget_ms = window_ms * IDLE_CPU_PERCENT / 100;
        let quiet = match self.quiet {
            Some(quiet) if !wrote && cpu_ms.saturating_sub(quiet.cpu_ms) <= budget_ms => quiet,
            _ => Quiet {
                since_ms: reading.at_ms,
                cpu_ms,
            },
        };
        self.quiet = Some(quiet);
        let silent_ms = reading.at_ms.saturating_sub(quiet.since_ms);
        // Bazel 9 writes no command.log by default. Without that heartbeat, a
        // child with a client waiting on it may be a test that is only sleeping.
        let provable = children == 0 || reading.log.is_some() || reading.clients == Some(0);
        self.stall = (window_ms > 0 && silent_ms >= window_ms && provable).then_some(Stall {
            silent_ms,
            cpu_ms: cpu_ms.saturating_sub(quiet.cpu_ms),
            children,
            clients: reading.clients,
        });
        self.stall
    }
    pub fn stalled(&self) -> Option<Stall> {
        self.stall
    }
}

/// A file that disappeared wrote nothing: a deleted output base is not progress.
fn changed(known: &mut Option<Fingerprint>, seen: Option<Fingerprint>) -> bool {
    let Some(seen) = seen else {
        return false;
    };
    known.replace(seen).is_some_and(|known| known != seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: u64 = 600_000;

    fn file(bytes: u64) -> Option<Fingerprint> {
        Some(Fingerprint {
            bytes,
            ..Fingerprint::default()
        })
    }
    fn reading(at_ms: u64) -> Reading {
        Reading {
            at_ms,
            events: file(1),
            log: file(1),
            children: Some(0),
            cpu_ms: Some(1_000),
            clients: Some(0),
        }
    }
    fn watched(readings: &[Reading]) -> Option<Stall> {
        let mut watch = Watch::default();
        readings
            .iter()
            .map(|reading| watch.observe(*reading, WINDOW))
            .last()
            .flatten()
    }

    #[test]
    fn a_silent_server_without_children_stalls_exactly_at_the_window() {
        assert_eq!(watched(&[reading(0), reading(WINDOW - 1)]), None);
        let stall = watched(&[reading(0), reading(WINDOW)]).unwrap();
        assert_eq!(stall.silent_ms, WINDOW);
        assert_eq!(stall.children, 0);
        assert!(stall.orphaned());
    }

    #[test]
    fn output_or_events_restart_the_silence() {
        for touched in [
            Reading {
                log: file(2),
                ..reading(WINDOW - 1)
            },
            Reading {
                events: file(2),
                ..reading(WINDOW - 1)
            },
        ] {
            let quiet_at = |at_ms| Reading { at_ms, ..touched };
            assert_eq!(watched(&[reading(0), touched, quiet_at(WINDOW)]), None);
            assert!(watched(&[reading(0), touched, quiet_at(2 * WINDOW)]).is_some());
        }
    }

    #[test]
    fn a_slow_test_is_not_a_stall_while_it_uses_the_machine() {
        let busy = Reading {
            cpu_ms: Some(1_000 + WINDOW / 10),
            children: Some(3),
            ..reading(WINDOW)
        };
        assert_eq!(watched(&[reading(0), busy]), None);
    }

    #[test]
    fn a_sleeping_test_stays_alive_through_bazels_progress_lines() {
        let readings: Vec<_> = (0..30_u64)
            .map(|minute| Reading {
                log: file(minute),
                children: Some(1),
                ..reading(minute * 60_000)
            })
            .collect();
        assert_eq!(watched(&readings), None);
    }

    #[test]
    fn idle_cpu_spikes_within_the_budget_do_not_restart_the_silence() {
        let spiked = Reading {
            cpu_ms: Some(1_000 + WINDOW * IDLE_CPU_PERCENT / 100),
            ..reading(WINDOW)
        };
        assert_eq!(watched(&[reading(0), spiked]).unwrap().silent_ms, WINDOW);
    }

    #[test]
    fn hung_children_do_not_hide_a_stall() {
        let hung = Reading {
            children: Some(2),
            ..reading(WINDOW)
        };
        assert_eq!(watched(&[reading(0), hung]).unwrap().children, 2);
    }

    #[test]
    fn without_a_heartbeat_a_child_with_a_waiting_client_may_be_a_sleeping_test() {
        let sleeping = |clients| Reading {
            log: None,
            children: Some(1),
            clients,
            ..reading(WINDOW)
        };
        let start = Reading {
            log: None,
            ..reading(0)
        };
        assert_eq!(watched(&[start, sleeping(Some(1))]), None);
        assert_eq!(watched(&[start, sleeping(None)]), None);
        assert!(watched(&[start, sleeping(Some(0))]).is_some());
        let no_children = Reading {
            children: Some(0),
            ..sleeping(Some(1))
        };
        assert!(watched(&[start, no_children]).is_some());
    }

    #[test]
    fn unknown_signals_never_prove_a_stall_and_restart_the_silence() {
        let no_cpu = Reading {
            cpu_ms: None,
            ..reading(WINDOW)
        };
        let no_children = Reading {
            children: None,
            ..reading(WINDOW)
        };
        assert_eq!(watched(&[reading(0), no_cpu]), None);
        assert_eq!(watched(&[reading(0), no_children]), None);
        assert_eq!(watched(&[reading(0), no_cpu, reading(WINDOW + 1)]), None);
    }

    #[test]
    fn deleting_the_output_base_is_not_progress() {
        let deleted = Reading {
            log: None,
            events: None,
            ..reading(WINDOW)
        };
        assert!(watched(&[reading(0), deleted]).is_some());
    }

    #[test]
    fn an_established_or_unreadable_client_means_the_server_is_not_proven_orphaned() {
        for clients in [Some(1), None] {
            let connected = Reading {
                clients,
                ..reading(WINDOW)
            };
            assert!(!watched(&[reading(0), connected]).unwrap().orphaned());
        }
    }

    #[test]
    fn a_verdict_ends_when_progress_returns_and_a_zero_window_never_stalls() {
        let mut watch = Watch::default();
        watch.observe(reading(0), WINDOW);
        assert!(watch.observe(reading(WINDOW), WINDOW).is_some());
        assert!(watch.stalled().is_some());
        let resumed = Reading {
            log: file(9),
            ..reading(WINDOW + 1)
        };
        assert_eq!(watch.observe(resumed, WINDOW), None);
        assert_eq!(watch.stalled(), None);
        let mut off = Watch::default();
        off.observe(reading(0), 0);
        assert_eq!(off.observe(reading(u64::MAX), 0), None);
    }
}
