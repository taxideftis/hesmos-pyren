//! Terminal rendering tokens — the ONE place that knows about colors, symbols and
//! number/hash formatting (code-structure §3: presentation logic, nothing else).
//!
//! Style rules: `NO_COLOR` (https://no-color.org) or a non-TTY stdout forces plain
//! output — scripts and CI must never parse ANSI escapes. Every glyph used here has a
//! plain fallback, so the two styles render the same INFORMATION.

use std::io::IsTerminal;

/// Resolved output style (probed once per command invocation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// `NO_COLOR` wins over TTY detection; a piped stdout is plain by definition.
    pub fn detect() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        Self { color }
    }

    pub fn green(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[32m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn red(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[31m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn yellow(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[33m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn dim(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[2m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Verdict glyph with a plain fallback (`✓` / `✗` / `↻` vs `ok` / `x` / `~`).
    pub fn pass(&self) -> String {
        self.green("✓")
    }
    pub fn fail(&self) -> String {
        self.red("✗")
    }
    pub fn retry(&self) -> String {
        self.yellow("↻")
    }
}

/// Thousands separators (표준 요약: `250,000`) — grouping only, no locale logic.
pub fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A 22-char meter bar (`■■■■□□□□…`) — spent fraction filled, limit-relative. An
/// uncapped budget renders as `-` (there is no fraction to draw).
pub fn bar(spent: u64, limit: Option<u64>, style: &Style) -> String {
    const WIDTH: usize = 22;
    let Some(limit) = limit else {
        return style.dim(&"-".repeat(WIDTH));
    };
    let filled = if limit == 0 {
        WIDTH
    } else {
        ((spent.min(limit) as f64 / limit as f64) * WIDTH as f64).round() as usize
    };
    format!(
        "{}{}",
        style.yellow(&"■".repeat(filled)),
        style.dim(&"□".repeat(WIDTH - filled))
    )
}

/// Chain/contract hash summary — first 8 hex chars (`ab12cd34…`). The full hash stays
/// available via `--json`; the short form is for humans scanning a trace.
pub fn hash8(hash: &str) -> String {
    let mut short = hash.chars().take(8).collect::<String>();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commas_group_by_thousands() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(250_000_000), "250,000,000");
    }

    #[test]
    fn hash8_truncates_to_eight_chars() {
        assert_eq!(hash8(&"a".repeat(64)), "aaaaaaaa…");
    }

    /// Plain style (piped / NO_COLOR) emits no escape sequences — CI-safe by default.
    #[test]
    fn plain_style_has_no_ansi() {
        let style = Style { color: false };
        assert_eq!(style.pass(), "✓");
        assert_eq!(style.dim("x"), "x");
        assert_eq!(bar(50, Some(100), &style), "■".repeat(11) + &"□".repeat(11));
        assert_eq!(bar(0, None, &style), "-".repeat(22));
    }

    #[test]
    fn bar_clamps_at_the_limit() {
        let style = Style { color: false };
        assert_eq!(bar(500, Some(100), &style), "■".repeat(22));
        assert_eq!(bar(0, Some(100), &style), "□".repeat(22));
    }
}
