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
        .filter(|job| job.holds_metered_capacity())
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
    pub disk_free_mib: Option<u64>,
    pub cpu_busy_percent: Option<f64>,
    pub load_per_cpu: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryPressure {
    Normal,
    Warning,
    Critical,
}

impl MemoryPressure {
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

impl Sample {
    pub fn memory_pressure(&self) -> Option<MemoryPressure> {
        self.pressure.map(|level| match level {
            1 => MemoryPressure::Normal,
            2 => MemoryPressure::Warning,
            _ => MemoryPressure::Critical,
        })
    }
}

/// What limits new admission. Decisions read this; `Signals::reason` is only its display text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    MemoryUnavailable,
    SwapUnavailable,
    DiskUnavailable,
    DiskLow,
    Swapping,
    PressureCritical,
    CpuOverload,
    PressureWarning,
}

impl Limit {
    pub fn reason(self) -> &'static str {
        match self {
            Self::MemoryUnavailable => "memory sample unavailable",
            Self::SwapUnavailable => "swap sample unavailable",
            Self::DiskUnavailable => "disk sample unavailable",
            Self::DiskLow => "low disk space",
            Self::Swapping => "macOS is swapping",
            Self::PressureCritical => "memory pressure (critical)",
            Self::CpuOverload => "CPU overload",
            Self::PressureWarning => "memory pressure (warning)",
        }
    }

    pub fn mode(self) -> Pressure {
        match self {
            Self::PressureWarning => Pressure::Constrained,
            _ => Pressure::Held,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Signals {
    pub sample: Sample,
    pub swap_in_mib_per_min: Option<f64>,
    pub swap_out_mib_per_min: Option<f64>,
    #[serde(default)]
    pub swap_growth_mib_per_min: Option<f64>,
    #[serde(default)]
    pub limit: Option<Limit>,
    pub reason: Option<String>,
}

/// Long enough that the pauses between macOS's swap bursts do not read as recovery.
const SWAP_WINDOW_MS: u64 = 60_000;
const SENSOR_REUSE_MS: u64 = 5000;

/// Readings of one swap sensor over the window. A rate needs two readings in it.
#[derive(Default)]
struct SwapWindow(std::collections::VecDeque<(u64, u64)>);

impl SwapWindow {
    fn push(&mut self, at_ms: u64, value: Option<u64>) -> Option<f64> {
        let value = value?;
        if self.0.back().is_some_and(|(last, _)| at_ms <= *last) {
            return None;
        }
        self.0.push_back((at_ms, value));
        while self
            .0
            .front()
            .is_some_and(|(at, _)| at_ms.saturating_sub(*at) > SWAP_WINDOW_MS)
        {
            self.0.pop_front();
        }
        let (first, start) = *self.0.front()?;
        (first < at_ms)
            .then(|| value.saturating_sub(start) as f64 * 60_000.0 / (at_ms - first) as f64)
    }

    /// A counter never falls unless it restarted, and no rate may span that.
    fn push_counter(&mut self, at_ms: u64, bytes: Option<u64>) -> Option<f64> {
        if bytes
            .zip(self.0.back())
            .is_some_and(|(bytes, (_, last))| bytes < *last)
        {
            self.0.clear();
        }
        self.push(at_ms, bytes).map(|rate| rate / 1_048_576.0)
    }
}

#[derive(Default)]
pub struct Monitor {
    swap_in: SwapWindow,
    swap_out: SwapWindow,
    swap_used: SwapWindow,
    memory_at: Option<u64>,
    swap_at: Option<u64>,
    overloaded: bool,
    swapping: bool,
    pub signals: Signals,
}

impl Monitor {
    pub fn update(&mut self, mut sample: Sample, config: &Config) {
        let input = self
            .swap_in
            .push_counter(sample.at_ms, sample.swap_in_bytes);
        let output = self
            .swap_out
            .push_counter(sample.at_ms, sample.swap_out_bytes);
        let growth = self.swap_used.push(sample.at_ms, sample.swap_used_mib);
        let threshold = config.swap_thrashing_mib_per_min as f64;
        if [output, growth]
            .iter()
            .flatten()
            .any(|rate| *rate > threshold)
        {
            self.swapping = true;
            self.swap_at = Some(sample.at_ms);
        } else if output.is_some() && growth.is_some() {
            self.swapping = false;
            self.swap_at = Some(sample.at_ms);
        }
        if sample.available_mib.is_some() {
            self.memory_at = Some(sample.at_ms);
        } else if self
            .memory_at
            .is_some_and(|at| sample.at_ms.saturating_sub(at) <= SENSOR_REUSE_MS)
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
        let swap_known = self
            .swap_at
            .is_some_and(|at| sample.at_ms.saturating_sub(at) <= SENSOR_REUSE_MS);
        let limit = if sample.available_mib.is_none() || sample.pressure.is_none() {
            Some(Limit::MemoryUnavailable)
        } else if self.swapping {
            Some(Limit::Swapping)
        } else if !swap_known {
            Some(Limit::SwapUnavailable)
        } else if sample.disk_free_mib.is_none() {
            Some(Limit::DiskUnavailable)
        } else if sample
            .disk_free_mib
            .is_some_and(|free| free < config.disk_safety_mib)
        {
            Some(Limit::DiskLow)
        } else if sample.memory_pressure() == Some(MemoryPressure::Critical) {
            Some(Limit::PressureCritical)
        } else if self.overloaded {
            Some(Limit::CpuOverload)
        } else if sample.memory_pressure() == Some(MemoryPressure::Warning) {
            Some(Limit::PressureWarning)
        } else {
            None
        };
        self.signals = Signals {
            sample,
            swap_in_mib_per_min: input,
            swap_out_mib_per_min: output,
            swap_growth_mib_per_min: growth,
            limit,
            reason: limit.map(|limit| limit.reason().to_owned()),
        };
    }

    pub fn mode(&self) -> Pressure {
        self.signals.limit.map_or(Pressure::Normal, Limit::mode)
    }

    pub fn headroom(&self, config: &Config) -> u64 {
        let sample = &self.signals.sample;
        sample
            .available_mib
            .unwrap_or(0)
            .saturating_sub(config.memory_safety_mib.max(sample.total_mib / 20))
    }
}

const LANE_RUNS: usize = 3;
const LANES: usize = 512;

#[derive(Serialize, Deserialize)]
struct LaneRuns {
    peaks_mib: Vec<u64>,
    finished: u64,
}

/// Peak resident memory of recent builds per output base, server and children included.
#[derive(Default, Serialize, Deserialize)]
pub struct LaneMemory {
    lanes: std::collections::BTreeMap<String, LaneRuns>,
    finished: u64,
    #[serde(skip)]
    running: std::collections::HashMap<String, (String, u64)>,
}

impl LaneMemory {
    /// Folds one reading of every running build; one missing from it has ended.
    /// Returns whether a finished run changed what must be persisted.
    pub fn observe(&mut self, readings: impl IntoIterator<Item = (String, String, u64)>) -> bool {
        let mut running = std::collections::HashMap::new();
        for (id, lane, resident) in readings {
            let peak = self.running.remove(&id).map_or(0, |(_, peak)| peak);
            running.insert(id, (lane, peak.max(resident)));
        }
        let ended = std::mem::replace(&mut self.running, running);
        let changed = ended.values().any(|(_, peak)| *peak > 0);
        for (lane, peak) in ended.into_values().filter(|(_, peak)| *peak > 0) {
            self.finished += 1;
            let runs = self.lanes.entry(lane).or_insert(LaneRuns {
                peaks_mib: Vec::new(),
                finished: 0,
            });
            runs.finished = self.finished;
            runs.peaks_mib.push(peak);
            if runs.peaks_mib.len() > LANE_RUNS {
                runs.peaks_mib.remove(0);
            }
        }
        while self.lanes.len() > LANES {
            let oldest = self
                .lanes
                .iter()
                .min_by_key(|(_, runs)| runs.finished)
                .map(|(lane, _)| lane.clone())
                .expect("lanes is not empty");
            self.lanes.remove(&oldest);
        }
        changed
    }

    /// What a new build in this lane should reserve: its recent peak plus a quarter.
    pub fn needed_mib(&self, lane: &str) -> Option<u64> {
        let peak = self.lanes.get(lane)?.peaks_mib.iter().copied().max()?;
        Some(peak.saturating_add(peak / 4))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PLENTY: u64 = 1 << 20;
    fn sample(at_ms: u64, pressure: u32, counter: u64) -> Sample {
        Sample {
            at_ms,
            pressure: Some(pressure),
            total_mib: 18000,
            available_mib: Some(6000),
            swap_in_bytes: Some(counter),
            swap_out_bytes: Some(counter),
            swap_used_mib: Some(0),
            disk_free_mib: Some(PLENTY),
            ..Sample::default()
        }
    }
    fn settled(config: &Config, pressure: u32) -> Monitor {
        let mut monitor = Monitor::default();
        monitor.update(sample(0, pressure, 0), config);
        monitor.update(sample(1000, pressure, 0), config);
        monitor
    }
    #[test]
    fn warning_allows_one_build_and_critical_holds_until_the_next_safe_sample() {
        let config = Config::default();
        let mut monitor = settled(&config, 1);
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(sample(2000, 2, 0), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::PressureWarning));
        assert_eq!(monitor.mode(), Pressure::Constrained);
        monitor.update(sample(3000, 4, 0), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::PressureCritical));
        assert_eq!(monitor.mode(), Pressure::Held);
        monitor.update(sample(4000, 1, 0), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
    }
    #[test]
    fn the_measured_swap_growth_holds_warning_admission_until_it_stops() {
        let config = Config::default();
        let measured = |at_ms, used| Sample {
            available_mib: Some(6819),
            swap_used_mib: Some(used),
            ..sample(at_ms, 2, 0)
        };
        let mut monitor = Monitor::default();
        for second in 0..=30 {
            monitor.update(measured(second * 1000, 9000 + second * 17), &config);
        }
        assert_eq!(monitor.signals.limit, Some(Limit::Swapping));
        assert_eq!(monitor.mode(), Pressure::Held);
        assert_eq!(monitor.signals.swap_growth_mib_per_min, Some(1020.0));
        let plateau = 9000 + 30 * 17;
        monitor.update(measured(61_000, plateau), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::Swapping));
        monitor.update(measured(91_000, plateau), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::PressureWarning));
        monitor.update(
            Sample {
                swap_used_mib: Some(plateau),
                ..sample(92_000, 1, 0)
            },
            &config,
        );
        assert_eq!(monitor.mode(), Pressure::Normal);
    }
    #[test]
    fn swap_writes_hold_admission_even_while_occupancy_stays_flat() {
        let config = Config::default();
        let mut monitor = settled(&config, 1);
        let mut next = sample(61_000, 1, 0);
        next.swap_out_bytes = Some(65 * 1_048_576);
        monitor.update(next, &config);
        assert_eq!(monitor.signals.limit, Some(Limit::Swapping));
    }
    #[test]
    fn a_pause_between_swap_bursts_is_not_recovery() {
        let config = Config::default();
        let mut monitor = settled(&config, 1);
        let burst = 200 * 1_048_576;
        let mut next = sample(2000, 1, 0);
        next.swap_out_bytes = Some(burst);
        monitor.update(next.clone(), &config);
        assert_eq!(monitor.mode(), Pressure::Held);
        next.at_ms = 30_000;
        monitor.update(next.clone(), &config);
        assert_eq!(monitor.mode(), Pressure::Held);
        next.at_ms = 63_000;
        monitor.update(next, &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
    }
    #[test]
    fn old_swap_and_swap_in_alone_do_not_slow_builds_down() {
        let config = Config::default();
        let mut monitor = Monitor::default();
        for second in 0..5 {
            monitor.update(
                Sample {
                    swap_used_mib: Some(9592),
                    swap_in_bytes: Some(second * 500 * 1_048_576),
                    swap_out_bytes: Some(0),
                    ..sample(second * 1000, 1, 0)
                },
                &config,
            );
        }
        assert_eq!(monitor.mode(), Pressure::Normal);
    }
    #[test]
    fn exact_threshold_is_not_swapping() {
        let config = Config::default();
        let mut monitor = settled(&config, 1);
        let mut next = sample(61_000, 1, 0);
        next.swap_out_bytes = Some(64 * 1_048_576);
        monitor.update(next, &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
    }
    #[test]
    fn swap_that_cannot_be_measured_holds_admission() {
        let config = Config::default();
        let mut monitor = Monitor::default();
        monitor.update(sample(0, 1, 0), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::SwapUnavailable));
        monitor.update(sample(1000, 1, 0), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        let mute = |at_ms| Sample {
            swap_in_bytes: None,
            swap_out_bytes: None,
            swap_used_mib: None,
            ..sample(at_ms, 1, 0)
        };
        monitor.update(mute(6000), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(mute(6001), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::SwapUnavailable));
        assert_eq!(monitor.mode(), Pressure::Held);
    }
    #[test]
    fn low_or_unknown_disk_holds_admission() {
        let config = Config::default();
        let mut monitor = settled(&config, 1);
        let disk = |at_ms, free| Sample {
            disk_free_mib: free,
            ..sample(at_ms, 1, 0)
        };
        monitor.update(disk(2000, Some(config.disk_safety_mib - 1)), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::DiskLow));
        assert_eq!(monitor.mode(), Pressure::Held);
        monitor.update(disk(3000, Some(config.disk_safety_mib)), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(disk(4000, None), &config);
        assert_eq!(monitor.signals.limit, Some(Limit::DiskUnavailable));
    }
    #[test]
    fn a_status_reader_gets_the_typed_limit_beside_its_text() {
        let config = Config::default();
        let mut monitor = settled(&config, 4);
        monitor.update(sample(2000, 4, 0), &config);
        let json = serde_json::to_value(&monitor.signals).unwrap();
        assert_eq!(json["limit"], "pressure_critical");
        assert_eq!(json["reason"], "memory pressure (critical)");
    }
    #[test]
    fn missing_memory_expires_and_cpu_recovers_without_a_timer() {
        let config = Config::default();
        let mut monitor = settled(&config, 2);
        let missing = |at_ms| Sample {
            available_mib: None,
            total_mib: 0,
            ..sample(at_ms, 1, 0)
        };
        monitor.update(missing(6000), &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        monitor.update(missing(6001), &config);
        assert_eq!(monitor.mode(), Pressure::Held);
        let mut next = sample(7000, 1, 0);
        next.load_per_cpu = Some(4.1);
        monitor.update(next.clone(), &config);
        assert_eq!(monitor.mode(), Pressure::Held);
        next.at_ms = 8000;
        next.load_per_cpu = Some(2.9);
        monitor.update(next, &config);
        assert_eq!(monitor.mode(), Pressure::Normal);
        assert_eq!(monitor.headroom(&config), 4976);
    }
    #[test]
    fn known_swapping_needs_valid_readings_to_recover() {
        let config = Config::default();
        for invalid in [
            Sample {
                at_ms: 2000,
                ..Sample::default()
            },
            sample(1000, 1, 2_000_000),
            sample(2000, 1, 0),
        ] {
            let mut monitor = Monitor::default();
            monitor.update(sample(0, 1, 0), &config);
            monitor.update(sample(1000, 1, 2_000_000), &config);
            assert_eq!(monitor.signals.limit, Some(Limit::Swapping));
            monitor.update(invalid, &config);
            assert_eq!(monitor.signals.limit, Some(Limit::Swapping));
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
    fn reading(id: &str, resident: u64) -> (String, String, u64) {
        (id.into(), "base".into(), resident)
    }
    #[test]
    fn a_build_leaves_its_peak_for_the_next_one_in_that_output_base() {
        let mut lanes = LaneMemory::default();
        assert!(!lanes.observe([reading("a", 900)]));
        assert!(!lanes.observe([reading("a", 2000)]));
        assert!(!lanes.observe([reading("a", 1200)]));
        assert_eq!(lanes.needed_mib("base"), None);
        assert!(lanes.observe([]));
        assert_eq!(lanes.needed_mib("base"), Some(2500));
        assert_eq!(lanes.needed_mib("other"), None);
        let restored: LaneMemory =
            serde_json::from_str(&serde_json::to_string(&lanes).unwrap()).unwrap();
        assert_eq!(restored.needed_mib("base"), Some(2500));
    }
    #[test]
    fn an_output_base_forgets_all_but_its_latest_runs() {
        let mut lanes = LaneMemory::default();
        for (run, peak) in [4000, 1000, 1000, 1000].into_iter().enumerate() {
            lanes.observe([reading(&run.to_string(), peak)]);
        }
        lanes.observe([]);
        assert_eq!(lanes.needed_mib("base"), Some(1250));
    }
    #[test]
    fn a_build_never_measured_leaves_no_peak() {
        let mut lanes = LaneMemory::default();
        lanes.observe([reading("a", 0)]);
        assert!(!lanes.observe([]));
        assert_eq!(lanes.needed_mib("base"), None);
    }
}
