use std::process::ExitCode;

use telltale::gauge::disk::Disk;

fn main() -> ExitCode {
    telltale::run("the fullest local filesystem, as df counts it", Disk::new())
}
