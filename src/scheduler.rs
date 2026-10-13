#![forbid(unsafe_code)]
use crate::{
    config::Config,
    protocol::{Budget, Job},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Normal,
    Constrained,
    Held,
}

impl Pressure {
    pub fn from_samples(
        level: Option<u32>,
        last_known: Option<u32>,
        healthy_samples: u32,
        recovery_samples: u32,
    ) -> Self {
        match level {
            Some(1) if healthy_samples >= recovery_samples => Self::Normal,
            Some(1 | 2) => Self::Constrained,
            Some(_) => Self::Held,
            None if matches!(last_known, None | Some(1 | 2)) => Self::Constrained,
            None => Self::Held,
        }
    }
}

pub fn admission_budget(config: &Config, pressure: Pressure) -> Budget {
    if pressure == Pressure::Constrained {
        config.constrained_budget()
    } else {
        Budget {
            cpu: config.cpu_capacity,
            memory_mib: config.memory_capacity_mib,
            exclusive: false,
        }
    }
}

pub fn fit_queued(budget: &mut Budget, config: &Config, pressure: Pressure) -> bool {
    if budget.exclusive {
        return false;
    }
    let before = budget.clone();
    let share = config.budget(true);
    if pressure == Pressure::Normal && budget.cpu < share.cpu {
        *budget = share;
    }
    let limit = admission_budget(config, pressure);
    budget.cpu = budget.cpu.min(limit.cpu);
    budget.memory_mib = budget.memory_mib.min(limit.memory_mib);
    *budget != before
}

pub fn raise_lone_job<'a>(
    candidates: &[Job],
    jobs: &'a mut [Job],
    limit: &Budget,
    remaining_memory: u64,
) -> Option<&'a mut Job> {
    let raisable = |job: &Job| {
        job.state == "queued"
            && job.request.prepared
            && !job.request.budget.exclusive
            && job.request.command != "exec"
    };
    if remaining_memory < 256
        || candidates.iter().filter(|job| raisable(job)).count() != 1
        || jobs
            .iter()
            .filter(|job| job.state == "queued" && !job.cancel)
            .count()
            != 1
        || candidates.iter().any(|job| job.holds_capacity())
    {
        return None;
    }
    let job = jobs.iter_mut().find(|job| raisable(job))?;
    job.request.budget.cpu = limit.cpu;
    job.request.budget.memory_mib = remaining_memory;
    Some(job)
}

pub struct Capacity {
    pub pressure: Pressure,
    pub legacy_active: bool,
    pub drained: bool,
    pub run_memory_mib: u64,
}

