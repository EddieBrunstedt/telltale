//! The fullest local filesystem.
//!
//! "Full" is what `df` means: used against the space you can actually write,
//! so ext4's root reserve does not count as free, and the percentage is
//! rounded up exactly as `df` rounds it. The number can be checked against
//! `df` by eye.

use std::ffi::CString;
use std::fs;
use std::io;
use std::time::Duration;

use crate::gauge::Gauge;
use crate::gauge::pace::Pace;
use crate::reading::{Level, Reading};

const EVERY: Duration = Duration::from_secs(60);
/// Percent used, as shown (rounded up like `df`).
const ATTENTION_FROM: u64 = 90;
const CRITICAL_FROM: u64 = 95;
/// Filesystems that live on a local block device. Pseudo filesystems
/// (proc, tmpfs, overlay) and network mounts are not disks you fill.
const LOCAL: &[&str] = &[
    "ext4", "ext3", "ext2", "btrfs", "xfs", "vfat", "exfat", "f2fs", "ntfs3",
];

pub struct Disk {
    pace: Pace,
}

impl Disk {
    pub fn new() -> Disk {
        Disk {
            pace: Pace::every(EVERY),
        }
    }
}

impl Default for Disk {
    fn default() -> Disk {
        Disk::new()
    }
}

impl Gauge for Disk {
    fn next(&mut self) -> Reading {
        self.pace.wait();
        let filesystems = fs::read_to_string("/proc/self/mounts")
            .map_err(|e| format!("/proc/self/mounts: {e}"))
            .map(|text| {
                parse_mounts(&text)
                    .into_iter()
                    .map(|mount| {
                        let space = statvfs(&mount.target);
                        (mount, space)
                    })
                    .collect()
            });
        assess(filesystems)
    }
}

#[derive(Debug, PartialEq)]
struct Mount {
    source: String,
    target: String,
    fstype: String,
}

/// Each local filesystem with its space, or why it could not be measured.
type Measured = Vec<(Mount, Result<Space, String>)>;

/// In bytes.
#[derive(Debug, PartialEq)]
struct Space {
    size: u64,
    used: u64,
    available: u64,
}

impl Space {
    /// Used against used plus what you can still write, rounded up: `df`'s
    /// `Use%`.
    fn percent(&self) -> u64 {
        let usable = self.used + self.available;
        if usable == 0 {
            return 0;
        }
        (self.used * 100).div_ceil(usable)
    }
}

/// Local filesystems, one per device: a device mounted twice (a bind mount,
/// btrfs subvolumes) has one fullness, kept under its shortest path.
fn parse_mounts(text: &str) -> Vec<Mount> {
    let mut mounts: Vec<Mount> = text
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (source, target, fstype) = (f.next()?, f.next()?, f.next()?);
            (source.starts_with("/dev/") && LOCAL.contains(&fstype)).then(|| Mount {
                source: unescape(source),
                target: unescape(target),
                fstype: fstype.to_owned(),
            })
        })
        .collect();
    mounts.sort_by(|a, b| (a.target.len(), &a.target).cmp(&(b.target.len(), &b.target)));
    let mut seen = Vec::new();
    mounts.retain(|m| {
        let first = !seen.contains(&m.source);
        seen.push(m.source.clone());
        first
    });
    mounts
}

