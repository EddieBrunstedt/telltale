//! Steady ticks for gauges that measure on an interval.

use std::thread;
use std::time::{Duration, Instant};

/// Ticks on a fixed grid, so intervals do not drift. After a long stall
/// (suspend) the grid restarts instead of firing a burst of late ticks.
pub(crate) struct Pace {
    every: Duration,
    next: Option<Instant>,
}

impl Pace {
    pub(crate) fn every(every: Duration) -> Pace {
        Pace { every, next: None }
    }

    /// Returns at once the first time, then sleeps to each following tick.
    pub(crate) fn wait(&mut self) {
        let now = Instant::now();
        let tick = match self.next {
            None => now,
            Some(t) if t > now => {
                thread::sleep(t - now);
                t
            }
            Some(t) if now - t < self.every => t,
            Some(_) => now,
        };
        self.next = Some(tick + self.every);
    }
}
