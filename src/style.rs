//! Human-mode styling: when color is on, the ANSI codes picocolors used,
//! and the symbols with their ASCII fallbacks.

use std::io::IsTerminal;

use crate::output::{ColorMode, Globals, Mode};

pub struct Symbols {
    pub active: &'static str,
    pub error: &'static str,
    pub pending: &'static str,
    pub success: &'static str,
    pub warning: &'static str,
}

pub struct Ui {
    pub color: bool,
    pub symbols: Symbols,
}

fn env_is(name: &str, value: &str) -> bool {
    std::env::var_os(name).is_some_and(|actual| actual == value)
}

impl Ui {
    /// Color follows --color, then NO_COLOR, FORCE_COLOR=0 and TERM=dumb,
    /// then whether stderr is a terminal. Machine modes never color.
    pub fn new(globals: &Globals) -> Self {
        let color = match globals.color {
            _ if globals.mode != Mode::Human => false,
            ColorMode::Never => false,
            ColorMode::Always => true,
            ColorMode::Auto => {
                std::env::var_os("NO_COLOR").is_none()
                    && !env_is("FORCE_COLOR", "0")
                    && !env_is("TERM", "dumb")
                    && std::io::stderr().is_terminal()
            }
        };
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .collect::<String>();
        let lower = locale.to_lowercase();
        let unicode = !env_is("TERM", "dumb")
            && (locale.is_empty() || lower.contains("utf-8") || lower.contains("utf8"));
        let symbols = if unicode {
            Symbols {
                active: "●",
                error: "✗",
                pending: "○",
                success: "✓",
                warning: "!",
            }
        } else {
            Symbols {
                active: "[*]",
                error: "[error]",
                pending: "[-]",
                success: "[ok]",
                warning: "[warn]",
            }
        };
        Self { color, symbols }
    }

    /// picocolors' wrapper: a close code inside the text re-opens the style
    /// so nesting survives.
    fn paint(&self, open: &str, close: &str, value: &str) -> String {
        if !self.color {
            return value.to_owned();
        }
        let inner = value.replace(close, &format!("{close}{open}"));
        format!("{open}{inner}{close}")
    }

    pub fn brand(&self, value: &str) -> String {
        self.paint("\x1b[35m", "\x1b[39m", value)
    }
    pub fn command(&self, value: &str) -> String {
        self.paint("\x1b[1m", "\x1b[22m", value)
    }
    pub fn danger(&self, value: &str) -> String {
        self.paint("\x1b[31m", "\x1b[39m", value)
    }
    pub fn flag(&self, value: &str) -> String {
        self.paint("\x1b[36m", "\x1b[39m", value)
    }
    pub fn heading(&self, value: &str) -> String {
        self.paint("\x1b[1m", "\x1b[22m", value)
    }
    pub fn muted(&self, value: &str) -> String {
        self.paint("\x1b[2m", "\x1b[22m", value)
    }
    pub fn warning(&self, value: &str) -> String {
        self.paint("\x1b[33m", "\x1b[39m", value)
    }
}

impl Ui {
    /// A left-aligned table: columns two spaces apart and at most 48 wide
    /// when padded, the header bold, trailing spaces trimmed.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        let width = |text: &str| text.chars().count();
        let widths: Vec<usize> = (0..headers.len())
            .map(|column| {
                let cells = std::iter::once(headers[column].to_owned()).chain(
                    rows.iter()
                        .map(|row| row.get(column).cloned().unwrap_or_default()),
                );
                cells.map(|cell| width(&cell)).max().unwrap_or(0).min(48)
            })
            .collect();
        let render = |cells: Vec<String>, bold: bool| -> String {
            cells
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let padded = format!(
                        "{cell}{}",
                        " ".repeat(widths[column].saturating_sub(width(cell)))
                    );
                    if bold { self.heading(&padded) } else { padded }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        };
        let mut lines = vec![render(
            headers.iter().map(|header| (*header).to_owned()).collect(),
            true,
        )];
        lines.extend(rows.iter().map(|row| render(row.clone(), false)));
        lines.join("\n")
    }
}
