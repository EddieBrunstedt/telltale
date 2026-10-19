//! The gauges. Each one paces itself; `run` only asks for the next reading.

pub mod mem;
pub mod net;
mod pace;

use crate::reading::Reading;

pub trait Gauge {
    /// Blocks until the next reading is due, then returns it. The first call
    /// returns as soon as an honest reading exists (for a rate, after one
    /// window). I/O failures become unknown readings; this never panics.
    fn next(&mut self) -> Reading;
}
