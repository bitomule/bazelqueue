#![forbid(unsafe_code)]
use crate::{
    config::Config,
    protocol::{Admission, Budget, Job, Request},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Normal,
    Constrained,
    Held,
}

pub fn admission_budget(config: &Config, pressure: Pressure) -> Budget {
    if pressure == Pressure::Constrained {
        config.constrained_budget()
    } else {
        Budget {
            cpu: config.cpu_capacity,
            memory_mib: config.memory_capacity_mib,
            exclusive: false,
            estimate: None,
        }
    }
}

pub fn fit_queued(budget: &mut Budget, config: &Config, pressure: Pressure) -> bool {
    if budget.exclusive {
        return false;
    }
    let before = budget.clone();
    let share = config.budget(true);
    if pressure == Pressure::Normal {
        if let Some(estimate) = budget.estimate {
            budget.cpu = estimate.cpu;
            budget.memory_mib = estimate.memory_mib;
        } else if budget.cpu < share.cpu {
            *budget = share;
        }
    }
    let limit = admission_budget(config, pressure);
    budget.cpu = budget.cpu.min(limit.cpu);
    budget.memory_mib = budget.memory_mib.min(limit.memory_mib);
    *budget != before
}

pub struct Capacity {
    pub pressure: Pressure,
    pub legacy_active: bool,
    pub drained: bool,
    pub run_memory_mib: u64,
    /// Fresh headroom after subtracting growth still promised to active work.
    pub new_memory_mib: u64,
    pub reason: Option<String>,
}

/// Reprofile only queued managed work. A grant is immutable, even when sensors change.
pub fn allocate_queued(jobs: &mut [Job], config: &Config, capacity: &Capacity) {
    let limit = admission_budget(config, capacity.pressure);
    // Opaque argv cannot be constrained. It owns the CPU lane exclusively and
    // accounts for all currently available memory, rather than demanding that
    // macOS and every other app relinquish their RAM before it may start.
    for job in jobs
        .iter_mut()
        .filter(|job| job.state == "queued" && job.metered() && job.request.budget.exclusive)
    {
        job.request.budget.cpu = config.cpu_capacity;
        job.request.budget.memory_mib = config
            .memory_capacity_mib
            .saturating_sub(capacity.run_memory_mib)
            .min(capacity.new_memory_mib)
            .max(256);
    }
    let active_cpu: u32 = jobs
        .iter()
        .filter(|job| job.holds_build_capacity())
        .map(|job| job.request.budget.cpu)
        .sum();
    let active_memory: u64 = jobs
        .iter()
        .filter(|job| job.holds_build_capacity())
        .map(|job| job.request.budget.memory_mib)
        .sum();
    let cpu = limit.cpu.saturating_sub(active_cpu);
    let memory = limit
        .memory_mib
        .saturating_sub(active_memory)
        .saturating_sub(capacity.run_memory_mib)
        .min(capacity.new_memory_mib);
    let lanes: std::collections::BTreeSet<_> = jobs
        .iter()
        .filter(|job| {
            job.state == "queued"
                && !job.cancel
                && job.metered()
                && !job.request.budget.exclusive
                && job.request.command != "exec"
        })
        .filter(|job| {
            !jobs.iter().any(|other| {
                other.holds_build_capacity() && other.request.shares_lane(&job.request)
            })
        })
        .map(|job| job.request.lane.clone())
        .collect();
    let count = lanes
        .len()
        .min(if capacity.pressure == Pressure::Constrained {
            1
        } else {
            config.max_builds
        })
        .min(cpu as usize)
        .min((memory / 256) as usize);
    if count == 0 {
        return;
    }
    let mut allocated = std::collections::BTreeSet::new();
    let selected: Vec<_> = jobs
        .iter()
        .enumerate()
        .filter(|(_, job)| {
            job.state == "queued"
                && !job.cancel
                && job.metered()
                && !job.request.budget.exclusive
                && job.request.command != "exec"
        })
        .filter(|(_, job)| {
            lanes.contains(&job.request.lane) && allocated.insert(job.request.lane.clone())
        })
        .take(count)
        .map(|(index, _)| index)
        .collect();
    let cpu_demands: Vec<_> = selected
        .iter()
        .map(|index| {
            u64::from(
                jobs[*index]
                    .request
                    .budget
                    .estimate
                    .map_or(limit.cpu, |estimate| estimate.cpu),
            )
            .max(1)
        })
        .collect();
    let memory_demands: Vec<_> = selected
        .iter()
        .map(|index| {
            jobs[*index]
                .request
                .budget
                .estimate
                .map_or(limit.memory_mib, |estimate| estimate.memory_mib)
                .max(256)
        })
        .collect();
    let cpus = fair_shares(u64::from(cpu), &cpu_demands);
    let memories = fair_shares(memory, &memory_demands);
    for (position, index) in selected.iter().enumerate() {
        jobs[*index].request.budget.cpu = cpus[position] as u32;
        jobs[*index].request.budget.memory_mib = memories[position];
    }
}

