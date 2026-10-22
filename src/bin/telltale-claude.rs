use std::process::ExitCode;

use telltale::gauge::claude::{Claude, OPTIONS};

fn main() -> ExitCode {
    telltale::run_with(
        "Claude Code sessions, and whether one is waiting on you",
        &OPTIONS,
        Claude::from_options,
    )
}
