#![forbid(unsafe_code)]
use crate::{config::Config, protocol::Job};

pub struct Capacity {
    pub healthy: bool,
    pub legacy_active: bool,
    pub drained: bool,
    pub run_memory_mib: u64,
}

pub fn eligible(jobs: &[Job], config: &Config, capacity: &Capacity) -> Vec<u64> {
    if !capacity.healthy || capacity.legacy_active || capacity.drained {
        return Vec::new();
    }
    let mut active: Vec<&Job> = jobs.iter().filter(|job| job.holds_capacity()).collect();
    let mut cpu = active.iter().map(|job| job.request.budget.cpu).sum::<u32>();
    let mut memory = active
        .iter()
        .map(|job| job.request.budget.memory_mib)
        .sum::<u64>()
        .saturating_add(capacity.run_memory_mib);
    let mut chosen = Vec::new();
    let mut waiting: Vec<_> = jobs
        .iter()
        .filter(|job| job.state == "queued" && !job.cancel)
        .collect();
    let ready: Vec<_> = waiting
        .iter()
        .copied()
        .filter(|job| job.request.prepared)
        .collect();
    let ready_lanes = ready
        .iter()
        .map(|job| &job.request.lane)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if active.is_empty()
        && !ready.is_empty()
        && ready_lanes < config.max_builds
        && ready.len() < config.max_builds.saturating_mul(2)
        && !ready[0].request.budget.exclusive
        && let Some(index) = waiting.iter().position(|job| {
            !job.request.prepared
                && !ready.iter().any(|other| {
                    other.request.workspace == job.request.workspace
                        || other.request.lane == job.request.lane
                })
        })
    {
        // Discover one more lane before committing the bounded ready batch.
        let preparation = waiting.remove(index);
        waiting.insert(0, preparation);
    }
    for job in waiting {
        if active
            .iter()
            .any(|other| other.request.lane == job.request.lane)
        {
            continue;
        }
        let budget = &job.request.budget;
        let exclusive = active.iter().any(|job| job.request.budget.exclusive);
        if exclusive
            || (budget.exclusive && !active.is_empty())
            || active.len() >= config.max_builds
            || cpu.saturating_add(budget.cpu) > config.cpu_capacity
            || memory.saturating_add(budget.memory_mib) > config.memory_capacity_mib
        {
            break;
        }
        cpu += budget.cpu;
        memory += budget.memory_mib;
        active.push(job);
        chosen.push(job.sequence);
    }
    chosen
}

pub fn reason(job: &Job, jobs: &[Job], capacity: &Capacity) -> &'static str {
    if capacity.drained {
        "queue is draining"
    } else if capacity.legacy_active {
        "legacy Bazel invocations are draining"
    } else if !capacity.healthy {
        "memory pressure"
    } else if jobs
        .iter()
        .any(|other| other.holds_capacity() && other.request.lane == job.request.lane)
    {
        "workspace is busy"
    } else {
        "resource capacity"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        platform::Identity,
        protocol::{Budget, Request},
    };
    fn job(sequence: u64, lane: &str, cpu: u32, exclusive: bool, state: &str) -> Job {
        Job {
            sequence,
            request: Request {
                id: sequence.to_string(),
                owner: Identity {
                    pid: 1,
                    birth: "test".into(),
                },
                lane: lane.into(),
                command: "build".into(),
                budget: Budget {
                    cpu,
                    memory_mib: 512,
                    exclusive,
                },
                prepared: true,
                workspace: lane.into(),
            },
            state: state.into(),
            child: None,
            server: None,
            code: None,
            cancel: false,
        }
    }
    fn capacity() -> Capacity {
        Capacity {
            healthy: true,
            legacy_active: false,
            drained: false,
            run_memory_mib: 0,
        }
    }
    fn config() -> Config {
        Config {
            cpu_capacity: 8,
            memory_capacity_mib: 2048,
            max_builds: 2,
            action_memory_mib: 1024,
            ..Config::default()
        }
    }
    #[test]
    fn budgets_are_immutable_and_large_jobs_are_not_starved() {
        let jobs = vec![
            job(1, "a", 4, false, "running"),
            job(2, "b", 8, false, "queued"),
            job(3, "c", 1, false, "queued"),
        ];
        assert!(eligible(&jobs, &config(), &capacity()).is_empty());
    }
    #[test]
    fn busy_lane_does_not_block_another_workspace() {
        let jobs = vec![
            job(1, "a", 4, false, "running"),
            job(2, "a", 4, false, "queued"),
            job(3, "b", 4, false, "queued"),
        ];
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![3]);
    }
    #[test]
    fn exclusive_and_pressure_stop_admission() {
        let jobs = vec![
            job(1, "a", 4, true, "running"),
            job(2, "b", 4, false, "queued"),
        ];
        assert!(eligible(&jobs, &config(), &capacity()).is_empty());
        assert!(
            eligible(
                &[job(1, "a", 4, false, "queued")],
                &config(),
                &Capacity {
                    healthy: false,
                    ..capacity()
                }
            )
            .is_empty()
        );
    }
    #[test]
    fn released_run_memory_still_counts() {
        assert!(
            eligible(
                &[job(1, "a", 4, false, "queued")],
                &config(),
                &Capacity {
                    run_memory_mib: 1800,
                    ..capacity()
                }
            )
            .is_empty()
        );
    }
    #[test]
    fn native_preparation_forms_a_bounded_parallel_batch() {
        let first = job(1, "a", 4, false, "queued");
        let mut second = job(2, "b", 8, true, "queued");
        second.request.prepared = false;
        assert_eq!(
            eligible(&[first.clone(), second.clone()], &config(), &capacity()),
            vec![2]
        );
        second.request.prepared = true;
        second.request.budget.cpu = 4;
        second.request.budget.exclusive = false;
        assert_eq!(
            eligible(&[first.clone(), second], &config(), &capacity()),
            vec![1, 2]
        );
        let mut same_workspace = job(2, "new-lane", 8, true, "queued");
        same_workspace.request.prepared = false;
        same_workspace.request.workspace = first.request.workspace.clone();
        assert_eq!(
            eligible(&[first, same_workspace], &config(), &capacity()),
            vec![1]
        );
        let exclusive = job(1, "a", 8, true, "queued");
        let mut pending = job(2, "b", 8, true, "queued");
        pending.request.prepared = false;
        assert_eq!(
            eligible(&[exclusive, pending], &config(), &capacity()),
            vec![1]
        );
        let first = job(1, "same-final-lane", 4, false, "queued");
        let mut alias = job(2, "same-final-lane", 4, false, "queued");
        alias.request.workspace = "alias-workspace".into();
        let mut distinct = job(3, "different-lane", 8, true, "queued");
        distinct.request.prepared = false;
        assert_eq!(
            eligible(&[first, alias, distinct], &config(), &capacity()),
            vec![3]
        );
    }
}
