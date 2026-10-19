//! Everything an executable does around its gauge: arguments, check or watch,
//! exit status, and stopping cleanly.

use std::io::{self, Write};
use std::process::ExitCode;

use crate::format::{self, Format};
use crate::gauge::Gauge;
use crate::reading::Level;

/// sysexits.h: the command was used incorrectly.
const EX_USAGE: u8 = 64;

#[derive(Debug, PartialEq)]
struct Options {
    watch: bool,
    format: Format,
    floor: Level,
}

/// Options a gauge takes beyond the shared ones.
pub struct Extra {
    /// Appended to the usage line, e.g. `--server URL [--insecure]`.
    pub usage: &'static str,
    /// Appended to `--help`.
    pub help: &'static str,
    /// Each flag, and whether it takes a value.
    pub flags: &'static [(&'static str, bool)],
}

impl Extra {
    const NONE: Extra = Extra {
        usage: "",
        help: "",
        flags: &[],
    };
}

/// The gauge's own options as given on the command line, in order.
#[derive(Debug, Default, PartialEq)]
pub struct Given(Vec<(String, Option<String>)>);

impl Given {
    /// The value of the last occurrence of `flag`.
    pub fn value(&self, flag: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(f, _)| f == flag)
            .and_then(|(_, v)| v.as_deref())
    }

    pub fn required(&self, flag: &str) -> Result<&str, String> {
        self.value(flag)
            .ok_or_else(|| format!("{flag} is required"))
    }

    pub fn has(&self, flag: &str) -> bool {
        self.0.iter().any(|(f, _)| f == flag)
    }
}

/// The whole life of a gauge's executable. `about` is one line for `--help`.
pub fn run(about: &str, gauge: impl Gauge) -> ExitCode {
    run_with(about, &Extra::NONE, |_| Ok(gauge))
}

/// As `run`, for a gauge with options of its own. `build` makes the gauge
/// from them; an error is a usage error.
pub fn run_with<G: Gauge>(
    about: &str,
    extra: &Extra,
    build: impl FnOnce(&Given) -> Result<G, String>,
) -> ExitCode {
    let mut args = std::env::args();
    let name = args
        .next()
        .and_then(|a| a.rsplit('/').next().map(str::to_owned))
        .unwrap_or_else(|| "telltale".to_owned());
    let usage = format!(
        "usage: {name} [--watch] [--format text|waybar] [--floor LEVEL]{}{}",
        if extra.usage.is_empty() { "" } else { " " },
        extra.usage
    );

    let parsed = parse(args, extra).and_then(|p| {
        p.map(|(o, given)| build(&given).map(|g| (o, g)))
            .transpose()
    });
    match parsed {
        Ok(Some((options, gauge))) if options.watch => watch(gauge, &options),
        Ok(Some((options, gauge))) => check(gauge, &options),
        Ok(None) => {
            println!("{name}: {about}\n\n{usage}\n\n{HELP}{}", extra.help);
            ExitCode::SUCCESS
        }
        Err(problem) => {
            eprintln!("{name}: {problem}\n{usage}");
            ExitCode::from(EX_USAGE)
        }
    }
}

const HELP: &str = "\
Without --watch, takes one reading, writes it and exits with the reading's
level: 0 quiet or normal, 1 attention, 2 critical, 3 unknown.

  --watch            keep going: write a new reading whenever it changes
  --format FORMAT    text (default) for a person, waybar for a waybar
                     custom module with return-type json
  --floor LEVEL      write readings below LEVEL empty, so a display shows
                     the gauge only when it matters; LEVEL is quiet, normal,
                     attention or critical. Unknown readings always show.";