/// `/proc/self/mounts` writes space, tab, newline and backslash in paths as
/// three-digit octal escapes (`\040`).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let octal = bytes
            .get(i + 1..i + 4)
            .filter(|d| bytes[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal.and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok()) {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn statvfs(path: &str) -> Result<Space, String> {
    let c_path = CString::new(path).map_err(|e| e.to_string())?;
    // SAFETY: statvfs only writes into the struct we hand it, and c_path is a
    // valid NUL-terminated string for the duration of the call.
    let s = unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut s) != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        s
    };
    let block = s.f_frsize as u64;
    Ok(Space {
        size: s.f_blocks as u64 * block,
        used: (s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * block,
        available: s.f_bavail as u64 * block,
    })
}

fn assess(filesystems: Result<Measured, String>) -> Reading {
    let filesystems = match filesystems {
        Ok(f) if f.is_empty() => Err("no local filesystems are mounted".to_owned()),
        other => other,
    };
    let mut filesystems = match filesystems {
        Ok(f) => f,
        Err(why) => return unknown(vec![format!("Cannot measure: {why}")]),
    };
    // Fullest first; filesystems that could not be read go to the top, since
    // they are what the reading is about.
    filesystems.sort_by_key(|(_, space)| {
        std::cmp::Reverse(space.as_ref().map_or(u64::MAX, Space::percent))
    });
    let width = filesystems
        .iter()
        .map(|(m, _)| m.target.chars().count())
        .max()
        .unwrap_or(0);
    let detail: Vec<String> = filesystems
        .iter()
        .map(|(m, space)| {
            let device = m.source.rsplit('/').next().unwrap_or(&m.source);
            match space {
                Ok(s) => format!(
                    "{:<width$}  {:>3}%  {} free of {}  ({}, {device})",
                    m.target,
                    s.percent(),
                    binary(s.available),
                    binary(s.size),
                    m.fstype,
                ),
                Err(why) => format!("{:<width$}  cannot measure: {why}", m.target),
            }
        })
        .collect();

    // An unreadable filesystem leaves "the fullest" unknowable.
    let Some(fullest) = filesystems
        .iter()
        .map(|(_, space)| space.as_ref().ok().map(Space::percent))
        .collect::<Option<Vec<u64>>>()
        .and_then(|percents| percents.into_iter().max())
    else {
        return unknown(detail);
    };
    let level = if fullest >= CRITICAL_FROM {
        Level::Critical
    } else if fullest >= ATTENTION_FROM {
        Level::Attention
    } else {
        Level::Normal
    };
    Reading {
        level,
        summary: summary(&format!("{fullest:>3}%")),
        detail,
    }
}

fn unknown(detail: Vec<String>) -> Reading {
    Reading {
        level: Level::Unknown,
        summary: summary("   ?"),
        detail,
    }
}

/// The value is exactly 4 characters, so every summary has the same width.
fn summary(value: &str) -> String {
    format!("󰋊 {value}")
}

/// Binary units with three significant figures: `172 GiB`, `3.64 TiB`.
fn binary(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 999.5 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    let u = UNITS[unit];
    if unit == 0 {
        format!("{bytes} {u}")
    } else if v < 9.995 {
        format!("{v:.2} {u}")
    } else if v < 99.95 {
        format!("{v:.1} {u}")
    } else {
        format!("{v:.0} {u}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = include_str!("../../tests/fixtures/mounts");

    fn mount(target: &str) -> Mount {
        Mount {
            source: format!("/dev/x{}", target.len()),
            target: target.to_owned(),
            fstype: "ext4".to_owned(),
        }
    }

    /// `percent` of a 100-byte filesystem with no reserve.
    fn space(percent: u64) -> Result<Space, String> {
        Ok(Space {
            size: 100,
            used: percent,
            available: 100 - percent,
        })
    }

    #[test]
    fn mounts_from_this_machine_are_the_three_local_filesystems() {
        let targets: Vec<(String, String)> = parse_mounts(MOUNTS)
            .into_iter()
            .map(|m| (m.target, m.fstype))
            .collect();
        assert_eq!(
            targets,
            [
                ("/".to_owned(), "ext4".to_owned()),
                ("/boot".to_owned(), "vfat".to_owned()),
                ("/data/shared".to_owned(), "ntfs3".to_owned()),
            ]
        );
    }

    #[test]
    fn one_entry_per_device_under_its_shortest_path() {
        let text = "/dev/sda2 /home btrfs rw 0 0\n\
                    /dev/sda2 / btrfs rw 0 0\n\
                    /dev/sdb1 /media/my\\040disk ext4 rw 0 0\n\
                    tmpfs /tmp tmpfs rw 0 0\n\
                    server:/x /mnt/nfs nfs4 rw 0 0\n";
        let targets: Vec<String> = parse_mounts(text).into_iter().map(|m| m.target).collect();
        assert_eq!(targets, ["/", "/media/my disk"]);
    }

    #[test]
    fn percent_matches_df_on_this_machine() {
        // `df -B1 --output=target,used,avail,pcent` on 2026-10-19.
        let df = |used, available| {
            Space {
                size: 0,
                used,
                available,
            }
            .percent()
        };
        assert_eq!(df(241_455_890_432, 183_497_179_136), 57); // /
        assert_eq!(df(148_545_536, 923_078_656), 14); // /boot
        assert_eq!(df(2_822_365_765_632, 1_178_402_557_952), 71); // /data/shared
    }

    #[test]
    fn percent_rounds_up_like_df_and_survives_an_empty_filesystem() {
        let p = |used, available| {
            Space {
                size: 0,
                used,
                available,
            }
            .percent()
        };
        assert_eq!(p(901, 99), 91); // 90.1% shows as 91%
        assert_eq!(p(90, 10), 90);
        assert_eq!(p(0, 0), 0);
    }

    #[test]
    fn levels_come_from_the_fullest_filesystem() {
        let level = |percents: &[u64]| {
            let filesystems = percents
                .iter()
                .enumerate()
                .map(|(i, &p)| (mount(&"/m".repeat(i + 1)), space(p)))
                .collect();
            assess(Ok(filesystems)).level
        };
        assert_eq!(level(&[57, 14, 89]), Level::Normal);
        assert_eq!(level(&[57, 90]), Level::Attention);
        assert_eq!(level(&[94, 14]), Level::Attention);
        assert_eq!(level(&[95, 14]), Level::Critical);
    }

    #[test]
    fn one_unreadable_filesystem_makes_the_reading_unknown() {
        let reading = assess(Ok(vec![
            (mount("/"), space(57)),
            (
                mount("/boot"),
                Err("Permission denied (os error 13)".to_owned()),
            ),
        ]));
        assert_eq!(reading.level, Level::Unknown);
        assert_eq!(reading.summary, summary("   ?"));
        assert_eq!(
            reading.detail[0],
            "/boot  cannot measure: Permission denied (os error 13)"
        );
        assert_eq!(assess(Ok(vec![])).level, Level::Unknown);
        assert_eq!(assess(Err("gone".to_owned())).level, Level::Unknown);
    }

    #[test]
    fn detail_is_aligned_and_fullest_first() {
        let fs =
            |source: &str, target: &str, fstype: &str, used: u64, available: u64, size: u64| {
                (
                    Mount {
                        source: source.to_owned(),
                        target: target.to_owned(),
                        fstype: fstype.to_owned(),
                    },
                    Ok(Space {
                        size,
                        used,
                        available,
                    }),
                )
            };
        let reading = assess(Ok(vec![
            fs(
                "/dev/sda3",
                "/",
                "ext4",
                241_455_890_432,
                183_497_179_136,
                447_774_015_488,
            ),
            fs(
                "/dev/sda1",
                "/boot",
                "vfat",
                148_545_536,
                923_078_656,
                1_071_624_192,
            ),
            fs(
                "/dev/nvme1n1p2",
                "/data/shared",
                "ntfs3",
                2_822_365_765_632,
                1_178_402_557_952,
                4_000_768_323_584,
            ),
        ]));
        assert_eq!(reading.level, Level::Normal);
        assert_eq!(reading.summary, summary(" 71%"));
        assert_eq!(
            reading.detail,
            [
                "/data/shared   71%  1.07 TiB free of 3.64 TiB  (ntfs3, nvme1n1p2)",
                "/              57%  171 GiB free of 417 GiB  (ext4, sda3)",
                "/boot          14%  880 MiB free of 1.00 GiB  (vfat, sda1)",
            ]
        );
    }

    #[test]
    fn every_summary_has_the_same_width() {
        let width = |r: Reading| r.summary.chars().count();
        let normal = width(assess(Ok(vec![(mount("/"), space(5))])));
        assert_eq!(width(assess(Ok(vec![(mount("/"), space(100))]))), normal);
        assert_eq!(width(assess(Err(String::new()))), normal);
    }
}
