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
    pub fn success(&self, value: &str) -> String {
        self.paint("\x1b[32m", "\x1b[39m", value)
    }
    pub fn info(&self, value: &str) -> String {
        self.paint("\x1b[34m", "\x1b[39m", value)
    }
    fn bold_dim(&self, value: &str) -> String {
        self.heading(&self.paint("\x1b[2m", "\x1b[22m", value))
    }
    pub fn warning(&self, value: &str) -> String {
        self.paint("\x1b[33m", "\x1b[39m", value)
    }
}

impl Ui {
    /// A left-aligned table: columns two spaces apart and at most 48 wide
    /// when padded, the header bold, trailing spaces trimmed.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        self.sectioned_table(headers, &[(None, rows.to_vec())])
    }

    /// One table split into titled sections, the columns sized across all
    /// of them.
    pub fn sectioned_table(
        &self,
        headers: &[&str],
        sections: &[(Option<String>, Vec<Vec<String>>)],
    ) -> String {
        let width = |text: &str| visible_width(text);
        let all_rows = sections.iter().flat_map(|(_, rows)| rows.iter());
        let mut widths: Vec<usize> = headers.iter().map(|header| width(header)).collect();
        for row in all_rows {
            for (column, cell) in row.iter().enumerate() {
                if let Some(current) = widths.get_mut(column) {
                    *current = (*current).max(width(cell));
                }
            }
        }
        for current in &mut widths {
            *current = (*current).min(48);
        }
        let render = |cells: &[String], bold: bool| -> String {
            cells
                .iter()
                .enumerate()
                .map(|(column, cell)| {
                    let pad = widths
                        .get(column)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(width(cell));
                    let padded = format!("{cell}{}", " ".repeat(pad));
                    if bold { self.heading(&padded) } else { padded }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_owned()
        };
        let header_cells: Vec<String> = headers.iter().map(|header| (*header).to_owned()).collect();
        let mut lines = vec![render(&header_cells, true)];
        for (title, rows) in sections {
            if let Some(title) = title {
                lines.push(String::new());
                lines.push(self.bold_dim(title));
            }
            lines.extend(rows.iter().map(|row| render(row, false)));
        }
        lines.join("\n")
    }
}

/// Characters on screen, ignoring color codes.
fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut escape = false;
    for character in text.chars() {
        if escape {
            escape = !character.is_ascii_alphabetic();
        } else if character == '\x1b' {
            escape = true;
        } else {
            width += 1;
        }
    }
    width
}
