//! Failed systemd units, system and user.
//!
//! Asks `systemctl` every 30 s. A D-Bus subscription would react at once, but
//! systemd has no "failed units changed" signal to subscribe to, and the
//! D-Bus client would outweigh the rest of Telltale. Thirty seconds late is
//! fine for something that waits until you get to it.

use std::process::Command;
use std::time::Duration;

use crate::gauge::Gauge;
use crate::gauge::clock::clock;
use crate::gauge::pace::Pace;
use crate::reading::{Level, Reading};

const EVERY: Duration = Duration::from_secs(30);

pub struct Units {
    pace: Pace,
}

impl Units {
    pub fn new() -> Units {
        Units {
            pace: Pace::every(EVERY),
        }
    }
}

impl Default for Units {
    fn default() -> Units {
        Units::new()
    }
}

impl Gauge for Units {
    fn next(&mut self) -> Reading {
        self.pace.wait();
        let failed = [Scope::System, Scope::User]
            .into_iter()
            .map(failed_in)
            .collect::<Result<Vec<_>, _>>()
            .map(|scopes| scopes.into_iter().flatten().collect());
        assess(failed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Scope {
    System,
    User,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::User => "user",
        }
    }

    fn systemctl(self, args: &[&str]) -> Result<String, String> {
        let mut command = Command::new("systemctl");
        if self == Scope::User {
            command.arg("--user");
        }
        let out = command
            .args(["--no-pager"])
            .args(args)
            .output()
            .map_err(|e| format!("systemctl: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "systemctl{} {}: {}",
                if self == Scope::User { " --user" } else { "" },
                args.join(" "),
                stderr.trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[derive(Debug, PartialEq)]
struct Failed {
    scope: Scope,
    unit: String,
    /// systemd's `Result`: why it failed (`exit-code`, `signal`, `protocol`…).
    result: Option<String>,
    /// Seconds since the epoch at which it entered the failed state.
    since: Option<i64>,
}

fn failed_in(scope: Scope) -> Result<Vec<Failed>, String> {
    let units = parse_failed(&scope.systemctl(&["--failed", "--output=json"])?)?;
    if units.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec![
        "show",
        "--timestamp=unix",
        "--property=Id,Result,StateChangeTimestamp",
        "--",
    ];
    args.extend(units.iter().map(String::as_str));
    // The why and when are extras: the count stands without them.
    let shown = scope
        .systemctl(&args)
        .map(|s| parse_show(&s))
        .unwrap_or_default();
    Ok(units
        .into_iter()
        .map(|unit| {
            let (result, since) = shown
                .iter()
                .find(|(id, ..)| *id == unit)
                .map(|(_, result, since)| (result.clone(), *since))
                .unwrap_or_default();
            Failed {
                scope,
                unit,
                result,
                since,
            }
        })
        .collect())
}

/// Unit names from `systemctl --failed --output=json`.
fn parse_failed(json: &str) -> Result<Vec<String>, String> {
    let units: Vec<serde_json::Value> =
        serde_json::from_str(json).map_err(|e| format!("systemctl --failed: {e}"))?;
    units
        .iter()
        .map(|u| {
            u["unit"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "systemctl --failed: an entry has no unit name".to_owned())
        })
        .collect()
}

/// `(Id, Result, StateChangeTimestamp)` per unit from `systemctl show
/// --timestamp=unix`, which separates units with blank lines and does not
/// keep the order the properties were asked in.
fn parse_show(text: &str) -> Vec<(String, Option<String>, Option<i64>)> {
    text.split("\n\n")
        .filter_map(|block| {
            let get = |key: &str| {
                block.lines().find_map(|line| {
                    line.strip_prefix(key)?
                        .strip_prefix('=')
                        .filter(|v| !v.is_empty())
                })
            };
            Some((
                get("Id")?.to_owned(),
                get("Result").map(str::to_owned),
                get("StateChangeTimestamp")
                    .and_then(|t| t.strip_prefix('@'))
                    .and_then(|t| t.parse().ok()),
            ))
        })
        .collect()
}

fn assess(failed: Result<Vec<Failed>, String>) -> Reading {
    let failed = match failed {
        Ok(f) => f,
        Err(why) => {
            return Reading {
                level: Level::Unknown,
                summary: summary(" ?"),
                detail: vec![format!("Cannot measure: {why}")],
            };
        }
    };
    if failed.is_empty() {
        return Reading {
            level: Level::Quiet,
            summary: summary(" 0"),
            detail: vec!["No failed units, system or user".to_owned()],
        };
    }
    let result = |f: &Failed| f.result.clone().unwrap_or_else(|| "?".to_owned());
    let unit_width = failed.iter().map(|f| f.unit.len()).max().unwrap_or(0);
    let result_width = failed.iter().map(|f| result(f).len()).max().unwrap_or(0);
    let mut detail = vec![format!(
        "{} failed unit{}",
        failed.len(),
        if failed.len() == 1 { "" } else { "s" }
    )];
    detail.extend(failed.iter().map(|f| {
        let since = f.since.map(|t| format!("since {}", clock(t)));
        format!(
            "{:<6}  {:<unit_width$}  {:<result_width$}  {}",
            f.scope.name(),
            f.unit,
            result(f),
            since.unwrap_or_default(),
        )
        .trim_end()
        .to_owned()
    }));
    Reading {
        // Units that systemd restarts never get here; only the ones it gave
        // up on do, and those stay down until someone acts. Broken, so red.
        level: Level::Critical,
        summary: summary(&format!("{:>2}", failed.len())),
        detail,
    }
}

/// The count is 2 characters (a 3-digit count of failed units widens it,
/// rather than being hidden), so the summary does not change width.
fn summary(count: &str) -> String {
    format!("󰀦 {count}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAILED: &str = include_str!("../../tests/fixtures/systemctl_failed.json");
    const SHOW: &str = include_str!("../../tests/fixtures/systemctl_show");
    /// When the fixtures' ghostty unit failed.
    const FAILED_AT: i64 = 1_792_419_356;

    fn failed(scope: Scope, unit: &str, result: &str, since: i64) -> Failed {
        Failed {
            scope,
            unit: unit.to_owned(),
            result: Some(result.to_owned()),
            since: Some(since),
        }
    }

    #[test]
    fn failed_list_and_show_from_this_machine() {
        assert_eq!(
            parse_failed(FAILED),
            Ok(vec!["app-com.mitchellh.ghostty.service".to_owned()])
        );
        assert_eq!(parse_failed("[]"), Ok(vec![]));
        assert_eq!(
            parse_show(SHOW),
            [(
                "app-com.mitchellh.ghostty.service".to_owned(),
                Some("protocol".to_owned()),
                Some(FAILED_AT)
            )]
        );
    }

    #[test]
    fn show_with_several_units_and_missing_values() {
        let text = "Id=a.service\nResult=exit-code\nStateChangeTimestamp=@100\n\n\
                    Result=\nId=b.service\nStateChangeTimestamp=\n";
        assert_eq!(
            parse_show(text),
            [
                (
                    "a.service".to_owned(),
                    Some("exit-code".to_owned()),
                    Some(100)
                ),
                ("b.service".to_owned(), None, None),
            ]
        );
    }

    #[test]
    fn garbage_from_systemctl_is_an_error_not_zero_failures() {
        assert!(parse_failed("").is_err());
        assert!(parse_failed(r#"[{"load":"loaded"}]"#).is_err());
    }

    #[test]
    fn nothing_failed_is_quiet() {
        let reading = assess(Ok(vec![]));
        assert_eq!(reading.level, Level::Quiet);
        assert_eq!(reading.summary, summary(" 0"));
    }

    #[test]
    fn any_failure_is_critical_and_says_which_why_and_when() {
        let reading = assess(Ok(vec![
            failed(
                Scope::User,
                "app-com.mitchellh.ghostty.service",
                "protocol",
                FAILED_AT,
            ),
            failed(
                Scope::System,
                "backup.service",
                "exit-code",
                FAILED_AT + 5_000,
            ),
        ]));
        assert_eq!(reading.level, Level::Critical);
        assert_eq!(reading.summary, summary(" 2"));
        assert_eq!(
            reading.detail,
            [
                "2 failed units".to_owned(),
                format!(
                    "user    app-com.mitchellh.ghostty.service  protocol   since {}",
                    clock(FAILED_AT)
                ),
                format!(
                    "system  backup.service                     exit-code  since {}",
                    clock(FAILED_AT + 5_000)
                ),
            ]
        );
    }

    #[test]
    fn a_failed_systemctl_is_unknown_and_the_same_width() {
        let reading = assess(Err("systemctl --user: no bus".to_owned()));
        assert_eq!(reading.level, Level::Unknown);
        assert_eq!(
            reading.summary.chars().count(),
            assess(Ok(vec![])).summary.chars().count()
        );
    }
}
