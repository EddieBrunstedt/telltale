use std::process::ExitCode;

use telltale::gauge::net::Net;

fn main() -> ExitCode {
    telltale::run(
        "traffic on the interface that carries the default route",
        Net::new(),
    )
}