/// Satisfy small demands first, then divide the remainder among larger ones.
fn fair_shares(mut total: u64, demands: &[u64]) -> Vec<u64> {
    let mut result = vec![0; demands.len()];
    let mut pending: Vec<_> = (0..demands.len()).collect();
    while !pending.is_empty() {
        let share = total / pending.len() as u64;
        let small: Vec<_> = pending
            .iter()
            .copied()
            .filter(|index| demands[*index] <= share)
            .collect();
        if small.is_empty() {
            let mut remaining = pending.len() as u64;
            for index in pending {
                result[index] = total.div_ceil(remaining);
                total -= result[index];
                remaining -= 1;
            }
            break;
        }
        for index in small {
            result[index] = demands[index];
            total -= demands[index];
            pending.retain(|other| *other != index);
        }
    }
    result
}

pub fn eligible(jobs: &[Job], config: &Config, capacity: &Capacity) -> Vec<u64> {
    if capacity.drained {
        return Vec::new();
    }
    let mut chosen = Vec::new();
    let mut maintained: Vec<&Request> = jobs
        .iter()
        .filter(|job| job.holds_capacity() && job.request.admission() == Admission::Maintenance)
        .map(|job| &job.request)
        .collect();
    for job in jobs
        .iter()
        .filter(|job| job.state == "queued" && !job.cancel)
    {
        match job.request.admission() {
            Admission::Immediate => chosen.push(job.sequence),
            Admission::Maintenance
                if !jobs
                    .iter()
                    .filter(|other| other.holds_build_capacity())
                    .map(|other| &other.request)
                    .chain(maintained.iter().copied())
                    .any(|other| other.shares_lane(&job.request)) =>
            {
                maintained.push(&job.request);
                chosen.push(job.sequence);
            }
            _ => {}
        }
    }
    if capacity.pressure == Pressure::Held || capacity.legacy_active {
        return chosen;
    }
    let limit = admission_budget(config, capacity.pressure);
    let max_builds = if capacity.pressure == Pressure::Constrained {
        1
    } else {
        config.max_builds
    };
    let mut active: Vec<&Job> = jobs
        .iter()
        .filter(|job| job.holds_build_capacity())
        .collect();
    let mut cpu = active.iter().map(|job| job.request.budget.cpu).sum::<u32>();
    let mut memory = active
        .iter()
        .map(|job| job.request.budget.memory_mib)
        .sum::<u64>()
        .saturating_add(capacity.run_memory_mib);
    let mut fresh_memory = 0_u64;
    let mut waiting: Vec<_> = jobs
        .iter()
        .filter(|job| job.state == "queued" && !job.cancel && job.metered())
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
            || maintained
                .iter()
                .any(|other| other.shares_lane(&job.request))
        {
            continue;
        }
        let budget = &job.request.budget;
        if capacity.pressure == Pressure::Constrained && budget.exclusive {
            break;
        }
        let exclusive = active.iter().any(|job| job.request.budget.exclusive);
        let (cpu_limit, memory_limit) = if budget.exclusive {
            (config.cpu_capacity, config.memory_capacity_mib)
        } else {
            (limit.cpu, limit.memory_mib)
        };
        let slots = active
            .iter()
            .filter(|job| {
                capacity.pressure == Pressure::Constrained || job.request.budget.estimate.is_none()
            })
            .count();
        if exclusive
            || (budget.exclusive && !active.is_empty())
            || ((budget.estimate.is_none() || capacity.pressure == Pressure::Constrained)
                && slots >= max_builds)
            || cpu.saturating_add(budget.cpu) > cpu_limit
            || memory.saturating_add(budget.memory_mib) > memory_limit
            || fresh_memory.saturating_add(budget.memory_mib) > capacity.new_memory_mib
        {
            break;
        }
        cpu += budget.cpu;
        memory += budget.memory_mib;
        fresh_memory += budget.memory_mib;
        active.push(job);
        chosen.push(job.sequence);
    }
    chosen
}

