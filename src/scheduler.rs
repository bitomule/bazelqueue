#![forbid(unsafe_code)]
use crate::{
    config::Config,
    protocol::{Admission, Budget, FastLane, Job, Request, WaitKind},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    Normal,
    /// New builds go on at half size.
    Overloaded,
    /// One reduced build, and only with nothing else of ours running.
    Constrained,
    Held,
}

/// How many later jobs may start ahead of one that does not fit. Bazel gives no
/// reliable durations to reserve its start with, so the bound is a count.
const MAX_PASSES: u32 = 4;

pub fn admission_budget(config: &Config, pressure: Pressure) -> Budget {
    match pressure {
        Pressure::Constrained => config.constrained_budget(),
        Pressure::Overloaded => config.overload_budget(),
        Pressure::Normal | Pressure::Held => Budget {
            cpu: config.cpu_capacity,
            memory_mib: config.memory_capacity_mib,
            exclusive: false,
            estimate: None,
        },
    }
}

/// What a new build may take. Under overload a start only needs the queue below half
/// size, and then gets at least what its workers need.
fn free_cpu(config: &Config, pressure: Pressure, active_cpu: u32) -> u32 {
    let limit = admission_budget(config, pressure).cpu;
    if pressure == Pressure::Overloaded && active_cpu < limit {
        (limit - active_cpu).max(config.fast_budget().cpu)
    } else {
        limit.saturating_sub(active_cpu)
    }
}

/// The order queued metered work is served in.
fn waiting_order(jobs: &[Job]) -> Vec<usize> {
    (0..jobs.len())
        .filter(|index| {
            let job = &jobs[*index];
            job.state == "queued" && !job.cancel && job.metered()
        })
        .collect()
}

