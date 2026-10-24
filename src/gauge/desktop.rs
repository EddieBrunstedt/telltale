//! `~/desktop`, a drawer you keep empty.
//!
//! Anything in it is waiting to be filed or deleted. What was dropped there
//! this boot is work in progress; what was already there before it has been
//! forgotten. An entry's ctime tells the two apart without any state of our
//! own: creating, copying or moving an entry into the drawer sets it, so an
//! entry whose ctime is older than boot has been there since an earlier one.
//!
//! Watching costs nothing between changes: inotify on the drawer.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::gauge::Gauge;
use crate::gauge::clock::clock;
use crate::gauge::inotify;
use crate::reading::{Level, Reading};
use crate::run::{Extra, Given};

/// How long to wait before trying again to watch a drawer that could not be
/// watched, including one that does not exist yet.
const RETRY_EVERY: Duration = Duration::from_secs(60);
/// Names are cut to this many characters, so one long name does not widen
/// every row.
const NAME_WIDTH: usize = 40;
/// Rows listed in the detail, across both groups.
const ROWS: usize = 12;

// nf-md-inbox / nf-md-inbox_full: the same width, so the bar does not shift.
const EMPTY: &str = "\u{f0687}";
const FULL: &str = "\u{f1272}";

/// No options of its own, but a check must not set up a watch, so it needs
/// to know whether it is being watched.
pub const OPTIONS: Extra = Extra {
    usage: "",
    help: "",
    flags: &[],
};

pub struct Desktop {
    home: Option<String>,
    watching: bool,
    inotify: Option<OwnedFd>,
    /// The last attempt to watch failed, so the next one waits first.
    failed: bool,
}

impl Desktop {
    pub fn from_options(given: &Given) -> Result<Desktop, String> {
        Ok(Desktop {
            home: std::env::var("HOME").ok(),
            watching: given.watching(),
            inotify: None,
            failed: false,
        })
    }

    fn dir(&self) -> Result<PathBuf, String> {
        let home = self.home.as_ref().ok_or("$HOME is not set")?;
        Ok(Path::new(home).join("desktop"))
    }

    /// Starts watching, waiting first if the last attempt failed. A drawer
    /// that does not exist is not an error: the reading says so, and the
    /// watch is tried again later.
    fn watch(&mut self) -> Result<(), String> {
        if self.failed {
            thread::sleep(RETRY_EVERY);
        }
        // Entries arriving and leaving, and anything that changes a direct
        // entry's ctime (chmod, touch, a write).
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_TO
            | libc::IN_MOVED_FROM
            | libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        match inotify::watch(&self.dir()?, mask) {
            Ok(fd) => {
                self.inotify = Some(fd);
                self.failed = false;
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                match e.kind() {
                    io::ErrorKind::NotFound => Ok(()),
                    _ => Err(format!("cannot watch ~/desktop: {e}")),
                }
            }
        }
    }

    /// Blocks until the drawer changes. Returns whether it is still there.
    fn wait(inotify: &OwnedFd) -> bool {
        let mut fd = libc::pollfd {
            fd: inotify.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: fd is one live pollfd.
            if unsafe { libc::poll(&mut fd, 1, -1) } >= 0 {
                return inotify::drain(inotify);
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                // Not expected; start over rather than spin.
                return false;
            }
        }
    }

    /// The drawer's entries, or `None` when it does not exist.
    fn read(&self) -> Result<Option<Vec<Entry>>, String> {
        match read_entries(&self.dir()?) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            found => found.map(Some).map_err(|e| format!("~/desktop: {e}")),
        }
    }
}

impl Gauge for Desktop {
    fn next(&mut self) -> Reading {
        if self.watching {
            if let Some(fd) = &self.inotify
                && !Desktop::wait(fd)
            {
                // The drawer went away; the watch went with it.
                self.inotify = None;
            }
            if self.inotify.is_none()
                && let Err(why) = self.watch()
            {
                return assess(Err(why), Ok(0));
            }
        }
        assess(self.read(), boot_time())
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    name: String,
    /// Seconds since the epoch.
    ctime: i64,
}

/// Every entry, hidden ones included. Symlinks are not followed: a link
/// arrived when it was made, whatever it points at, and a dangling one is
/// still something to clear out.
fn read_entries(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            // Removed between listing and looking.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            ctime: meta.ctime(),
        });
    }
    Ok(entries)
}

