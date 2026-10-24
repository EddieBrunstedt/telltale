use std::process::ExitCode;

use telltale::gauge::desktop::{Desktop, OPTIONS};

fn main() -> ExitCode {
    telltale::run_with(
        "~/desktop, a drawer you keep empty, and whether anything has outstayed a boot",
        &OPTIONS,
        Desktop::from_options,
    )
}
