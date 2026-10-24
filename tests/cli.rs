//! Every gauge's real executable, on the real machine, as a check. Readings
//! depend on the machine, so these assert on shape and on the exit status
//! agreeing with the level written, not on values.

use std::process::Command;

/// Each gauge, and the glyphs its summary can start with.
const GAUGES: &[(&str, &[&str])] = &[
    (env!("CARGO_BIN_EXE_telltale-net"), &["󰁅"]),
    (env!("CARGO_BIN_EXE_telltale-mem"), &["󰍛"]),
    (env!("CARGO_BIN_EXE_telltale-disk"), &["󰋊"]),
    (env!("CARGO_BIN_EXE_telltale-units"), &["󰀦"]),
    (env!("CARGO_BIN_EXE_telltale-claude"), &["󰚩"]),
    (
        env!("CARGO_BIN_EXE_telltale-desktop"),
        &["\u{f0687}", "\u{f1272}"],
    ),
];

fn run(exe: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new(exe).args(args).output().expect("runs");
    (
        out.status.code().expect("exits"),
        String::from_utf8(out.stdout).expect("utf-8"),
    )
}

#[test]
fn a_text_check_writes_level_and_summary_and_exits_with_that_level() {
    for &(exe, glyphs) in GAUGES {
        let (status, out) = run(exe, &[]);
        let first = out.lines().next().unwrap_or_default();
        let (level, summary) = first.split_once(": ").expect("level: summary");
        let expected = match level {
            "quiet" | "normal" => 0,
            "attention" => 1,
            "critical" => 2,
            "unknown" => 3,
            other => panic!("{exe}: unknown level {other:?}"),
        };
        assert_eq!(status, expected, "{exe}: {first:?}");
        assert!(
            glyphs.iter().any(|g| summary.starts_with(g)),
            "{exe}: {first:?}"
        );
    }
}

#[test]
fn a_waybar_check_writes_one_json_object() {
    for &(exe, _) in GAUGES {
        let (_, out) = run(exe, &["--format", "waybar"]);
        let v: serde_json::Value = serde_json::from_str(out.trim_end()).expect("json");
        assert!(
            v["text"].is_string() && v["class"].is_string() && v["tooltip"].is_string(),
            "{exe}: {out}"
        );
    }
}

#[test]
fn bad_arguments_exit_64() {
    for &(exe, _) in GAUGES {
        assert_eq!(run(exe, &["--floor", "loud"]).0, 64, "{exe}");
    }
}