/// `Ok(None)` means help was asked for.
fn parse(
    args: impl Iterator<Item = String>,
    extra: &Extra,
) -> Result<Option<(Options, Given)>, String> {
    let mut options = Options {
        watch: false,
        format: Format::Text,
        floor: Level::Quiet,
    };
    let mut given = Given::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg, None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--watch" => options.watch = true,
            "--format" => {
                let v = value()?;
                options.format = Format::parse(&v).ok_or(format!("unknown format '{v}'"))?;
            }
            "--floor" => {
                let v = value()?;
                options.floor = Level::parse_floor(&v).ok_or(format!("unknown floor '{v}'"))?;
            }
            _ => match extra.flags.iter().find(|(f, _)| *f == flag) {
                Some((_, true)) => {
                    let v = value()?;
                    given.0.push((flag, Some(v)));
                }
                Some((_, false)) if inline.is_none() => given.0.push((flag, None)),
                Some((_, false)) => return Err(format!("{flag} takes no value")),
                None => return Err(format!("unknown argument '{flag}'")),
            },
        }
    }
    Ok(Some((options, given)))
}

fn check(mut gauge: impl Gauge, options: &Options) -> ExitCode {
    let reading = gauge.next();
    let text = format::render(&reading, options.format, options.floor);
    // A closed stdout must not change the answer; the exit status still holds.
    let _ = writeln!(io::stdout(), "{text}");
    ExitCode::from(reading.level.exit_status())
}

fn watch(mut gauge: impl Gauge, options: &Options) -> ExitCode {
    die_with_parent();
    let mut out = io::stdout().lock();
    let mut last: Option<String> = None;
    loop {
        let text = format::render(&gauge.next(), options.format, options.floor);
        if last.as_deref() == Some(text.as_str()) {
            continue;
        }
        // Text readings span several lines, so a blank line separates them.
        let separator = if options.format == Format::Text && last.is_some() {
            "\n"
        } else {
            ""
        };
        // Whoever was reading has gone (EPIPE): that is the normal way out.
        if writeln!(out, "{separator}{text}")
            .and_then(|()| out.flush())
            .is_err()
        {
            return ExitCode::SUCCESS;
        }
        last = Some(text);
    }
}

/// A watch only notices a closed pipe on its next write, which for a quiet
/// gauge may be a long way off. Ask the kernel to end us with our parent
/// instead. Note the signal follows the parent *thread* that spawned us.
fn die_with_parent() {
    // SAFETY: getppid has no preconditions; PR_SET_PDEATHSIG only sets a flag
    // on this process.
    unsafe {
        let parent = libc::getppid();
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        // The parent may have died before the prctl took effect.
        if libc::getppid() != parent {
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXTRA: Extra = Extra {
        usage: "--server URL [--insecure]",
        help: "",
        flags: &[("--server", true), ("--insecure", false)],
    };

    fn args(s: &str) -> impl Iterator<Item = String> {
        s.split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn options(s: &str) -> Result<Option<Options>, String> {
        parse(args(s), &Extra::NONE).map(|p| p.map(|(o, _)| o))
    }

    #[test]
    fn defaults_to_a_text_check_with_no_floor() {
        assert_eq!(
            options(""),
            Ok(Some(Options {
                watch: false,
                format: Format::Text,
                floor: Level::Quiet,
            }))
        );
    }

    #[test]
    fn takes_values_separately_or_after_equals() {
        let expected = Ok(Some(Options {
            watch: true,
            format: Format::Waybar,
            floor: Level::Attention,
        }));
        assert_eq!(
            options("--watch --format waybar --floor attention"),
            expected
        );
        assert_eq!(
            options("--floor=attention --format=waybar --watch"),
            expected
        );
    }

    #[test]
    fn rejects_unknown_values_and_arguments() {
        assert!(options("--format json").is_err());
        assert!(options("--floor unknown").is_err());
        assert!(options("--floor").is_err());
        assert!(options("--once").is_err());
        assert!(options("--server x").is_err(), "not this gauge's option");
    }

    #[test]
    fn help_wins() {
        assert_eq!(options("--watch --help"), Ok(None));
    }

    #[test]
    fn a_gauges_own_options_are_handed_over() {
        let (o, given) = parse(args("--watch --server=imap://a --insecure"), &EXTRA)
            .unwrap()
            .unwrap();
        assert!(o.watch);
        assert_eq!(given.value("--server"), Some("imap://a"));
        assert!(given.has("--insecure"));
        assert_eq!(
            given.required("--mailbox"),
            Err("--mailbox is required".to_owned())
        );
        assert!(parse(args("--insecure=yes"), &EXTRA).is_err());
        assert!(parse(args("--server"), &EXTRA).is_err());
    }
}
