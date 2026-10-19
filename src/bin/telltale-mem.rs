use std::process::ExitCode;

use telltale::gauge::mem::Mem;

fn main() -> ExitCode {
    telltale::run(
        "memory in use: the total minus what the kernel reports as available",
        Mem::new(),
    )
}