pub fn fit_queued(budget: &mut Budget, config: &Config, pressure: Pressure) -> bool {
    if budget.exclusive {
        return false;
    }
    let before = budget.clone();
    let share = config.budget(true);
    if matches!(pressure, Pressure::Normal | Pressure::Overloaded) {
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
    /// Measured memory of each job, server and children included.
    pub resident_mib: std::collections::HashMap<String, u64>,
    pub reason: Option<String>,
}

impl Capacity {
    /// A reservation is a promise, what the job really uses can already exceed it.
    fn occupied_mib(&self, job: &Job) -> u64 {
        job.request
            .budget
            .memory_mib
            .max(self.resident_mib.get(&job.request.id).copied().unwrap_or(0))
    }

    /// What a start takes from free memory: a live server is already out of it.
    fn fresh_mib(&self, job: &Job) -> u64 {
        job.request
            .budget
            .memory_mib
            .saturating_sub(self.resident_mib.get(&job.request.id).copied().unwrap_or(0))
    }
}

/// Waiting for memory beats starting on a reservation smaller than the job's own server.
pub fn raise_to_measured(
    jobs: &mut [Job],
    config: &Config,
    capacity: &Capacity,
    measured: impl Fn(&str) -> Option<u64>,
) {
    let limit = admission_budget(config, capacity.pressure)
        .memory_mib
        .saturating_sub(capacity.run_memory_mib);
    let unmeasured = config
        .fast_budget()
        .memory_mib
        .min(config.budget(true).memory_mib);
    for job in jobs.iter_mut().filter(|job| {
        job.state == "queued"
            && job.metered()
            && !job.request.budget.exclusive
            && job.request.command != "exec"
    }) {
        let floor = measured(&job.request.lane)
            .map_or(unmeasured, |peak| peak.max(unmeasured))
            .min(limit);
        job.request.budget.memory_mib = job.request.budget.memory_mib.max(floor);
    }
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
        .map(|job| capacity.occupied_mib(job))
        .sum();
    let cpu = free_cpu(config, capacity.pressure, active_cpu);
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
                other.holds_metered_capacity() && other.request.shares_lane(&job.request)
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
    let selected: Vec<_> = waiting_order(jobs)
        .into_iter()
        .filter(|index| {
            let job = &jobs[*index];
            !job.request.budget.exclusive
                && job.request.command != "exec"
                && lanes.contains(&job.request.lane)
                && allocated.insert(job.request.lane.clone())
        })
        .take(count)
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

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Grants {
    pub chosen: Vec<u64>,
    /// One entry per job that started ahead of this one.
    pub overtaken: Vec<u64>,
}

pub fn eligible(jobs: &[Job], config: &Config, capacity: &Capacity) -> Vec<u64> {
    grants(jobs, config, capacity).chosen
}

pub fn grants(jobs: &[Job], config: &Config, capacity: &Capacity) -> Grants {
    if capacity.drained {
        return Grants::default();
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
                    .filter(|other| other.holds_metered_capacity())
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
        return Grants {
            chosen,
            overtaken: Vec::new(),
        };
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
        .map(|job| capacity.occupied_mib(job))
        .sum::<u64>()
        .saturating_add(capacity.run_memory_mib);
    let mut fresh_memory = 0_u64;
    // Free memory is a cautious figure: with nothing of ours running, one build
    // may start on it, or big apps would stop the queue for good.
    let idle = !jobs.iter().any(|job| job.holds_metered_capacity());
    let mut waiting: Vec<_> = waiting_order(jobs)
        .into_iter()
        .map(|index| &jobs[index])
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
    let mut skipped = Vec::new();
    let mut overtaken = Vec::new();
    let mut passes = MAX_PASSES;
    for job in waiting {
        if capacity.pressure == Pressure::Constrained && !(idle && active.is_empty()) {
            break;
        }
        if active
            .iter()
            .any(|other| other.request.lane == job.request.lane)
            || jobs
                .iter()
                .any(|other| other.holds_fast_capacity() && other.request.shares_lane(&job.request))
            || maintained
                .iter()
                .any(|other| other.shares_lane(&job.request))
        {
            continue;
        }
        let budget = &job.request.budget;
        // Opaque work cannot be reduced, so it simply does not start here.
        if capacity.pressure == Pressure::Constrained && budget.exclusive {
            continue;
        }
        let exclusive = active.iter().any(|job| job.request.budget.exclusive);
        let (free, memory_limit) = if budget.exclusive {
            (
                config.cpu_capacity.saturating_sub(cpu),
                config.memory_capacity_mib,
            )
        } else {
            (free_cpu(config, capacity.pressure, cpu), limit.memory_mib)
        };
        let slots = active
            .iter()
            .filter(|job| job.request.budget.estimate.is_none())
            .count();
        if exclusive
            || (budget.exclusive && !active.is_empty())
            || (budget.estimate.is_none() && slots >= max_builds)
            || budget.cpu > free
            || memory.saturating_add(budget.memory_mib) > memory_limit
            || (fresh_memory.saturating_add(capacity.fresh_mib(job)) > capacity.new_memory_mib
                && !(idle && active.is_empty()))
        {
            passes = passes.min(MAX_PASSES.saturating_sub(job.passed));
            if passes == 0 {
                break;
            }
            skipped.push(job.sequence);
            continue;
        }
        cpu += budget.cpu;
        memory += budget.memory_mib;
        fresh_memory += capacity.fresh_mib(job);
        active.push(job);
        chosen.push(job.sequence);
        if !skipped.is_empty() {
            overtaken.extend_from_slice(&skipped);
            passes -= 1;
            if passes == 0 {
                break;
            }
        }
    }
    Grants { chosen, overtaken }
}

/// Queued fast-lane requests that may be checked now, ahead of the build queue.
/// A check runs only where Bazel has built before: a first invocation in a
/// workspace never passed one in the measured history.
pub fn fast_lane(jobs: &[Job], config: &Config, capacity: &Capacity) -> Vec<u64> {
    if capacity.drained || capacity.legacy_active || capacity.pressure != Pressure::Normal {
        return Vec::new();
    }
    let memory = config.fast_budget().memory_mib;
    let mut slots = jobs.iter().filter(|job| job.holds_fast_capacity()).count();
    let mut fresh_memory = 0_u64;
    let mut chosen: Vec<&Job> = Vec::new();
    for job in jobs.iter().filter(|job| {
        job.state == "queued"
            && !job.cancel
            && job.request.fast_lane
            && !job.request.prepared
            && job.fast == FastLane::Unchecked
    }) {
        if slots >= config.fast_lane_slots as usize
            || fresh_memory.saturating_add(memory) > capacity.new_memory_mib
        {
            break;
        }
        let busy = jobs
            .iter()
            .filter(|other| {
                other.holds_capacity()
                    || (other.state == "queued" && !other.cancel && other.sequence < job.sequence)
            })
            .chain(chosen.iter().copied())
            .any(|other| other.request.shares_lane(&job.request));
        let built_before = jobs.iter().any(|other| {
            other.sequence < job.sequence
                && other.request.workspace == job.request.workspace
                && matches!(other.request.command.as_str(), "build" | "test")
                && other.state == "finished"
                && other.code == Some(0)
        });
        if busy || !built_before {
            continue;
        }
        slots += 1;
        fresh_memory += memory;
        chosen.push(job);
    }
    chosen.iter().map(|job| job.sequence).collect()
}

pub fn reason<'a>(job: &Job, jobs: &[Job], capacity: &'a Capacity) -> &'a str {
    waiting(job, jobs, capacity).1
}

pub fn waiting<'a>(job: &Job, jobs: &[Job], capacity: &'a Capacity) -> (WaitKind, &'a str) {
    if capacity.drained {
        (WaitKind::Draining, "queue is draining")
    } else if job.request.admission() == Admission::Maintenance {
        (WaitKind::Maintenance, "workspace is busy")
    } else if capacity.legacy_active {
        (
            WaitKind::LegacyDraining,
            "legacy Bazel invocations are draining",
        )
    } else if capacity.pressure == Pressure::Held {
        (
            WaitKind::MemoryHeld,
            capacity
                .reason
                .as_deref()
                .unwrap_or("memory sample unavailable"),
        )
    } else if jobs
        .iter()
        .any(|other| other.holds_capacity() && other.request.lane == job.request.lane)
    {
        (WaitKind::WorkspaceBusy, "workspace is busy")
    } else if matches!(
        capacity.pressure,
        Pressure::Constrained | Pressure::Overloaded
    ) && jobs.iter().any(|other| other.holds_metered_capacity())
    {
        (
            WaitKind::ReducedAdmission,
            capacity.reason.as_deref().unwrap_or("reduced admission"),
        )
    } else if capacity.fresh_mib(job) > capacity.new_memory_mib {
        (WaitKind::WaitingForMemory, "waiting for free memory")
    } else {
        (
            WaitKind::Capacity,
            capacity.reason.as_deref().unwrap_or("resource capacity"),
        )
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
                fast_lane: false,
            },
            state: state.into(),
            child: None,
            server: None,
            code: None,
            cancel: false,
            reason: None,
            retiring_server: None,
            fast: Default::default(),
            passed: 0,
            progress: None,
            stall: None,
            queued_at_ms: None,
            wait: None,
        }
    }
    fn capacity() -> Capacity {
        Capacity {
            pressure: Pressure::Normal,
            legacy_active: false,
            drained: false,
            run_memory_mib: 0,
            new_memory_mib: u64::MAX,
            resident_mib: Default::default(),
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
    fn a_small_job_passes_a_large_one_only_a_bounded_number_of_times() {
        let mut jobs = vec![
            job(1, "a", 4, false, "running"),
            job(2, "b", 8, false, "queued"),
            job(3, "c", 1, false, "queued"),
        ];
        assert_eq!(
            grants(&jobs, &config(), &capacity()),
            Grants {
                chosen: vec![3],
                overtaken: vec![2],
            }
        );
        jobs[1].passed = MAX_PASSES;
        assert_eq!(grants(&jobs, &config(), &capacity()), Grants::default());
    }
    #[test]
    fn one_round_cannot_spend_more_passes_than_the_job_has_left() {
        let mut jobs = vec![
            job(1, "a", 1, false, "running"),
            job(2, "b", 8, false, "queued"),
            job(3, "c", 1, false, "queued"),
            job(4, "d", 1, false, "queued"),
        ];
        jobs[1].passed = MAX_PASSES - 1;
        let mut roomy = config();
        roomy.max_builds = 4;
        assert_eq!(
            grants(&jobs, &roomy, &capacity()),
            Grants {
                chosen: vec![3],
                overtaken: vec![2],
            }
        );
    }
    #[test]
    fn a_job_whose_workspace_is_busy_is_not_passed_and_stops_nobody() {
        let mut jobs = vec![
            job(1, "a", 1, false, "running"),
            job(2, "a", 1, false, "queued"),
            job(3, "b", 1, false, "queued"),
        ];
        jobs[1].passed = MAX_PASSES;
        assert_eq!(
            grants(&jobs, &config(), &capacity()),
            Grants {
                chosen: vec![3],
                overtaken: Vec::new(),
            }
        );
    }
    #[test]
    fn opaque_work_is_passed_while_builds_run_and_then_gets_the_machine() {
        let mut opaque = job(2, "b", 8, true, "queued");
        opaque.request.budget = config().budget(false);
        let mut jobs = vec![
            job(1, "a", 1, false, "running"),
            opaque,
            job(3, "c", 1, false, "queued"),
        ];
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![3]);
        jobs[1].passed = MAX_PASSES;
        assert!(eligible(&jobs, &config(), &capacity()).is_empty());
        jobs[0].state = "finished".into();
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![2]);
    }
    fn overloaded() -> Capacity {
        Capacity {
            pressure: Pressure::Overloaded,
            reason: Some("CPU overload".into()),
            ..capacity()
        }
    }
    #[test]
    fn overload_runs_two_small_builds_or_one_large_one() {
        let config = incident_config();
        let mut small = vec![
            estimated(1, "a", 2, "queued"),
            estimated(2, "b", 2, "queued"),
            estimated(3, "c", 2, "queued"),
        ];
        assert_eq!(eligible(&small, &config, &overloaded()), vec![1, 2]);
        small[0].state = "running".into();
        small[1].state = "running".into();
        assert!(eligible(&small, &config, &overloaded()).is_empty());
        assert_eq!(reason(&small[2], &small, &overloaded()), "CPU overload");

        let mut large = estimated(1, "a", 6, "queued").request.budget;
        fit_queued(&mut large, &config, Pressure::Overloaded);
        assert_eq!(large.cpu, 3);
        let mut jobs = vec![
            estimated(1, "a", 3, "running"),
            estimated(2, "b", 2, "queued"),
        ];
        jobs[0].request.budget = large;
        assert!(eligible(&jobs, &config, &overloaded()).is_empty());
    }
    #[test]
    fn overload_never_starts_a_build_below_what_its_workers_need() {
        let config = incident_config();
        let mut jobs = vec![
            estimated(1, "a", 2, "running"),
            estimated(2, "b", 6, "queued"),
        ];
        allocate_queued(&mut jobs, &config, &overloaded());
        assert_eq!(jobs[1].request.budget.cpu, 2);
        assert_eq!(eligible(&jobs, &config, &overloaded()), vec![2]);
    }
    #[test]
    fn overload_lets_opaque_work_start_on_an_idle_queue() {
        let mut opaque = job(1, "a", 6, true, "queued");
        opaque.request.budget = incident_config().budget(false);
        assert_eq!(
            eligible(&[opaque], &incident_config(), &overloaded()),
            vec![1]
        );
    }
    #[test]
    fn a_reservation_made_under_memory_trouble_grows_back_under_overload() {
        let config = incident_config();
        let mut budget = estimated(1, "a", 2, "queued").request.budget;
        fit_queued(&mut budget, &config, Pressure::Constrained);
        assert_eq!(budget.cpu, 1);
        fit_queued(&mut budget, &config, Pressure::Overloaded);
        assert_eq!(budget.cpu, 2);
    }
    fn incident_config() -> Config {
        Config {
            cpu_capacity: 6,
            memory_capacity_mib: 8192,
            max_builds: 1,
            action_memory_mib: 6144,
            worker_instances: 2,
            ..Config::default()
        }
    }
    /// Estimated CPU and tenths of a minute really run, from the jobs of 6 November 2026.
    const INCIDENT: [(Option<u32>, u32); 23] = [
        (Some(3), 3),
        (None, 2),
        (Some(2), 37),
        (Some(6), 476),
        (Some(3), 91),
        (Some(6), 3),
        (Some(3), 1),
        (Some(6), 32),
        (Some(3), 215),
        (Some(2), 13),
        (Some(2), 235),
        (None, 1),
        (Some(2), 10),
        (None, 7),
        (Some(6), 3),
        (Some(2), 45),
        (Some(2), 15),
        (Some(6), 212),
        (Some(6), 22),
        (Some(6), 50),
        (Some(3), 1),
        (Some(6), 6),
        (Some(4), 9),
    ];
    /// The 142 minutes job 15947 waited, one step per tenth of a minute.
    const INCIDENT_STEPS: u32 = 1420;
    #[test]
    fn the_queue_of_the_6_november_incident_keeps_moving_under_cpu_overload() {
        use crate::resources::{Monitor, Sample};
        let config = incident_config();
        let mut monitor = Monitor::default();
        for at_ms in [0, 1000] {
            monitor.update(
                Sample {
                    at_ms,
                    pressure: Some(2),
                    total_mib: 18432,
                    available_mib: Some(7557),
                    swap_in_bytes: Some(0),
                    swap_out_bytes: Some(0),
                    swap_used_mib: Some(0),
                    disk_free_mib: Some(1 << 20),
                    load_per_cpu: Some(5.0),
                    ..Sample::default()
                },
                &config,
            );
        }
        let mut jobs: Vec<_> = INCIDENT
            .iter()
            .enumerate()
            .map(|(index, (cpu, _))| {
                let lane = format!("lane-{index}");
                let sequence = index as u64 + 1;
                match cpu {
                    Some(cpu) => estimated(sequence, &lane, *cpu, "queued"),
                    None => managed(sequence, &lane),
                }
            })
            .collect();
        let mut left: Vec<_> = INCIDENT.iter().map(|(_, tenths)| *tenths).collect();
        let mut idle_steps = 0;
        for _ in 0..INCIDENT_STEPS {
            let capacity = Capacity {
                pressure: monitor.mode(),
                new_memory_mib: monitor.headroom(&config),
                reason: monitor.signals.reason.clone(),
                ..capacity()
            };
            for job in jobs.iter_mut().filter(|job| job.state == "queued") {
                fit_queued(&mut job.request.budget, &config, capacity.pressure);
            }
            allocate_queued(&mut jobs, &config, &capacity);
            let granted = grants(&jobs, &config, &capacity);
            for job in &mut jobs {
                job.passed += granted
                    .overtaken
                    .iter()
                    .filter(|sequence| **sequence == job.sequence)
                    .count() as u32;
                if granted.chosen.contains(&job.sequence) {
                    job.state = "running".into();
                }
            }
            let reserved: u32 = jobs
                .iter()
                .filter(|job| job.state == "running")
                .map(|job| job.request.budget.cpu)
                .sum();
            assert!(reserved <= 4, "{reserved} CPU reserved under overload");
            let waiting = jobs.iter().any(|job| job.state == "queued");
            if reserved == 0 && waiting {
                idle_steps += 1;
            }
            for (job, left) in jobs.iter_mut().zip(&mut left) {
                if job.state == "running" {
                    *left -= 1;
                    if *left == 0 {
                        job.state = "finished".into();
                    }
                }
            }
        }
        let finished = jobs.iter().filter(|job| job.state == "finished").count();
        assert_eq!(idle_steps, 0, "the queue sat empty with jobs waiting");
        assert_eq!(finished, INCIDENT.len(), "10 finished in the incident");
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
    #[test]
    fn a_server_bigger_than_its_reservation_takes_what_it_really_uses() {
        let jobs = vec![
            job(1, "a", 1, false, "running"),
            job(2, "b", 1, false, "queued"),
        ];
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![2]);
        let measured = Capacity {
            resident_mib: [("1".to_owned(), 1800)].into(),
            ..capacity()
        };
        assert!(eligible(&jobs, &config(), &measured).is_empty());
    }
    #[test]
    fn a_new_share_leaves_out_what_a_running_server_really_uses() {
        let mut jobs = vec![
            job(1, "a", 1, false, "running"),
            job(2, "b", 1, false, "queued"),
        ];
        let measured = Capacity {
            resident_mib: [("1".to_owned(), 1000)].into(),
            ..capacity()
        };
        allocate_queued(&mut jobs, &config(), &measured);
        assert_eq!(jobs[1].request.budget.memory_mib, 1048);
    }
    #[test]
    fn a_build_waits_for_memory_instead_of_starting_on_a_token_reservation() {
        let mut jobs = vec![
            job(1, "a", 1, false, "running"),
            job(2, "b", 1, false, "queued"),
        ];
        jobs[1].request.budget.memory_mib = 300;
        let tight = Capacity {
            new_memory_mib: 300,
            ..capacity()
        };
        assert_eq!(eligible(&jobs, &config(), &tight), vec![2]);
        raise_to_measured(&mut jobs, &config(), &tight, |_| Some(1400));
        assert_eq!(jobs[1].request.budget.memory_mib, 1400);
        assert!(eligible(&jobs, &config(), &tight).is_empty());
        assert_eq!(reason(&jobs[1], &jobs, &tight), "waiting for free memory");
    }
    #[test]
    fn a_live_server_is_not_asked_to_fit_in_free_memory_a_second_time() {
        let mut jobs = vec![
            job(1, "a", 1, false, "running"),
            job(2, "b", 1, false, "queued"),
        ];
        jobs[1].request.budget.memory_mib = 1400;
        let tight = Capacity {
            new_memory_mib: 300,
            ..capacity()
        };
        assert!(eligible(&jobs, &config(), &tight).is_empty());
        let warm = Capacity {
            resident_mib: [("2".to_owned(), 1200)].into(),
            ..tight
        };
        assert_eq!(eligible(&jobs, &config(), &warm), vec![2]);
    }
    #[test]
    fn a_short_failed_run_cannot_bring_a_token_reservation_back() {
        let mut jobs = vec![job(1, "a", 1, false, "queued")];
        jobs[0].request.budget.memory_mib = 300;
        raise_to_measured(&mut jobs, &config(), &capacity(), |_| Some(500));
        assert_eq!(jobs[0].request.budget.memory_mib, 1024);
    }
    #[test]
    fn a_build_short_of_free_memory_says_so() {
        let mut fast = job(1, "a", 1, false, "running");
        fast.fast = FastLane::Admitted;
        let jobs = vec![fast, job(2, "b", 1, false, "queued")];
        let short = Capacity {
            new_memory_mib: 0,
            ..capacity()
        };
        assert_eq!(reason(&jobs[1], &jobs, &short), "waiting for free memory");
    }
    #[test]
    fn only_a_machine_without_limits_opens_the_fast_lane() {
        let jobs = vec![built(1, "w", 0), candidate(2, "w")];
        assert_eq!(fast_lane(&jobs, &config(), &capacity()), vec![2]);
        for pressure in [Pressure::Overloaded, Pressure::Constrained, Pressure::Held] {
            let limited = Capacity {
                pressure,
                ..capacity()
            };
            assert!(fast_lane(&jobs, &config(), &limited).is_empty());
        }
    }
    #[test]
    fn a_build_never_measured_still_reserves_a_server_within_its_share() {
        let mut jobs = vec![job(1, "a", 1, false, "queued")];
        jobs[0].request.budget.memory_mib = 300;
        let wide = Config {
            memory_capacity_mib: 8192,
            ..config()
        };
        raise_to_measured(&mut jobs, &wide, &capacity(), |_| None);
        assert_eq!(jobs[0].request.budget.memory_mib, 1536);
        jobs[0].request.budget.memory_mib = 300;
        raise_to_measured(&mut jobs, &config(), &capacity(), |_| None);
        assert_eq!(jobs[0].request.budget.memory_mib, 1024);
    }
    #[test]
    fn a_measured_peak_cannot_exceed_what_admission_could_ever_grant() {
        let mut jobs = vec![job(1, "a", 1, false, "queued")];
        raise_to_measured(&mut jobs, &config(), &capacity(), |_| Some(9000));
        assert_eq!(jobs[0].request.budget.memory_mib, 2048);
        jobs[0].request.budget.memory_mib = 1;
        raise_to_measured(&mut jobs, &config(), &constrained(), |_| Some(9000));
        assert_eq!(
            jobs[0].request.budget.memory_mib,
            config().constrained_budget().memory_mib
        );
    }
    #[test]
    fn opaque_and_running_reservations_are_never_raised() {
        let mut jobs = vec![
            job(1, "a", 1, true, "queued"),
            job(2, "b", 1, false, "running"),
        ];
        raise_to_measured(&mut jobs, &config(), &capacity(), |_| Some(1400));
        assert!(jobs.iter().all(|job| job.request.budget.memory_mib == 512));
    }
    #[test]
    fn one_build_starts_on_an_idle_machine_short_of_free_memory_unless_held() {
        let jobs = vec![
            job(1, "a", 1, false, "queued"),
            job(2, "b", 1, false, "queued"),
        ];
        let short = Capacity {
            new_memory_mib: 0,
            ..capacity()
        };
        assert_eq!(eligible(&jobs, &config(), &short), vec![1]);
        let swapping = Capacity {
            pressure: Pressure::Constrained,
            new_memory_mib: 0,
            ..capacity()
        };
        assert_eq!(eligible(&jobs, &config(), &swapping), vec![1]);
        let held = Capacity {
            pressure: Pressure::Held,
            new_memory_mib: 0,
            ..capacity()
        };
        assert!(eligible(&jobs, &config(), &held).is_empty());
        let mut busy = jobs;
        busy[1].state = "running".into();
        assert!(eligible(&busy, &config(), &short).is_empty());
    }
    #[test]
    fn swapping_keeps_the_next_build_waiting_behind_anything_of_ours_running() {
        let mut jobs = vec![
            job(1, "a", 1, false, "running"),
            job(2, "b", 1, false, "queued"),
        ];
        assert!(eligible(&jobs, &config(), &constrained()).is_empty());
        jobs[0].fast = FastLane::Admitted;
        assert!(eligible(&jobs, &config(), &constrained()).is_empty());
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
        let swapping = Capacity {
            reason: Some("macOS is swapping".into()),
            ..constrained()
        };
        assert_eq!(reason(&jobs[1], &jobs, &swapping), "macOS is swapping");
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
        let mut reduced = managed(2, "b");
        fit_queued(
            &mut reduced.request.budget,
            &config(),
            Pressure::Constrained,
        );
        let jobs = vec![unmanaged, reduced];
        assert_eq!(
            grants(&jobs, &config(), &constrained()),
            Grants {
                chosen: vec![2],
                overtaken: Vec::new(),
            }
        );
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
    fn small_runs_go_beside_a_large_run_that_does_not_fit_yet() {
        let mut large = job(2, "b", 8, false, "queued");
        large.request.budget.memory_mib = 9216;
        let jobs = vec![
            estimated(1, "a", 2, "running"),
            large,
            estimated(3, "c", 2, "queued"),
        ];
        assert_eq!(eligible(&jobs, &whole_machine(), &capacity()), vec![3]);
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

    fn candidate(sequence: u64, lane: &str) -> Job {
        let mut job = managed(sequence, lane);
        job.request.prepared = false;
        job.request.fast_lane = true;
        job
    }
    fn built(sequence: u64, lane: &str, code: i32) -> Job {
        let mut job = job(sequence, lane, 1, false, "finished");
        job.code = Some(code);
        job
    }
    #[test]
    fn fast_lane_checks_pass_a_full_build_queue_where_bazel_built_before() {
        let jobs = vec![
            built(1, "w", 0),
            job(2, "other", 8, false, "running"),
            managed(3, "x"),
            candidate(4, "w"),
        ];
        assert!(eligible(&jobs, &config(), &capacity()).is_empty());
        assert_eq!(fast_lane(&jobs, &config(), &capacity()), vec![4]);
    }
    #[test]
    fn fast_lane_waits_for_history_slots_memory_and_a_free_workspace() {
        let config = config();
        let both = vec![
            built(1, "w", 0),
            built(2, "v", 0),
            candidate(3, "w"),
            candidate(4, "v"),
        ];
        assert_eq!(fast_lane(&both, &config, &capacity()), vec![3, 4]);

        let first_time = vec![built(1, "w", 1), candidate(2, "w"), candidate(3, "v")];
        assert!(fast_lane(&first_time, &config, &capacity()).is_empty());

        let single = Config {
            fast_lane_slots: 1,
            ..config.clone()
        };
        assert_eq!(fast_lane(&both, &single, &capacity()), vec![3]);
        let off = Config {
            fast_lane_slots: 0,
            ..config.clone()
        };
        assert!(fast_lane(&both, &off, &capacity()).is_empty());

        let mut checking = both.clone();
        checking[2].state = "preparing".into();
        checking[2].fast = FastLane::Checking {
            queued: config.budget(true),
        };
        assert_eq!(fast_lane(&checking, &config, &capacity()), vec![4]);
        assert!(fast_lane(&checking, &single, &capacity()).is_empty());

        let mut declined = both.clone();
        declined[2].fast = FastLane::Declined;
        assert_eq!(fast_lane(&declined, &config, &capacity()), vec![4]);

        let mut busy = both.clone();
        busy.push(job(5, "w", 2, false, "running"));
        assert_eq!(fast_lane(&busy, &config, &capacity()), vec![4]);

        let one_slot_of_memory = Capacity {
            new_memory_mib: config.fast_budget().memory_mib,
            ..capacity()
        };
        assert_eq!(fast_lane(&both, &config, &one_slot_of_memory), vec![3]);
        let no_memory = Capacity {
            new_memory_mib: config.fast_budget().memory_mib - 1,
            ..capacity()
        };
        assert!(fast_lane(&both, &config, &no_memory).is_empty());
    }
    #[test]
    fn fast_lane_never_passes_earlier_work_queued_in_its_workspace() {
        let mut declined = managed(2, "w");
        declined.fast = FastLane::Declined;
        for earlier in [
            command(2, "w", "clean", "queued"),
            declined,
            candidate(2, "w"),
        ] {
            let jobs = vec![
                built(1, "w", 0),
                earlier,
                candidate(3, "w"),
                candidate(4, "v"),
            ];
            let chosen = fast_lane(&jobs, &config(), &capacity());
            assert!(!chosen.contains(&3), "{chosen:?}");
        }
        let mut cancelled = command(2, "w", "clean", "queued");
        cancelled.cancel = true;
        let jobs = vec![built(1, "w", 0), cancelled, candidate(3, "w")];
        assert_eq!(fast_lane(&jobs, &config(), &capacity()), vec![3]);
    }
    #[test]
    fn fast_lane_stops_whenever_ordinary_admission_is_held() {
        let jobs = vec![built(1, "w", 0), candidate(2, "w")];
        for capacity in [
            Capacity {
                pressure: Pressure::Held,
                ..capacity()
            },
            Capacity {
                drained: true,
                ..capacity()
            },
            Capacity {
                legacy_active: true,
                ..capacity()
            },
        ] {
            assert!(fast_lane(&jobs, &config(), &capacity).is_empty());
        }
    }
    #[test]
    fn a_fast_lane_run_holds_its_workspace_but_not_the_build_capacity() {
        let mut fast = job(1, "w", 2, false, "running");
        fast.fast = FastLane::Admitted;
        let jobs = vec![
            fast.clone(),
            job(2, "w", 8, false, "queued"),
            job(3, "x", 8, false, "queued"),
        ];
        assert!(!fast.holds_build_capacity());
        assert!(fast.holds_metered_capacity());
        assert_eq!(eligible(&jobs, &config(), &capacity()), vec![3]);
        let clean = vec![fast, command(2, "w", "clean", "queued")];
        assert!(eligible(&clean, &config(), &capacity()).is_empty());
    }
}
