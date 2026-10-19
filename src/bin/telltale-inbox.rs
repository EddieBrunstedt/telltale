use std::process::ExitCode;

use telltale::gauge::inbox::{Inbox, OPTIONS};

fn main() -> ExitCode {
    telltale::run_with(
        "messages in an IMAP inbox you keep empty",
        &OPTIONS,
        Inbox::from_options,
    )
}
