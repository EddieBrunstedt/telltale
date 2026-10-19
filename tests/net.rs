//! The real executable, on the real machine, as a check.

use std::process::Command;

fn telltale_net(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_telltale-net"))
        .args(args)
        .output()
        .expect("runs");
    (
        out.status.code().expect("exits"),
        String::from_utf8(out.stdout).expect("utf-8"),
    )
}

#[test]
fn a_text_check_writes_level_and_summary_and_exits_with_the_level() {
    let (status, out) = telltale_net(&[]);
    let first = out.lines().next().unwrap_or_default();
    let expected = match status {
        0 => ["quiet: ", "normal: "].as_slice(),
        2 => &["critical: "],
        3 => &["unknown: "],
        other => panic!("net never asks for attention, got status {other}"),
    };
    assert!(
        expected.iter().any(|p| first.starts_with(p)),
        "status {status}, first line {first:?}"
    );
    assert!(first.contains('󰁅'), "{first:?}");
}

#[test]
fn a_waybar_check_writes_one_json_object() {
    let (_, out) = telltale_net(&["--format", "waybar"]);
    let v: serde_json::Value = serde_json::from_str(out.trim_end()).expect("json");
    assert!(v["text"].is_string() && v["class"].is_string() && v["tooltip"].is_string());
}

#[test]
fn bad_arguments_exit_64() {
    assert_eq!(telltale_net(&["--floor", "loud"]).0, 64);
}
