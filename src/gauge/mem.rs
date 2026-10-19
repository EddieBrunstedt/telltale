//! Memory in use: the total minus what the kernel reports as available.
//!
//! Page cache is not "in use": the kernel reclaims it on demand, and
//! `MemAvailable` is its own estimate of what could be handed out without
//! swapping. This is what `free` calls "used".

use std::fs;
use std::time::Duration;

use crate::gauge::Gauge;
use crate::gauge::pace::Pace;
use crate::reading::{Level, Reading};

const EVERY: Duration = Duration::from_secs(2);
/// Percent of the total in use, as shown (rounded).
const NORMAL_FROM: u64 = 50;
const ATTENTION_FROM: u64 = 80;
const CRITICAL_FROM: u64 = 90;
/// Percent of the last 10 s in which every task was stalled on memory. Above
/// this the machine is thrashing, whatever the percentage says.
const THRASHING_ABOVE: f64 = 1.0;

pub struct Mem {
    pace: Pace,
}

impl Mem {
    pub fn new() -> Mem {
        Mem {
            pace: Pace::every(EVERY),
        }
    }
}

impl Default for Mem {
    fn default() -> Mem {
        Mem::new()
    }
}

impl Gauge for Mem {
    fn next(&mut self) -> Reading {
        self.pace.wait();
        let memory = fs::read_to_string("/proc/meminfo")
            .map_err(|e| format!("/proc/meminfo: {e}"))
            .and_then(|text| {
                parse_meminfo(&text).ok_or_else(|| {
                    "/proc/meminfo: no usable MemTotal, MemAvailable, SwapTotal or SwapFree"
                        .to_owned()
                })
            });
        // Missing when the kernel is built without PSI: that loses the
        // thrashing check, but the percentage is still an honest reading.
        let stalled = fs::read_to_string("/proc/pressure/memory")
            .ok()
            .and_then(|text| parse_full_avg10(&text));
        assess(memory, stalled)
    }
}

/// All in KiB, as `/proc/meminfo` reports them (it says "kB").
#[derive(Debug, PartialEq)]
struct Memory {
    total: u64,
    available: u64,
    swap_total: u64,
    swap_free: u64,
}

fn parse_meminfo(text: &str) -> Option<Memory> {
    let field = |name: &str| -> Option<u64> {
        text.lines().find_map(|line| {
            line.strip_prefix(name)?
                .strip_prefix(':')?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
    };
    Some(Memory {
        total: field("MemTotal").filter(|&t| t > 0)?,
        available: field("MemAvailable")?,
        swap_total: field("SwapTotal")?,
        swap_free: field("SwapFree")?,
    })
}

/// `full avg10` from a PSI file: percent of the last 10 s in which every
/// non-idle task was stalled.
fn parse_full_avg10(text: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix("full "))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

fn assess(memory: Result<Memory, String>, stalled: Option<f64>) -> Reading {
    let m = match memory {
        Ok(m) => m,
        Err(why) => {
            return Reading {
                level: Level::Unknown,
                summary: summary("   ?"),
                detail: vec![format!("Cannot measure: {why}")],
            };
        }
    };
    let used = m.total.saturating_sub(m.available);
    // Rounded, and the level is judged on this same number, so the summary
    // never shows 80% in the colour of 79%.
    let percent = (used * 100 + m.total / 2) / m.total;
    let thrashing = stalled.is_some_and(|s| s > THRASHING_ABOVE);
    let level = if thrashing || percent >= CRITICAL_FROM {
        Level::Critical
    } else if percent >= ATTENTION_FROM {
        Level::Attention
    } else if percent >= NORMAL_FROM {
        Level::Normal
    } else {
        Level::Quiet
    };
    Reading {
        level,
        summary: summary(&format!("{percent:>3}%")),
        detail: vec![
            format!(
                "Used {} of {} (available {})",
                gib(used),
                gib(m.total),
                gib(m.available)
            ),
            if m.swap_total == 0 {
                "No swap".to_owned()
            } else {
                format!(
                    "Swap {} of {}",
                    gib(m.swap_total.saturating_sub(m.swap_free)),
                    gib(m.swap_total)
                )
            },
            match stalled {
                Some(s) => format!("Every task stalled on memory {s:.2}% of the last 10 s"),
                None => "Memory pressure not available (no /proc/pressure/memory)".to_owned(),
            },
        ],
    }
}

/// The value is exactly 4 characters, so every summary has the same width.
fn summary(value: &str) -> String {
    format!("󰍛 {value}")
}

fn gib(kib: u64) -> String {
    format!("{:.1} GiB", kib as f64 / 1_048_576.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = include_str!("../../tests/fixtures/meminfo");
    const PRESSURE: &str = include_str!("../../tests/fixtures/pressure_memory");

    fn used(percent_tenths: u64) -> Memory {
        Memory {
            total: 10_000,
            available: 10_000 - percent_tenths * 10,
            swap_total: 0,
            swap_free: 0,
        }
    }

    #[test]
    fn meminfo_and_pressure_from_this_machine() {
        assert_eq!(
            parse_meminfo(MEMINFO),
            Some(Memory {
                total: 32_766_760,
                available: 25_972_656,
                swap_total: 41_943_036,
                swap_free: 41_943_036,
            })
        );
        assert_eq!(parse_full_avg10(PRESSURE), Some(0.0));
    }

    #[test]
    fn meminfo_without_a_total_is_unusable() {
        assert_eq!(parse_meminfo("MemAvailable: 5 kB\n"), None);
        assert_eq!(
            parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n"),
            None
        );
    }

    #[test]
    fn the_level_follows_the_rounded_percentage_shown() {
        let cases = [
            (494, Level::Quiet, " 49%"),
            (495, Level::Normal, " 50%"),
            (794, Level::Normal, " 79%"),
            (795, Level::Attention, " 80%"),
            (894, Level::Attention, " 89%"),
            (895, Level::Critical, " 90%"),
            (1000, Level::Critical, "100%"),
        ];
        for (tenths, level, shown) in cases {
            let reading = assess(Ok(used(tenths)), Some(0.0));
            assert_eq!(reading.level, level, "{tenths}‰");
            assert_eq!(reading.summary, summary(shown), "{tenths}‰");
        }
    }

    #[test]
    fn thrashing_is_critical_whatever_the_percentage() {
        assert_eq!(assess(Ok(used(300)), Some(1.5)).level, Level::Critical);
        assert_eq!(assess(Ok(used(300)), Some(1.0)).level, Level::Quiet);
        assert_eq!(assess(Ok(used(300)), None).level, Level::Quiet);
    }

    #[test]
    fn unreadable_is_unknown_and_the_same_width() {
        let unknown = assess(Err("/proc/meminfo: gone".to_owned()), None);
        assert_eq!(unknown.level, Level::Unknown);
        assert_eq!(
            unknown.summary.chars().count(),
            assess(Ok(used(500)), None).summary.chars().count()
        );
    }

    #[test]
    fn detail_from_this_machine() {
        let reading = assess(parse_meminfo(MEMINFO).ok_or_else(String::new), Some(0.0));
        assert_eq!(reading.level, Level::Quiet);
        assert_eq!(
            reading.detail,
            [
                "Used 6.5 GiB of 31.2 GiB (available 24.8 GiB)",
                "Swap 0.0 GiB of 40.0 GiB",
                "Every task stalled on memory 0.00% of the last 10 s",
            ]
        );
    }

    #[test]
    fn detail_without_swap_or_pressure() {
        let reading = assess(Ok(used(500)), None);
        assert_eq!(reading.detail[1], "No swap");
        assert!(reading.detail[2].contains("not available"));
    }
}