pub fn reason<'a>(job: &Job, jobs: &[Job], capacity: &'a Capacity) -> &'a str {
    if capacity.drained {
        "queue is draining"
    } else if job.request.admission() == Admission::Maintenance {
        "workspace is busy"
    } else if capacity.legacy_active {
        "legacy Bazel invocations are draining"
    } else if let Some(reason) = &capacity.reason {
        reason
    } else if capacity.pressure == Pressure::Held {
        "memory sample unavailable"
    } else if jobs
        .iter()
        .any(|other| other.holds_capacity() && other.request.lane == job.request.lane)
    {
        "workspace is busy"
    } else if capacity.pressure == Pressure::Constrained
        && jobs.iter().any(|other| other.holds_build_capacity())
    {
        "reduced admission: one build at a time"
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
                    estimate: None,
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
            new_memory_mib: u64::MAX,
            reason: None,
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
    fn constrained_admission_runs_exactly_one_job_with_the_reduced_budget() {
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
            "reduced admission: one build at a time"
        );
    }
    #[test]
    fn constrained_admission_holds_opaque_work() {
        let mut unmanaged = job(1, "a", 0, true, "queued");
        unmanaged.request.budget = config().budget(false);
        assert!(!fit_queued(
            &mut unmanaged.request.budget,
            &config(),
            Pressure::Constrained
        ));
        let jobs = vec![unmanaged, managed(2, "b")];
        assert!(eligible(&jobs, &config(), &constrained()).is_empty());
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![1]);
    }
    #[test]
    fn missing_samples_hold_admission_and_say_so() {
        let jobs = vec![managed(1, "a")];
        let held = Capacity {
            pressure: Pressure::Held,
            ..capacity()
        };
        assert!(eligible(&jobs, &config(), &held).is_empty());
        assert_eq!(reason(&jobs[0], &jobs, &held), "memory sample unavailable");
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
    fn command(sequence: u64, lane: &str, command: &str, state: &str) -> Job {
        let mut job = job(sequence, lane, 1, false, state);
        job.request.command = command.into();
        job.request.budget = config().request_budget(job.request.admission(), false);
        job
    }
    #[test]
    fn read_only_commands_never_wait_for_build_capacity() {
        let jobs = vec![
            job(1, "a", 8, true, "running"),
            job(2, "b", 8, true, "queued"),
            command(3, "a", "info", "queued"),
            command(4, "c", "cquery", "queued"),
            command(5, "d", "version", "queued"),
        ];
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![3, 4, 5]);
        let held = Capacity {
            pressure: Pressure::Held,
            ..capacity()
        };
        assert_eq!(eligible(&jobs, &config(), &held), vec![3, 4, 5]);
        let drained = Capacity {
            drained: true,
            ..capacity()
        };
        assert!(eligible(&jobs, &config(), &drained).is_empty());
    }
    #[test]
    fn running_read_only_commands_do_not_occupy_build_slots() {
        let jobs = vec![
            command(1, "a", "query", "running"),
            command(2, "b", "info", "running"),
            managed(3, "a"),
            managed(4, "b"),
        ];
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![3, 4]);
    }
    #[test]
    fn clean_waits_only_for_its_own_workspace() {
        let mut unprepared = command(3, "a-workspace", "clean", "queued");
        unprepared.request.workspace = "a".into();
        unprepared.request.prepared = false;
        let jobs = vec![
            job(1, "a", 8, true, "running"),
            command(2, "b", "clean", "queued"),
            unprepared,
            managed(4, "b"),
            managed(5, "c"),
        ];
        assert!(!jobs[1].request.budget.exclusive);
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![2]);
        assert_eq!(reason(&jobs[2], &jobs, &capacity()), "workspace is busy");
        let mut idle = jobs.clone();
        idle[0].state = "finished".into();
        assert_eq!(eligible(&idle, &config(), &capacity()), vec![2, 3, 5]);
        let mut cleaning = idle;
        cleaning[1].state = "running".into();
        cleaning[2].state = "running".into();
        assert_eq!(eligible(&cleaning, &config(), &capacity()), vec![5]);
    }
    fn whole_machine() -> Config {
        Config {
            cpu_capacity: 8,
            memory_capacity_mib: 9216,
            max_builds: 1,
            action_memory_mib: 6144,
            ..Config::default()
        }
    }
    fn estimated(sequence: u64, lane: &str, cpu: u32, state: &str) -> Job {
        let mut job = job(sequence, lane, cpu, false, state);
        job.request.command = "test".into();
        job.request.budget.memory_mib = 9216 * u64::from(cpu) / 8;
        job.request.budget.estimate = Some(crate::protocol::Estimate {
            cpu,
            memory_mib: job.request.budget.memory_mib,
        });
        job
    }
    #[test]
    fn small_estimated_runs_share_the_machine_even_with_one_build_slot() {
        let jobs: Vec<_> = (1..=5)
            .map(|sequence| estimated(sequence, &format!("w{sequence}"), 2, "queued"))
            .collect();
        assert_eq!(
            eligible(&jobs, &whole_machine(), &capacity()),
            vec![1, 2, 3, 4]
        );
        let mut unestimated = job(1, "a", 8, false, "queued");
        unestimated.request.budget.memory_mib = 9216;
        let mut second = unestimated.clone();
        second.sequence = 2;
        second.request.lane = "b".into();
        second.request.workspace = "b".into();
        assert_eq!(
            eligible(&[unestimated, second], &whole_machine(), &capacity()),
            vec![1]
        );
    }
    #[test]
    fn a_large_run_ahead_of_small_ones_is_not_overtaken() {
        let mut large = job(2, "b", 8, false, "queued");
        large.request.budget.memory_mib = 9216;
        let jobs = vec![
            estimated(1, "a", 2, "running"),
            large,
            estimated(3, "c", 2, "queued"),
        ];
        assert!(eligible(&jobs, &whole_machine(), &capacity()).is_empty());
    }
    #[test]
    fn memory_pressure_still_admits_one_reduced_run_at_a_time() {
        let jobs = vec![
            estimated(1, "a", 2, "queued"),
            estimated(2, "b", 2, "queued"),
        ];
        let constrained = Capacity {
            pressure: Pressure::Constrained,
            ..capacity()
        };
        assert_eq!(eligible(&jobs, &whole_machine(), &constrained), vec![1]);
    }
    #[test]
    fn an_estimate_survives_pressure_and_is_not_widened() {
        let config = whole_machine();
        let mut budget = estimated(1, "a", 3, "queued").request.budget;
        assert!(fit_queued(&mut budget, &config, Pressure::Constrained));
        assert_eq!((budget.cpu, budget.memory_mib), (2, 2304));
        assert!(fit_queued(&mut budget, &config, Pressure::Normal));
        assert_eq!((budget.cpu, budget.memory_mib), (3, 3456));
    }

    #[test]
    fn full_machine_estimates_share_actual_headroom_and_every_cpu_token() {
        let config = Config {
            cpu_capacity: 11,
            max_builds: 2,
            ..whole_machine()
        };
        let mut jobs = vec![
            estimated(1, "a", 11, "queued"),
            estimated(2, "b", 11, "queued"),
        ];
        let capacity = Capacity {
            new_memory_mib: 5000,
            ..capacity()
        };
        allocate_queued(&mut jobs, &config, &capacity);
        assert_eq!(eligible(&jobs, &config, &capacity), vec![1, 2]);
        assert_eq!(
            jobs.iter().map(|job| job.request.budget.cpu).sum::<u32>(),
            11
        );
        assert_eq!(
            jobs.iter()
                .map(|job| job.request.budget.memory_mib)
                .sum::<u64>(),
            5000
        );
        jobs[0].state = "running".into();
        let granted = jobs[0].request.budget.clone();
        allocate_queued(&mut jobs, &config, &constrained());
        assert_eq!(jobs[0].request.budget, granted);
    }

    #[test]
    fn unmaterialized_reservations_are_subtracted_but_resident_memory_is_not_counted_twice() {
        let mut active = estimated(1, "a", 2, "running");
        active.request.budget.memory_mib = 2048;
        let mut jobs = vec![
            active,
            estimated(2, "b", 8, "queued"),
            estimated(3, "c", 8, "queued"),
        ];
        let resident = std::collections::HashMap::from([("1".to_owned(), 1024)]);
        assert_eq!(crate::resources::promised_growth(&jobs, &resident), 1024);
        let capacity = Capacity {
            new_memory_mib: 4096 - crate::resources::promised_growth(&jobs, &resident),
            ..capacity()
        };
        let config = Config {
            max_builds: 2,
            ..whole_machine()
        };
        allocate_queued(&mut jobs, &config, &capacity);
        assert_eq!(eligible(&jobs, &config, &capacity), vec![2, 3]);
        assert_eq!(
            jobs[1].request.budget.memory_mib + jobs[2].request.budget.memory_mib,
            3072
        );
        let none = Capacity {
            new_memory_mib: 255,
            ..capacity
        };
        assert!(eligible(&jobs, &config, &none).is_empty());
    }
    #[test]
    fn a_small_estimate_leaves_its_unused_share_to_a_large_one() {
        let config = Config {
            cpu_capacity: 11,
            memory_capacity_mib: 10000,
            max_builds: 2,
            ..whole_machine()
        };
        let mut jobs = vec![
            estimated(1, "a", 11, "queued"),
            estimated(2, "b", 1, "queued"),
        ];
        jobs[0].request.budget.estimate.as_mut().unwrap().memory_mib = 10000;
        jobs[1].request.budget.estimate.as_mut().unwrap().memory_mib = 256;
        allocate_queued(
            &mut jobs,
            &config,
            &Capacity {
                new_memory_mib: 10000,
                ..capacity()
            },
        );
        assert_eq!(
            jobs.iter().map(|job| job.request.budget.cpu).sum::<u32>(),
            11
        );
        assert_eq!(
            jobs.iter()
                .map(|job| job.request.budget.memory_mib)
                .sum::<u64>(),
            10000
        );
        assert_eq!(
            (
                jobs[1].request.budget.cpu,
                jobs[1].request.budget.memory_mib
            ),
            (1, 256)
        );
    }
    #[test]
    fn an_opaque_invocation_can_start_with_other_applications_using_memory() {
        let config = whole_machine();
        let mut opaque = job(1, "a", 8, true, "queued");
        opaque.request.budget = config.budget(false);
        let mut jobs = vec![opaque];
        let capacity = Capacity {
            new_memory_mib: 4096,
            ..capacity()
        };
        allocate_queued(&mut jobs, &config, &capacity);
        assert_eq!(eligible(&jobs, &config, &capacity), vec![1]);
        assert!(jobs[0].request.budget.exclusive);
        assert_eq!(jobs[0].request.budget.cpu, config.cpu_capacity);
    }
    #[test]
    fn opaque_work_fits_beside_a_live_run_target_without_exceeding_global_memory() {
        let config = whole_machine();
        let mut jobs = vec![job(1, "a", 8, true, "queued")];
        jobs[0].request.budget = config.budget(false);
        let capacity = Capacity {
            new_memory_mib: 9216,
            run_memory_mib: 1024,
            ..capacity()
        };
        allocate_queued(&mut jobs, &config, &capacity);
        assert_eq!(eligible(&jobs, &config, &capacity), vec![1]);
        assert_eq!(jobs[0].request.budget.memory_mib, 8192);
        let full = Capacity {
            run_memory_mib: config.memory_capacity_mib,
            ..capacity
        };
        allocate_queued(&mut jobs, &config, &full);
        assert!(jobs[0].request.budget.memory_mib >= 256);
        assert!(eligible(&jobs, &config, &full).is_empty());
    }
}
