//! Terminal presentation layer.
//!
//! Two rules govern everything here:
//!
//! 1. Machine-readable lines are never decorated. `SP_BLOCKED v1 ...`,
//!    `SP_CONFLICT_JSON=`, `lowest_free: 201`, and `discovered N repository(ies)`
//!    are parsed by agents and scripts, so they stay byte-identical.
//! 2. Colour is emitted only when the stream is a terminal and the operator has
//!    not opted out. Piped output is therefore plain text, which keeps captured
//!    logs and agent transcripts free of escape sequences.

use std::io::IsTerminal;

/// The product mark. Used on human-facing headers only.
pub const CAT: &str = "\u{1F408}";

#[derive(Clone, Copy, Debug)]
pub struct Style {
    enabled: bool,
}

impl Style {
    /// Detects colour support for the given stream.
    ///
    /// `NO_COLOR` always wins: an operator who opted out should not be
    /// overridden by a harness that sets `CLICOLOR_FORCE`.
    pub fn detect(is_terminal: bool) -> Self {
        let opted_out = std::env::var_os("NO_COLOR").is_some();
        let forced = std::env::var_os("CLICOLOR_FORCE")
            .map(|value| value != "0")
            .unwrap_or(false);
        Self {
            enabled: !opted_out && (forced || is_terminal),
        }
    }

    pub fn stdout() -> Self {
        Self::detect(std::io::stdout().is_terminal())
    }

    pub fn stderr() -> Self {
        Self::detect(std::io::stderr().is_terminal())
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint("36", text)
    }

    pub fn good(&self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn bad(&self, text: &str) -> String {
        self.paint("31", text)
    }

    /// `🐈 ScratchPost · <title>` — the single header used by every command.
    /// An empty title renders the bare mark.
    pub fn header(&self, title: &str) -> String {
        if title.is_empty() {
            format!("{CAT} {}", self.bold("ScratchPost"))
        } else {
            format!(
                "{CAT} {} {} {}",
                self.bold("ScratchPost"),
                self.dim("·"),
                self.accent(title)
            )
        }
    }

    /// A dim section label, e.g. `commands`.
    pub fn section(&self, label: &str) -> String {
        self.dim(label)
    }

    /// Colours a claim or session state by severity.
    pub fn state(&self, state: &str) -> String {
        match state {
            "committed" | "active" => self.good(state),
            "reserved" | "waiting" => self.warn(state),
            "released" | "expired" | "evicted" => self.dim(state),
            "blocked" | "conflict" => self.bad(state),
            _ => state.to_string(),
        }
    }

    /// `key=value` with the key dimmed. Value is left untouched so it stays
    /// copy-pasteable.
    pub fn kv(&self, key: &str, value: &str) -> String {
        format!("{}={}", self.dim(key), value)
    }
}

/// Renders a two-column block without padding past the widest key, so short
/// tables stay compact in narrow terminals.
pub fn align(rows: &[(String, String)]) -> Vec<String> {
    let width = rows.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(key, value)| format!("{key:<width$}  {value}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_is_suppressed_when_not_a_terminal() {
        let style = Style::detect(false);
        assert_eq!(style.bold("x"), "x");
        assert_eq!(style.header("status"), "\u{1F408} ScratchPost · status");
        assert_eq!(style.header(""), "\u{1F408} ScratchPost");
    }

    #[test]
    fn header_carries_the_mark_and_title() {
        let style = Style::detect(false);
        let header = style.header("init");
        assert!(header.starts_with(CAT));
        assert!(header.ends_with("init"));
    }

    #[test]
    fn no_color_overrides_force() {
        // SAFETY: single-threaded test process; the variables are restored below.
        std::env::set_var("NO_COLOR", "1");
        std::env::set_var("CLICOLOR_FORCE", "1");
        assert!(!Style::detect(true).enabled);
        std::env::remove_var("NO_COLOR");
        assert!(Style::detect(true).enabled);
        std::env::remove_var("CLICOLOR_FORCE");
        assert!(!Style::detect(false).enabled);
    }

    #[test]
    fn align_pads_to_the_widest_key() {
        let rows = vec![
            ("mode".to_string(), "fcfs".to_string()),
            ("sessions".to_string(), "2".to_string()),
        ];
        let lines = align(&rows);
        assert_eq!(lines[0], "mode      fcfs");
        assert_eq!(lines[1], "sessions  2");
    }
}
