#![forbid(unsafe_code)]
//! Pure admission signals. Monotonic timestamps and counters are supplied by the caller.
use crate::{config::Config, scheduler::Pressure};
use serde::{Deserialize, Serialize};

pub fn refresh_resident(
    sample: &Sample,
    resident: &mut std::collections::HashMap<String, u64>,
    read: impl FnOnce() -> std::collections::HashMap<String, u64>,
) {
    // Availability and residency must refer to the same observation. Growing
    // residency cannot release promised growth against an older free-memory value.
    if sample.available_mib.is_some() {
        *resident = read();
    }
}

pub fn promised_growth(
    jobs: &[crate::protocol::Job],
    resident: &std::collections::HashMap<String, u64>,
) -> u64 {
    jobs.iter()
        .filter(|job| job.holds_build_capacity())
        .fold(0_u64, |total, job| {
            total.saturating_add(
                job.request
                    .budget
                    .memory_mib
                    .saturating_sub(resident.get(&job.request.id).copied().unwrap_or(0)),
            )
        })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sample {
    pub at_ms: u64,
    pub pressure: Option<u32>,
    pub total_mib: u64,
    pub available_mib: Option<u64>,
    pub swap_in_bytes: Option<u64>,
    pub swap_out_bytes: Option<u64>,
    pub swap_used_mib: Option<u64>,
    pub compressor_mib: Option<u64>,
    pub cpu_busy_percent: Option<f64>,
    pub load_per_cpu: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Signals {
    pub sample: Sample,
    pub swap_in_mib_per_min: Option<f64>,
    pub swap_out_mib_per_min: Option<f64>,
    pub reason: Option<String>,
}

#[derive(Default)]
pub struct Monitor {
    previous: Option<Sample>,
    memory_at: Option<u64>,
    overloaded: bool,
    thrashing: bool,
    pub signals: Signals,
}

impl Monitor {
    pub fn update(&mut self, mut sample: Sample, config: &Config) {
        let rate = |current: Option<u64>, previous: Option<u64>, elapsed: u64| {
            let delta = current?.checked_sub(previous?)?;
            (elapsed > 0).then(|| delta as f64 * 60_000.0 / elapsed as f64 / 1_048_576.0)
        };
        let (mut input, mut output) = (None, None);
        if let Some(previous) = &self.previous {
            let elapsed = sample.at_ms.saturating_sub(previous.at_ms);
            input = rate(sample.swap_in_bytes, previous.swap_in_bytes, elapsed);
            output = rate(sample.swap_out_bytes, previous.swap_out_bytes, elapsed);
        }
        self.previous = Some(sample.clone());
        if let Some((input, output)) = input.zip(output) {
            self.thrashing = input > config.swap_thrashing_mib_per_min as f64
                && output > config.swap_thrashing_mib_per_min as f64;
        }
        if sample.available_mib.is_some() {
            self.memory_at = Some(sample.at_ms);
        } else if self
            .memory_at
            .is_some_and(|at| sample.at_ms.saturating_sub(at) <= 5000)
        {
            sample.available_mib = self.signals.sample.available_mib;
            sample.total_mib = self.signals.sample.total_mib;
        }
        // Losing the pressure sensor cannot erase the last critical observation.
        if sample.pressure.is_none() {
            sample.pressure = self.signals.sample.pressure;
        }
        if let Some(load) = sample.load_per_cpu.filter(|load| load.is_finite()) {
            if load > 4.0 {
                self.overloaded = true;
            } else if load < 3.0 {
                self.overloaded = false;
            }
        }
        let reason = if sample.available_mib.is_none() {
            Some("memory sample unavailable")
        } else if self.overloaded {
            Some("CPU overload")
        } else if sample
            .pressure
            .is_some_and(|level| level != 1 && level != 2)
        {
            Some("memory pressure (critical)")
        } else if self.thrashing {
            Some("swap thrashing")
        } else {
            None
        };
        self.signals = Signals {
            sample,
            swap_in_mib_per_min: input,
            swap_out_mib_per_min: output,
            reason: reason.map(str::to_owned),
        };
    }

    pub fn mode(&self) -> Pressure {
        match self.signals.reason.as_deref() {
            Some("memory pressure (critical)" | "swap thrashing") => Pressure::Constrained,
            Some(_) => Pressure::Held,
            None => Pressure::Normal,
        }
    }

    pub fn headroom(&self, config: &Config) -> u64 {
        let sample = &self.signals.sample;
        sample
            .available_mib
            .unwrap_or(0)
            .saturating_sub(config.memory_safety_mib.max(sample.total_mib / 20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(at_ms: u64, pressure: u32, counter: u64) -> Sample {
        Sample {
            at_ms,
            pressure: Some(pressure),
            total_mib: 18000,
            available_mib: Some(6000),
            swap_in_bytes: Some(counter),
            swap_out_bytes: Some(counter),
            ..Sample::default()
        }
    }
    #[test]
    fn warning_is_full_critical_and_bidirectional_swap_reduce_and_recover_immediately() {
        let config = Config::default();
        let mut monitor = Monitor::default();
        monitor.update(sample(0, 2, 900_000_000), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(sample(1000, 4, 900_000_000), &config);
        assert_eq!(monitor.mode(), Pressure::Constrained);
        monitor.update(sample(2000, 2, 900_000_000), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(sample(3000, 2, 902_000_000), &config);
        assert_eq!(monitor.mode(), Pressure::Constrained);
        monitor.update(sample(4000, 2, 902_000_000), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(sample(5000, 2, 0), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        assert_eq!(monitor.signals.swap_out_mib_per_min, None);
    }
    #[test]
    fn one_way_swap_and_exact_threshold_are_not_thrash() {
        let config = Config::default();
        let mut monitor = Monitor::default();
        monitor.update(sample(0, 2, 0), &config);
        monitor.update(sample(60_000, 2, 64 * 1_048_576), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        let mut next = sample(61_000, 2, 128 * 1_048_576);
        next.swap_in_bytes = Some(64 * 1_048_576);
        monitor.update(next, &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
    }
    #[test]
    fn missing_memory_expires_and_cpu_recovers_without_a_timer() {
        let config = Config::default();
        let mut monitor = Monitor::default();
        monitor.update(sample(0, 2, 0), &config);
        monitor.update(
            Sample {
                at_ms: 5000,
                ..Sample::default()
            },
            &config,
        );
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(
            Sample {
                at_ms: 5001,
                ..Sample::default()
            },
            &config,
        );
        assert_eq!(monitor.mode(), Pressure::Held);
        let mut next = sample(6000, 2, 0);
        next.load_per_cpu = Some(4.1);
        monitor.update(next.clone(), &config);
        assert_eq!(monitor.mode(), Pressure::Held);
        next.load_per_cpu = Some(2.9);
        monitor.update(next, &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        assert_eq!(monitor.headroom(&config), 4976);
    }
    #[test]
    fn known_thrash_needs_valid_counter_rates_to_recover() {
        let config = Config::default();
        for invalid in [
            Sample {
                at_ms: 2000,
                ..Sample::default()
            },
            sample(1000, 2, 2_000_000),
            sample(2000, 2, 0),
        ] {
            let mut monitor = Monitor::default();
            monitor.update(sample(0, 2, 0), &config);
            monitor.update(sample(1000, 2, 2_000_000), &config);
            assert_eq!(monitor.mode(), Pressure::Constrained);
            monitor.update(invalid, &config);
            assert_eq!(monitor.mode(), Pressure::Constrained);
        }
    }
    #[test]
    fn stale_available_memory_keeps_its_original_residency_snapshot() {
        let mut resident = std::collections::HashMap::from([("active".into(), 0)]);
        let missing = Sample {
            at_ms: 1000,
            available_mib: None,
            ..Sample::default()
        };
        refresh_resident(&missing, &mut resident, || {
            std::collections::HashMap::from([("active".into(), 3000)])
        });
        assert_eq!(resident["active"], 0);
    }
}
