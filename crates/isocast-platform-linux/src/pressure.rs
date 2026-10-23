//! What the governor may know of this machine's headroom on Linux: how
//! busy the CPUs are (`/proc/stat`) and how long tasks stall waiting for
//! memory (the kernel's pressure-stall information,
//! `/proc/pressure/memory`).
//!
//! Only what the kernel reports is carried. Linux has no OS-wide thermal
//! state (thermal zones are raw temperatures, not a verdict), so
//! `thermal_state` stays 0, unknown; and the ffmpeg writers don't expose
//! a completion latency the way VideoToolbox sessions do, so
//! `encoder_threatened` stays false and `pending_frames` 0. The governor
//! reads those as "no evidence", not as "healthy": its recovery rules
//! still need the CPU and memory readings below.
//!
//! The daemon asks every 100 ms while an output runs. The CPU figure is
//! a delta over at least [`MIN_INTERVAL`] (a tick is 10 ms, so shorter
//! windows are noise); between samples the last reading is returned with
//! its true age, which the governor ignores after 500 ms.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use isocast_engine::governor::{Headroom, MemoryPressure};

use crate::system::{CpuTicks, cpu_ticks};

/// The shortest window the CPU percentage is computed over.
const MIN_INTERVAL: Duration = Duration::from_millis(250);
/// A baseline older than this is dropped: the machine was not being
/// watched in between, so the delta would average over an idle gap.
const STALE: Duration = Duration::from_secs(2);

/// Memory pressure that stalls some task this much of the last 10 s is
/// worth telling the governor about; all tasks stalled at once (`full`)
/// is far worse than one (`some`), so it needs far less. The thresholds
/// are a judgement (recorded in docs/notes.md): the kernel declares no
/// level the way macOS does.
const WARNING_SOME: f64 = 40.0;
const WARNING_FULL: f64 = 10.0;
const CRITICAL_FULL: f64 = 30.0;

/// The `avg10` figures of `/proc/pressure/memory`: percent of the last
/// ten seconds that some, and that every, non-idle task stalled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Stall {
    pub some: f64,
    pub full: f64,
}

impl Stall {
    /// Parse the file's two lines: `some avg10=0.00 avg60=… total=…` and
    /// `full …`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let avg10 = |kind: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))?
                .split_whitespace()
                .find_map(|field| field.strip_prefix("avg10="))?
                .parse::<f64>()
                .ok()
        };
        Some(Self {
            some: avg10("some")?,
            full: avg10("full")?,
        })
    }

    pub(crate) fn level(&self) -> MemoryPressure {
        if self.full >= CRITICAL_FULL {
            MemoryPressure::Critical
        } else if self.full >= WARNING_FULL || self.some >= WARNING_SOME {
            MemoryPressure::Warning
        } else {
            MemoryPressure::Normal
        }
    }
}

/// `None` where the kernel has no pressure-stall information (older than
/// 4.20, or booted with `psi=0`): unknown, not evidence of calm.
fn memory_pressure() -> Option<MemoryPressure> {
    let text = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    Some(Stall::parse(&text)?.level())
}

struct Sampled {
    at: Instant,
    ticks: CpuTicks,
    /// The CPU percentage over the window that ended at `at`.
    cpu_percent: Option<f64>,
    memory: Option<MemoryPressure>,
}

static LAST: Mutex<Option<Sampled>> = Mutex::new(None);

fn reading(sampled: &Sampled, now: Instant) -> Headroom {
    Headroom {
        age_ms: now.duration_since(sampled.at).as_secs_f64() * 1000.0,
        // Not ready until a CPU delta exists: the first call of a watch
        // only sets the baseline.
        ready: sampled.cpu_percent.is_some(),
        encoder_threatened: false,
        pending_frames: 0,
        recovery_ratio: 0.0,
        cpu_percent: sampled.cpu_percent,
        thermal_state: 0,
        memory_pressure: sampled.memory,
    }
}

