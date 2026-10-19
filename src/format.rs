//! Writing readings out for whoever consumes them.

use crate::reading::{Level, Reading};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// For a person: `level: summary`, then the detail.
    Text,
    /// One JSON object per line, as waybar's `return-type: json` expects.
    Waybar,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "text" => Some(Format::Text),
            "waybar" => Some(Format::Waybar),
            _ => None,
        }
    }
}

/// The reading as written, without a trailing newline. A reading below the
/// floor is written empty: an empty line, or a waybar object with no text,
/// which waybar hides.
pub fn render(reading: &Reading, format: Format, floor: Level) -> String {
    let hidden = reading.level < floor;
    match format {
        Format::Text if hidden => String::new(),
        Format::Text => {
            let mut out = format!("{}: {}", reading.level.name(), reading.summary);
            for line in &reading.detail {
                out.push('\n');
                out.push_str(line);
            }
            out
        }
        Format::Waybar if hidden => r#"{"text":""}"#.to_owned(),
        Format::Waybar => serde_json::json!({
            "text": reading.summary,
            "class": reading.level.name(),
            "tooltip": pango_escape(&reading.detail.join("\n")),
        })
        .to_string(),
    }
}

/// Waybar renders tooltips as Pango markup, so a detail mentioning `<` or `&`
/// (a file name, an error message) would otherwise break or vanish.
fn pango_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(level: Level) -> Reading {
        Reading {
            level,
            summary: "󰋊 92%".to_owned(),
            detail: vec!["/  92%".to_owned(), "a <b> & c".to_owned()],
        }
    }

    #[test]
    fn text_puts_level_and_summary_first_then_detail() {
        assert_eq!(
            render(&reading(Level::Attention), Format::Text, Level::Quiet),
            "attention: 󰋊 92%\n/  92%\na <b> & c"
        );
    }

    #[test]
    fn waybar_maps_summary_level_detail_and_escapes_pango() {
        let out = render(&reading(Level::Critical), Format::Waybar, Level::Quiet);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["text"], "󰋊 92%");
        assert_eq!(v["class"], "critical");
        assert_eq!(v["tooltip"], "/  92%\na &lt;b&gt; &amp; c");
        assert!(!out.contains('\n'), "one object per line");
    }

    #[test]
    fn below_the_floor_is_written_empty() {
        let r = reading(Level::Normal);
        assert_eq!(render(&r, Format::Text, Level::Attention), "");
        assert_eq!(
            render(&r, Format::Waybar, Level::Attention),
            r#"{"text":""}"#
        );
    }

    #[test]
    fn at_the_floor_is_shown() {
        let r = reading(Level::Attention);
        assert_ne!(
            render(&r, Format::Waybar, Level::Attention),
            r#"{"text":""}"#
        );
    }

    #[test]
    fn unknown_is_never_below_any_floor() {
        let r = reading(Level::Unknown);
        assert_ne!(render(&r, Format::Text, Level::Critical), "");
    }
}