fn boot_time() -> Result<i64, String> {
    let stat = fs::read_to_string("/proc/stat").map_err(|e| format!("/proc/stat: {e}"))?;
    btime(&stat).ok_or_else(|| "/proc/stat: no btime line".to_owned())
}

/// Boot time in seconds since the epoch, from `/proc/stat`'s `btime` line.
fn btime(stat: &str) -> Option<i64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|t| t.trim().parse().ok())
}

/// `boot` is only needed when the drawer holds something, so a boot time
/// that cannot be read makes only such a reading unknown.
fn assess(found: Result<Option<Vec<Entry>>, String>, boot: Result<i64, String>) -> Reading {
    let unknown = |why: String| Reading {
        level: Level::Unknown,
        summary: FULL.to_owned(),
        detail: vec![format!("Cannot measure: {why}")],
    };
    let empty = |detail: &str| Reading {
        level: Level::Quiet,
        summary: EMPTY.to_owned(),
        detail: vec![detail.to_owned()],
    };
    let mut entries = match found {
        Err(why) => return unknown(why),
        Ok(None) => return empty("~/desktop does not exist"),
        Ok(Some(entries)) if entries.is_empty() => return empty("~/desktop is empty"),
        Ok(Some(entries)) => entries,
    };
    let boot = match boot {
        Ok(boot) => boot,
        Err(why) => return unknown(why),
    };
    // Oldest first, so the left over come first; then by name, so rows with
    // equal times never swap places between readings.
    entries.sort_by(|a, b| (a.ctime, &a.name).cmp(&(b.ctime, &b.name)));
    let left = entries.iter().take_while(|e| e.ctime < boot).count();
    Reading {
        level: if left > 0 {
            Level::Attention
        } else {
            Level::Normal
        },
        summary: FULL.to_owned(),
        detail: detail(&entries, left),
    }
}

