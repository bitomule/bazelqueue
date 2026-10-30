#![forbid(unsafe_code)]
//! Integrates observed intervals, not job durations (parallel jobs overlap).
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Hour {
    pub start_unix_ms: u64,
    pub observed_ms: u64,
    pub busy_ms: u64,
    pub reserved_cpu_percent_ms: f64,
    pub reserved_memory_percent_ms: f64,
    pub host_cpu_percent_ms: f64,
    pub host_cpu_observed_ms: u64,
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub monotonic_ms: u64,
    pub unix_ms: u64,
    pub busy: bool,
    pub reserved_cpu_percent: f64,
    pub reserved_memory_percent: f64,
    pub host_cpu_percent: Option<f64>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Utilization {
    pub hours: Vec<Hour>,
    #[serde(default)]
    covered_until_ms: u64,
    #[serde(skip)]
    previous: Option<Observation>,
}

impl Utilization {
    pub fn observe(&mut self, next: Observation) {
        if let Some(previous) = self.previous.take() {
            let elapsed = next.monotonic_ms.checked_sub(previous.monotonic_ms);
            let wall = next.unix_ms.checked_sub(previous.unix_ms);
            if let (Some(elapsed), Some(wall)) = (elapsed, wall)
                && elapsed <= 5000
                && elapsed.abs_diff(wall) <= 1000
                && wall > 0
            {
                let mut cursor = previous.unix_ms.max(self.covered_until_ms);
                while cursor < next.unix_ms {
                    let start = cursor / 3_600_000 * 3_600_000;
                    let duration = (start + 3_600_000).min(next.unix_ms) - cursor;
                    let hour = match self
                        .hours
                        .iter_mut()
                        .find(|hour| hour.start_unix_ms == start)
                    {
                        Some(hour) => hour,
                        None => {
                            self.hours.push(Hour {
                                start_unix_ms: start,
                                ..Hour::default()
                            });
                            self.hours.last_mut().unwrap()
                        }
                    };
                    hour.observed_ms += duration;
                    hour.busy_ms += if previous.busy { duration } else { 0 };
                    hour.reserved_cpu_percent_ms += previous.reserved_cpu_percent * duration as f64;
                    hour.reserved_memory_percent_ms +=
                        previous.reserved_memory_percent * duration as f64;
                    if let Some(cpu) = previous.host_cpu_percent.filter(|cpu| cpu.is_finite()) {
                        hour.host_cpu_percent_ms += cpu.clamp(0.0, 100.0) * duration as f64;
                        hour.host_cpu_observed_ms += duration;
                    }
                    cursor += duration;
                }
                self.covered_until_ms = self.covered_until_ms.max(next.unix_ms);
            }
        }
        let first_hour = (self.covered_until_ms / 3_600_000).saturating_sub(23) * 3_600_000;
        self.hours.retain(|hour| hour.start_unix_ms >= first_hour);
        self.hours.sort_by_key(|hour| hour.start_unix_ms);
        self.previous = Some(next);
    }
    pub fn visible_hours(&self, unix_ms: u64) -> Vec<Hour> {
        let current = unix_ms / 3_600_000 * 3_600_000;
        self.hours
            .iter()
            .filter(|hour| hour.start_unix_ms <= current)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn observation(time: u64, busy: bool, reserved: f64) -> Observation {
        Observation {
            monotonic_ms: time,
            unix_ms: time,
            busy,
            reserved_cpu_percent: reserved,
            reserved_memory_percent: reserved,
            host_cpu_percent: Some(80.0),
        }
    }
    #[test]
    fn integrates_overlaps_as_one_busy_interval_and_splits_hours() {
        let mut usage = Utilization::default();
        usage.observe(observation(3_599_000, true, 50.0));
        usage.observe(observation(3_601_000, true, 100.0));
        usage.observe(observation(3_602_000, false, 0.0));
        usage.observe(observation(3_603_000, false, 0.0));
        assert_eq!(usage.hours[0].busy_ms, 1000);
        let hour = &usage.hours[1];
        assert_eq!((hour.observed_ms, hour.busy_ms), (3000, 2000));
        assert_eq!(hour.reserved_cpu_percent_ms, 150_000.0);
        assert_eq!(hour.host_cpu_percent_ms, 240_000.0);
    }
    #[test]
    fn restart_missing_samples_and_clock_jumps_are_unknown_not_idle() {
        let mut usage = Utilization::default();
        usage.observe(observation(0, true, 100.0));
        usage.observe(observation(1000, true, 100.0));
        let json = serde_json::to_string(&usage).unwrap();
        let mut usage: Utilization = serde_json::from_str(&json).unwrap();
        usage.observe(observation(10_000, true, 100.0));
        usage.observe(observation(11_000, false, 0.0));
        usage.observe(observation(30_000, false, 0.0));
        usage.observe(Observation {
            unix_ms: 10,
            ..observation(31_000, false, 0.0)
        });
        assert_eq!(usage.hours[0].observed_ms, 2000);
        assert_eq!(usage.hours[0].busy_ms, 2000);
    }
    #[test]
    fn clock_rewind_does_not_count_the_same_wall_interval_again_even_after_restart() {
        let mut usage = Utilization::default();
        usage.observe(Observation {
            unix_ms: 1000,
            ..observation(0, true, 100.0)
        });
        usage.observe(Observation {
            unix_ms: 2000,
            ..observation(1000, true, 100.0)
        });
        let mut usage: Utilization =
            serde_json::from_str(&serde_json::to_string(&usage).unwrap()).unwrap();
        usage.observe(Observation {
            unix_ms: 1000,
            ..observation(2000, true, 100.0)
        });
        usage.observe(Observation {
            unix_ms: 2000,
            ..observation(3000, true, 100.0)
        });
        assert_eq!(usage.hours[0].observed_ms, 1000);
    }
    #[test]
    fn a_rejected_forward_jump_does_not_suppress_corrected_clock_intervals() {
        let mut usage = Utilization::default();
        for (mono, unix) in [
            (0, 1000),
            (1000, 2000),
            (2000, 86_402_000),
            (3000, 3000),
            (4000, 4000),
        ] {
            usage.observe(Observation {
                unix_ms: unix,
                ..observation(mono, true, 100.0)
            });
        }
        assert_eq!(
            usage.hours.iter().map(|hour| hour.observed_ms).sum::<u64>(),
            2000
        );
    }
}
