//! What a gauge reports, and how much it asks of you.

/// Declared in rising order, so `Ord` is the order the floor uses. Unknown is
/// last: a floor never hides a reading the gauge could not take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Quiet,
    Normal,
    Attention,
    Critical,
    Unknown,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Quiet => "quiet",
            Level::Normal => "normal",
            Level::Attention => "attention",
            Level::Critical => "critical",
            Level::Unknown => "unknown",
        }
    }

    /// Accepts only levels that make sense as a floor; a floor of unknown
    /// would hide everything the gauge could measure.
    pub fn parse_floor(s: &str) -> Option<Level> {
        match s {
            "quiet" => Some(Level::Quiet),
            "normal" => Some(Level::Normal),
            "attention" => Some(Level::Attention),
            "critical" => Some(Level::Critical),
            _ => None,
        }
    }

    /// Exit status of a check, after the Nagios plugin convention.
    pub fn exit_status(self) -> u8 {
        match self {
            Level::Quiet | Level::Normal => 0,
            Level::Attention => 1,
            Level::Critical => 2,
            Level::Unknown => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub level: Level,
    /// A few characters, the same width for every reading of a gauge (values
    /// are padded after, on the right).
    pub summary: String,
    /// What was measured, over what window, how fresh. One entry per line.
    pub detail: Vec<String>,
}