/// The latest reading, sampling the kernel again if the last one is
/// older than [`MIN_INTERVAL`]. `None` if `/proc/stat` can't be read.
pub(crate) fn headroom() -> Option<Headroom> {
    let now = Instant::now();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(sampled) = last.as_ref()
        && now.duration_since(sampled.at) < MIN_INTERVAL
    {
        return Some(reading(sampled, now));
    }
    let ticks = cpu_ticks()?;
    let cpu_percent = last
        .as_ref()
        .filter(|before| now.duration_since(before.at) <= STALE)
        .and_then(|before| ticks.percent_since(&before.ticks));
    let sampled = Sampled {
        at: now,
        ticks,
        cpu_percent,
        memory: memory_pressure(),
    };
    let headroom = reading(&sampled, now);
    *last = Some(sampled);
    Some(headroom)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSI: &str = "some avg10=15.68 avg60=11.85 avg300=7.46 total=742075020\n\
                       full avg10=13.45 avg60=10.82 avg300=7.09 total=721390308\n";

    #[test]
    fn pressure_stall_lines_parse() {
        let stall = Stall::parse(PSI).unwrap();
        assert_eq!(stall.some, 15.68);
        assert_eq!(stall.full, 13.45);
        assert_eq!(Stall::parse("some avg10=1.00\n"), None);
        assert_eq!(Stall::parse(""), None);
        assert_eq!(Stall::parse("some avg10=x full avg10=1"), None);
    }

    #[test]
    fn stall_maps_to_a_level() {
        let level = |some, full| Stall { some, full }.level();
        assert_eq!(level(0.0, 0.0), MemoryPressure::Normal);
        assert_eq!(level(39.9, 9.9), MemoryPressure::Normal);
        assert_eq!(level(40.0, 0.0), MemoryPressure::Warning);
        assert_eq!(level(5.0, 10.0), MemoryPressure::Warning);
        assert_eq!(level(80.0, 29.9), MemoryPressure::Warning);
        assert_eq!(level(80.0, 30.0), MemoryPressure::Critical);
        // The sample the thresholds were set against: a box under real
        // memory stress is a Warning, as it should be.
        assert_eq!(Stall::parse(PSI).unwrap().level(), MemoryPressure::Warning);
    }

    #[test]
    fn cpu_ticks_parse_and_difference() {
        let before = CpuTicks::parse("cpu  100 10 50 800 40 5 5 0 0 0").unwrap();
        // total 1010, busy 1010 - 800 - 40 = 170
        assert_eq!(
            before,
            CpuTicks {
                busy: 170,
                total: 1010
            }
        );
        let after = CpuTicks::parse("cpu  160 10 90 850 50 5 5 0 0 0").unwrap();
        // 1170 total (+160), busy 270 (+100): 62.5 %
        assert_eq!(after.percent_since(&before), Some(62.5));
        assert_eq!(before.percent_since(&before), None);
        assert_eq!(before.percent_since(&after), None);
        assert!(CpuTicks::parse("cpu0 1 2 3 4 5 6 7 8").is_none());
        assert!(CpuTicks::parse("cpu 1 2 3").is_none());
    }

    /// The live kernel: a baseline, then a ready reading a window later.
    #[test]
    fn headroom_reads_this_machine() {
        let first = headroom().expect("/proc/stat is readable");
        std::thread::sleep(MIN_INTERVAL + Duration::from_millis(100));
        let second = headroom().expect("/proc/stat is readable");
        assert!(second.ready, "a delta exists after a full window");
        let cpu = second.cpu_percent.expect("a delta exists");
        assert!((0.0..=100.0).contains(&cpu), "{cpu}");
        assert!(second.age_ms < 50.0, "just sampled: {}", second.age_ms);
        assert_eq!(second.thermal_state, 0);
        eprintln!(
            "first ready={} cpu={:?}; second cpu={cpu:.1}% memory={:?}",
            first.ready, first.cpu_percent, second.memory_pressure
        );
    }

    #[test]
    fn the_first_reading_is_only_a_baseline() {
        let sampled = Sampled {
            at: Instant::now(),
            ticks: CpuTicks { busy: 1, total: 2 },
            cpu_percent: None,
            memory: Some(MemoryPressure::Normal),
        };
        let first = reading(&sampled, Instant::now());
        assert!(!first.ready);
        assert_eq!(first.cpu_percent, None);
        assert_eq!(first.thermal_state, 0);
        assert!(!first.encoder_threatened);
        let measured = Sampled {
            cpu_percent: Some(12.5),
            ..sampled
        };
        assert!(reading(&measured, Instant::now()).ready);
    }
}