pub fn eligible(jobs: &[Job], config: &Config, capacity: &Capacity) -> Vec<u64> {
    if capacity.pressure == Pressure::Held || capacity.legacy_active || capacity.drained {
        return Vec::new();
    }
    let limit = admission_budget(config, capacity.pressure);
    let max_builds = if capacity.pressure == Pressure::Constrained {
        1
    } else {
        config.max_builds
    };
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
        && ready_lanes < max_builds
        && ready.len() < max_builds.saturating_mul(2)
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
        let (cpu_limit, memory_limit) = if budget.exclusive {
            (config.cpu_capacity, config.memory_capacity_mib)
        } else {
            (limit.cpu, limit.memory_mib)
        };
        if exclusive
            || (budget.exclusive && !active.is_empty())
            || active.len() >= max_builds
            || cpu.saturating_add(budget.cpu) > cpu_limit
            || memory.saturating_add(budget.memory_mib) > memory_limit
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
    } else if capacity.pressure == Pressure::Held {
        "memory pressure (critical)"
    } else if jobs
        .iter()
        .any(|other| other.holds_capacity() && other.request.lane == job.request.lane)
    {
        "workspace is busy"
    } else if capacity.pressure == Pressure::Constrained
        && jobs.iter().any(|other| other.holds_capacity())
    {
        "memory pressure (warning: one reduced build at a time)"
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
            pressure: Pressure::Normal,
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
                    pressure: Pressure::Held,
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
    fn constrained() -> Capacity {
        Capacity {
            pressure: Pressure::Constrained,
            ..capacity()
        }
    }
    fn managed(sequence: u64, lane: &str) -> Job {
        let mut job = job(sequence, lane, 0, false, "queued");
        job.request.budget = config().budget(true);
        job
    }
    #[test]
    fn pressure_mode_follows_samples_and_hysteresis() {
        assert_eq!(
            Pressure::from_samples(Some(1), Some(1), 3, 3),
            Pressure::Normal
        );
        assert_eq!(
            Pressure::from_samples(Some(1), Some(2), 2, 3),
            Pressure::Constrained
        );
        assert_eq!(
            Pressure::from_samples(Some(2), Some(1), 0, 3),
            Pressure::Constrained
        );
        assert_eq!(
            Pressure::from_samples(Some(4), Some(2), 0, 3),
            Pressure::Held
        );
        assert_eq!(Pressure::from_samples(None, Some(4), 0, 3), Pressure::Held);
        assert_eq!(
            Pressure::from_samples(None, Some(2), 0, 3),
            Pressure::Constrained
        );
    }
    #[test]
    fn warning_admits_exactly_one_job_with_the_reduced_budget() {
        let mut jobs = vec![managed(1, "a"), managed(2, "b")];
        for job in &mut jobs {
            assert!(fit_queued(
                &mut job.request.budget,
                &config(),
                Pressure::Constrained
            ));
            assert_eq!(job.request.budget, config().constrained_budget());
        }
        assert_eq!(config().constrained_budget().cpu, 2);
        assert_eq!(config().constrained_budget().memory_mib, 1024);
        assert_eq!(eligible(&jobs, &config(), &constrained()), vec![1]);
        jobs[0].state = "running".into();
        assert!(eligible(&jobs, &config(), &constrained()).is_empty());
        assert_eq!(
            reason(&jobs[1], &jobs, &constrained()),
            "memory pressure (warning: one reduced build at a time)"
        );
    }
    #[test]
    fn warning_runs_an_unmanaged_job_alone_with_its_full_budget() {
        let mut unmanaged = job(1, "a", 0, true, "queued");
        unmanaged.request.budget = config().budget(false);
        assert!(!fit_queued(
            &mut unmanaged.request.budget,
            &config(),
            Pressure::Constrained
        ));
        let jobs = vec![unmanaged, managed(2, "b")];
        assert_eq!(eligible(&jobs, &config(), &constrained()), vec![1]);
    }
    #[test]
    fn critical_admits_nothing_and_says_so() {
        let jobs = vec![managed(1, "a")];
        let held = Capacity {
            pressure: Pressure::Held,
            ..capacity()
        };
        assert!(eligible(&jobs, &config(), &held).is_empty());
        assert_eq!(reason(&jobs[0], &jobs, &held), "memory pressure (critical)");
    }
    #[test]
    fn recovery_restores_full_capacity_admission() {
        let mut jobs = vec![managed(1, "a"), managed(2, "b")];
        for job in &mut jobs {
            fit_queued(&mut job.request.budget, &config(), Pressure::Constrained);
        }
        for job in &mut jobs {
            assert!(fit_queued(
                &mut job.request.budget,
                &config(),
                Pressure::Normal
            ));
            assert_eq!(job.request.budget, config().budget(true));
        }
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![1, 2]);
    }
    #[test]
    fn lone_job_raise_respects_the_constraint() {
        let mut jobs = vec![managed(1, "a")];
        let candidates = jobs.clone();
        let limit = admission_budget(&config(), Pressure::Constrained);
        let raised = raise_lone_job(&candidates, &mut jobs, &limit, limit.memory_mib).unwrap();
        assert_eq!(raised.request.budget.cpu, 2);
        assert_eq!(raised.request.budget.memory_mib, 1024);
        let limit = admission_budget(&config(), Pressure::Normal);
        let raised = raise_lone_job(&candidates, &mut jobs, &limit, limit.memory_mib).unwrap();
        assert_eq!(raised.request.budget.cpu, 8);
        assert_eq!(raised.request.budget.memory_mib, 2048);
    }
}
