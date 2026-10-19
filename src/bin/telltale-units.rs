use std::process::ExitCode;

use telltale::gauge::units::Units;

fn main() -> ExitCode {
    telltale::run("failed systemd units, system and user", Units::new())
}