/// A group per kind, each headed by its count, with a row per entry: its
/// name and when it arrived. At most ROWS rows in all; the left over are
/// listed first, since they are the ones to deal with.
fn detail(entries: &[Entry], left: usize) -> Vec<String> {
    let (old, new) = entries.split_at(left);
    let shown_old = old.len().min(ROWS);
    let shown_new = new.len().min(ROWS - shown_old);
    let width = old[..shown_old]
        .iter()
        .chain(&new[..shown_new])
        .map(|e| name(e).chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = Vec::new();
    let mut group = |heading: String, entries: &[Entry], shown: usize| {
        if entries.is_empty() {
            return;
        }
        lines.push(heading);
        for e in &entries[..shown] {
            lines.push(format!("  {:<width$}  {}", name(e), clock(e.ctime)));
        }
        if shown < entries.len() {
            lines.push(format!("  … and {} more", entries.len() - shown));
        }
    };
    group(
        format!("{} left over from before this boot", old.len()),
        old,
        shown_old,
    );
    group(format!("{} dropped this boot", new.len()), new, shown_new);
    lines
}

/// The name as a row can show it: control characters (a newline would
/// split the row) as `?`, cut to NAME_WIDTH.
fn name(e: &Entry) -> String {
    let clean: String = e
        .name
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if clean.chars().count() > NAME_WIDTH {
        clean.chars().take(NAME_WIDTH - 1).chain(['…']).collect()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This machine's, 2026-10-24.
    const STAT: &str = include_str!("../../tests/fixtures/proc_stat");
    const BOOT: i64 = 1_792_800_230;

    fn entry(name: &str, ctime: i64) -> Entry {
        Entry {
            name: name.to_owned(),
            ctime,
        }
    }

    fn assess_entries(entries: Vec<Entry>) -> Reading {
        assess(Ok(Some(entries)), Ok(BOOT))
    }

    #[test]
    fn boot_time_from_this_machine() {
        assert_eq!(btime(STAT), Some(BOOT));
        assert_eq!(btime("cpu  1 2 3\n"), None);
    }

    #[test]
    fn empty_and_missing_are_quiet_and_say_which() {
        let empty = assess_entries(vec![]);
        assert_eq!(empty.level, Level::Quiet);
        assert_eq!(empty.summary, EMPTY);
        assert_eq!(empty.detail, ["~/desktop is empty"]);

        let missing = assess(Ok(None), Ok(BOOT));
        assert_eq!(missing.level, Level::Quiet);
        assert_eq!(missing.summary, EMPTY);
        assert_eq!(missing.detail, ["~/desktop does not exist"]);
    }

    #[test]
    fn dropped_this_boot_is_normal() {
        let reading = assess_entries(vec![entry("shot.png", BOOT), entry("notes", BOOT + 60)]);
        assert_eq!(
            reading.level,
            Level::Normal,
            "arriving at boot is this boot"
        );
        assert_eq!(reading.summary, FULL);
        assert_eq!(
            reading.detail,
            [
                "2 dropped this boot".to_owned(),
                format!("  shot.png  {}", clock(BOOT)),
                format!("  notes     {}", clock(BOOT + 60)),
            ]
        );
    }

    #[test]
    fn left_over_asks_for_attention_and_comes_first() {
        let reading = assess_entries(vec![
            entry("shot.png", BOOT + 60),
            entry("report.pdf", BOOT - 1),
            entry("old", BOOT - 86_400),
        ]);
        assert_eq!(reading.level, Level::Attention);
        assert_eq!(reading.summary, FULL);
        assert_eq!(
            reading.detail,
            [
                "2 left over from before this boot".to_owned(),
                format!("  old         {}", clock(BOOT - 86_400)),
                format!("  report.pdf  {}", clock(BOOT - 1)),
                "1 dropped this boot".to_owned(),
                format!("  shot.png    {}", clock(BOOT + 60)),
            ]
        );
    }

    #[test]
    fn equal_times_go_by_name() {
        let reading = assess_entries(vec![entry("b", BOOT), entry("a", BOOT)]);
        let names: Vec<&str> = reading.detail[1..]
            .iter()
            .map(|row| row.split_whitespace().next().unwrap())
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn rows_stop_at_twelve_left_over_first() {
        let mut entries: Vec<Entry> = (0..10)
            .map(|i| entry(&format!("old{i}"), BOOT - 100 + i))
            .collect();
        entries.extend((0..5).map(|i| entry(&format!("new{i}"), BOOT + i)));
        let detail = assess_entries(entries).detail;
        assert_eq!(detail.len(), 1 + 10 + 1 + 2 + 1);
        assert_eq!(detail[11], "5 dropped this boot");
        assert_eq!(detail.last().unwrap(), "  … and 3 more");

        let many: Vec<Entry> = (0..20)
            .map(|i| entry(&format!("{i:02}"), BOOT - 100 + i))
            .collect();
        let detail = assess_entries(many).detail;
        assert_eq!(detail.len(), 1 + 12 + 1);
        assert_eq!(detail.last().unwrap(), "  … and 8 more");
    }

    #[test]
    fn names_are_cut_and_cannot_break_a_row() {
        assert_eq!(name(&entry("two\nlines", 0)), "two?lines");
        let long = "a".repeat(NAME_WIDTH + 5);
        let shown = name(&entry(&long, 0));
        assert_eq!(shown.chars().count(), NAME_WIDTH);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn cannot_read_is_unknown_and_says_why() {
        let reading = assess(
            Err("~/desktop: Permission denied (os error 13)".to_owned()),
            Ok(BOOT),
        );
        assert_eq!(reading.level, Level::Unknown);
        assert_eq!(reading.summary, FULL);
        assert_eq!(
            reading.detail,
            ["Cannot measure: ~/desktop: Permission denied (os error 13)"]
        );
    }

    #[test]
    fn boot_time_matters_only_when_something_is_there() {
        let no_boot = || Err("/proc/stat: no btime line".to_owned());
        assert_eq!(assess(Ok(Some(vec![])), no_boot()).level, Level::Quiet);
        let reading = assess(Ok(Some(vec![entry("a", BOOT)])), no_boot());
        assert_eq!(reading.level, Level::Unknown);
        assert_eq!(
            reading.detail,
            ["Cannot measure: /proc/stat: no btime line"]
        );
    }

    #[test]
    fn hidden_entries_count_and_symlinks_are_not_followed() {
        let dir = std::env::temp_dir().join(format!("telltale-desktop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".hidden"), "").unwrap();
        std::os::unix::fs::symlink("nowhere", dir.join("dangling")).unwrap();
        let mut names: Vec<String> = read_entries(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        fs::remove_dir_all(&dir).unwrap();
        names.sort();
        assert_eq!(names, [".hidden", "dangling"]);
    }
}
