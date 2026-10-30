#![forbid(unsafe_code)]
//! Recovery policy has no OS, store, spawning or wall-clock dependencies.
use crate::protocol::Job;
use std::time::Duration;

pub const UNRESPONSIVE_GRACE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeState {
    Gone,
    Idle,
    Busy,
    Unresponsive,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub owner_alive: bool,
    pub child_alive: bool,
    pub native: NativeState,
    pub unresponsive_for: Duration,
    pub other_owner: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Wait(&'static str),
    Finish(&'static str),
    StopServer,
}

pub fn decide(job: &Job, observation: Observation) -> Decision {
    if job.state != "quarantined" {
        return Decision::Wait("request is not quarantined");
    }
    if observation.child_alive {
        return Decision::Wait("owned child is still active");
    }
    match observation.native {
        NativeState::Gone => Decision::Finish("execution ended; no native server remains"),
        NativeState::Idle => Decision::Finish("native server verified idle"),
        NativeState::Busy => Decision::Wait("native server is busy"),
        NativeState::Unknown => Decision::Wait("native server could not be verified"),
        NativeState::Unresponsive
            if observation.unresponsive_for >= UNRESPONSIVE_GRACE
                && !observation.other_owner
                && !observation.owner_alive
                && job.server.is_some() =>
        {
            Decision::StopServer
        }
        NativeState::Unresponsive => Decision::Wait("native server is not responding"),
    }
}

/// The exit reported by a guardian wins; recovery never invents success.
pub fn finish(job: &mut Job, reason: &str) {
    job.state = "finished".into();
    job.retiring_server = None;
    let code = *job.code.get_or_insert(125);
    job.reason = Some(match job.reason.take() {
        Some(previous) if previous.contains("unresponsive orphan server") => {
            format!("exit={code}; unresponsive orphan server exited; {reason}")
        }
        Some(previous) if !previous.starts_with("recovery pending:") => {
            format!(
                "{}; recovered: {reason}",
                previous
                    .split("; recovery pending:")
                    .next()
                    .unwrap_or(&previous)
            )
        }
        _ => format!("exit={code}; recovered: {reason}"),
    });
}

pub fn record_finish(job: &mut Job, code: i32, stopped: bool, reason: Option<String>) {
    if job.terminal() {
        // A receipt may precede Finish. Enrich its generic diagnosis, but never
        // reopen a terminal job or replace a later recovery/cancellation reason.
        if job.code == Some(code)
            && reason.is_some()
            && (job.reason.is_none()
                || job.reason.as_deref() == Some(&format!("backend exited with code {code}")))
        {
            job.reason = reason;
        }
        return;
    }
    job.code = Some(code);
    job.state = if stopped { "finished" } else { "quarantined" }.into();
    job.reason = reason.or_else(|| (code != 0).then(|| format!("backend exited with code {code}")));
}

/// Both snapshots and the connection must still match when an async probe ends.
pub fn current(observed: &Job, job: &Job, connection: Option<u64>, now: Option<u64>) -> bool {
    observed == job && connection == now && job.state == "quarantined"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        protocol::NativeServer,
        scheduler::{self, Capacity, Pressure},
    };

    fn job(sequence: u64, state: &str) -> Job {
        serde_json::from_value(serde_json::json!({
            "sequence":sequence,"state":state,"child":null,"server":null,"code":null,"cancel":false,
            "request":{"id":sequence.to_string(),"owner":{"pid":1,"birth":"old"},
                "lane":"same","workspace":"same","command":"test","prepared":true,
                "budget":{"cpu":2,"memory_mib":2304,"exclusive":false}}
        }))
        .unwrap()
    }
    fn observation(native: NativeState) -> Observation {
        Observation {
            owner_alive: false,
            child_alive: false,
            native,
            unresponsive_for: Duration::ZERO,
            other_owner: false,
        }
    }
    fn server() -> NativeServer {
        NativeServer {
            identity: crate::platform::Identity {
                pid: 2,
                birth: "old".into(),
            },
            output_base: "same".into(),
            workspace: "same".into(),
            version: "8.4.2".into(),
        }
    }

    #[test]
    fn recovered_quarantine_releases_tokens_memory_slot_and_lane_before_admission() {
        let config = Config {
            cpu_capacity: 8,
            memory_capacity_mib: 9216,
            max_builds: 1,
            ..Config::default()
        };
        for native in [NativeState::Gone, NativeState::Idle] {
            for pressure in [Pressure::Normal, Pressure::Constrained] {
                let mut jobs = vec![job(1, "quarantined"), job(2, "queued"), job(3, "queued")];
                jobs[2].request.lane = "other".into();
                jobs[2].request.workspace = "other".into();
                let capacity = Capacity {
                    pressure,
                    legacy_active: false,
                    drained: false,
                    run_memory_mib: 0,
                    new_memory_mib: 9216,
                    reason: None,
                };
                assert!(scheduler::eligible(&jobs, &config, &capacity).is_empty());
                let Decision::Finish(reason) = decide(&jobs[0], observation(native)) else {
                    panic!("orphan not recovered")
                };
                finish(&mut jobs[0], reason);
                assert_eq!(jobs[0].code, Some(125));
                assert!(!jobs[0].holds_build_capacity());
                assert_eq!(scheduler::eligible(&jobs, &config, &capacity), vec![2]);
                jobs[1].state = "finished".into();
                assert_eq!(scheduler::eligible(&jobs, &config, &capacity), vec![3]);
            }
        }
    }

    #[test]
    fn disk_failure_finishes_and_preserves_the_exit_and_reason() {
        let mut failed = job(1, "running");
        record_finish(
            &mut failed,
            36,
            true,
            Some("disk full (No space left on device)".into()),
        );
        assert!(failed.terminal());
        assert!(!failed.holds_capacity());
        assert_eq!(failed.code, Some(36));
        assert!(failed.reason.unwrap().contains("disk full"));
    }

    #[test]
    fn late_finish_enriches_a_receipt_but_cannot_reopen_or_erase_a_recovered_job() {
        let mut ended = job(1, "running");
        record_finish(&mut ended, 36, true, None);
        record_finish(&mut ended, 36, false, Some("disk full".into()));
        assert_eq!(ended.state, "finished");
        assert_eq!(ended.reason.as_deref(), Some("disk full"));
        finish(&mut ended, "native server verified idle");
        let recovered = ended.clone();
        record_finish(&mut ended, 36, false, Some("disk full".into()));
        assert_eq!(ended, recovered);
        record_finish(&mut ended, 0, false, None);
        assert_eq!(ended, recovered);
    }

    #[test]
    fn no_amount_of_elapsed_time_releases_executing_or_unknown_work() {
        let mut orphan = job(1, "quarantined");
        orphan.server = Some(server());
        for native in [
            NativeState::Idle,
            NativeState::Gone,
            NativeState::Busy,
            NativeState::Unresponsive,
            NativeState::Unknown,
        ] {
            let mut observed = observation(native);
            observed.unresponsive_for = Duration::MAX;
            observed.child_alive = true;
            assert!(matches!(decide(&orphan, observed), Decision::Wait(_)));
        }
        for native in [NativeState::Busy, NativeState::Unknown] {
            assert!(matches!(
                decide(
                    &orphan,
                    Observation {
                        unresponsive_for: Duration::MAX,
                        ..observation(native)
                    }
                ),
                Decision::Wait(_)
            ));
        }
    }

    #[test]
    fn a_paused_guardian_cannot_hold_a_quarantine_after_authenticated_idleness() {
        let orphan = job(1, "quarantined");
        assert!(matches!(
            decide(
                &orphan,
                Observation {
                    owner_alive: true,
                    ..observation(NativeState::Idle)
                }
            ),
            Decision::Finish(_)
        ));
        let running = job(1, "running");
        assert!(matches!(
            decide(
                &running,
                Observation {
                    owner_alive: true,
                    ..observation(NativeState::Idle)
                }
            ),
            Decision::Wait(_)
        ));
    }

    #[test]
    fn unresponsive_recovery_requires_grace_and_exclusive_orphan_ownership() {
        let mut orphan = job(1, "quarantined");
        orphan.server = Some(server());
        let mut observed = observation(NativeState::Unresponsive);
        observed.unresponsive_for = UNRESPONSIVE_GRACE - Duration::from_nanos(1);
        assert!(matches!(decide(&orphan, observed), Decision::Wait(_)));
        observed.unresponsive_for = UNRESPONSIVE_GRACE;
        assert_eq!(decide(&orphan, observed), Decision::StopServer);
        observed.other_owner = true;
        assert!(matches!(decide(&orphan, observed), Decision::Wait(_)));
        observed.other_owner = false;
        orphan.server = None;
        assert!(matches!(decide(&orphan, observed), Decision::Wait(_)));
    }

    #[test]
    fn a_completed_guardian_need_not_die_before_recovery_can_finish() {
        let mut completed = job(1, "quarantined");
        record_finish(&mut completed, 130, false, None);
        let observed = Observation {
            owner_alive: true,
            ..observation(NativeState::Idle)
        };
        let Decision::Finish(reason) = decide(&completed, observed) else {
            panic!("completion held")
        };
        finish(&mut completed, reason);
        assert_eq!(completed.code, Some(130));
        assert!(completed.terminal());
    }

    #[test]
    fn changed_children_servers_states_and_reconnections_reject_late_results() {
        let original = job(1, "quarantined");
        assert!(current(&original, &original, Some(1), Some(1)));
        assert!(!current(&original, &original, Some(1), Some(2)));
        for mutate in [
            |job: &mut Job| job.state = "running".into(),
            |job: &mut Job| job.child = Some(server().identity),
            |job: &mut Job| job.server = Some(server()),
            |job: &mut Job| job.request.owner.birth = "reused".into(),
            |job: &mut Job| job.code = Some(1),
            |job: &mut Job| job.cancel = true,
        ] {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert!(!current(&original, &changed, Some(1), Some(1)));
        }
    }
}
