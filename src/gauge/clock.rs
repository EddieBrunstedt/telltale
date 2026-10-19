//! Wall-clock times for details.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch, now.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `HH:MM` in local time, with the date in front (`2026-10-17 16:15`) when it
/// is not today. A clock time rather than an age: the detail is written once
/// and read later, and "3 min ago" would go stale on screen.
pub(crate) fn clock(epoch: i64) -> String {
    let (Some(then), Some(today)) = (local(epoch), local(now())) else {
        return "??:??".to_owned();
    };
    let time = format!("{:02}:{:02}", then.tm_hour, then.tm_min);
    if (then.tm_year, then.tm_yday) == (today.tm_year, today.tm_yday) {
        time
    } else {
        format!(
            "{}-{:02}-{:02} {time}",
            then.tm_year + 1900,
            then.tm_mon + 1,
            then.tm_mday
        )
    }
}

fn local(epoch: i64) -> Option<libc::tm> {
    let t = epoch as libc::time_t;
    // SAFETY: localtime_r only writes the struct we hand it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&t, &mut tm).is_null()).then_some(tm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_is_the_time_alone() {
        let shown = clock(now());
        assert_eq!(shown.len(), 5, "{shown}");
        assert_eq!(&shown[2..3], ":");
    }

    #[test]
    fn another_day_has_its_date() {
        // Noon UTC, so the date holds in any time zone up to 11 h away.
        let shown = clock(1_760_011_200);
        assert_eq!(shown.len(), 16, "{shown}");
        assert!(shown.starts_with("2025-10-09 "), "{shown}");
    }
}
