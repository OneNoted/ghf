//! Terminal and JSON output.

use std::{
    borrow::Cow,
    fmt::Write as _,
    io::{self, Write},
};

use anstyle::Style;
use serde::Serialize;

use crate::{
    classify::Category,
    summary::{Summary, Totals},
};

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Source {
    Github {
        repository: String,
        #[serde(rename = "ref")]
        reference: String,
        default_branch: bool,
        commit: String,
    },
    Directory {
        path: String,
    },
    Archive {
        path: String,
        commit: Option<String>,
    },
}

pub fn json(out: &mut impl Write, source: &Source, summary: &Summary) -> io::Result<()> {
    #[derive(Serialize)]
    struct Report<'a> {
        /// Bumped on incompatible changes to this layout.
        schema: u32,
        ghf: &'static str,
        source: &'a Source,
        #[serde(flatten)]
        summary: &'a Summary<'a>,
    }
    let report = Report {
        schema: 1,
        ghf: env!("CARGO_PKG_VERSION"),
        source,
        summary,
    };
    let text = serde_json::to_string_pretty(&report)?;
    // serde_json escapes C0 controls; also escape DEL, C1 and bidi controls,
    // which can only occur inside strings, so the JSON stays equivalent.
    let mut safe = String::with_capacity(text.len());
    for c in text.chars() {
        if unsafe_char(c) && c >= '\u{7f}' {
            let _ = write!(safe, "\\u{:04x}", c as u32);
        } else {
            safe.push(c);
        }
    }
    writeln!(out, "{safe}")
}

fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
}

/// Escapes control and bidirectional-override characters so file names and
/// refs cannot drive the terminal.
pub fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.chars().any(unsafe_char) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if unsafe_char(c) {
            let _ = write!(out, "{}", c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

const BOLD: Style = Style::new().bold();
const DIM: Style = Style::new().dimmed();

/// A plain-text table with right-aligned numeric columns that drops columns
/// from the right, then truncates the first column, to fit `width`.
struct Table {
    head: Vec<&'static str>,
    rows: Vec<(Vec<String>, Style)>,
    /// The trailing free-text column (file paths), if any.
    tail: Option<&'static str>,
}

impl Table {
    fn render(&self, out: &mut String, width: Option<usize>) {
        let lead = self
            .head
            .iter()
            .take_while(|h| h.is_empty() || !is_numeric(h))
            .count()
            .max(1);
        let mut widths: Vec<usize> = (0..self.head.len())
            .map(|i| {
                self.rows
                    .iter()
                    .map(|(r, _)| r[i].chars().count())
                    .chain([self.head[i].len()])
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let mut shown: Vec<usize> = (0..self.head.len()).collect();
        let total = |shown: &[usize], widths: &[usize]| {
            shown.iter().map(|&i| widths[i]).sum::<usize>() + 2 * shown.len().saturating_sub(1)
        };
        if let Some(width) = width {
            let tail_min = if self.tail.is_some() { 12 } else { 0 };
            // Drop Lines, Blanks, Comments, then Files.
            for drop in ["Lines", "Blanks", "Comments", "Files"] {
                if total(&shown, &widths) + tail_min <= width {
                    break;
                }
                shown.retain(|&i| self.head[i] != drop);
            }
            let over = (total(&shown, &widths) + tail_min).saturating_sub(width);
            widths[0] = widths[0].saturating_sub(over).max(6);
        }
        let tail_width = width.map(|w| w.saturating_sub(total(&shown, &widths) + 2).max(12));

        let line = |cells: &[String], style: Style, out: &mut String| {
            let mut text = String::new();
            for (n, &i) in shown.iter().enumerate() {
                if n > 0 {
                    text.push_str("  ");
                }
                let cell = fit(&cells[i], widths[i], false);
                if i < lead {
                    let _ = write!(text, "{cell:<w$}", w = widths[i]);
                } else {
                    let _ = write!(text, "{cell:>w$}", w = widths[i]);
                }
            }
            if let (Some(_), Some(path)) = (self.tail, cells.get(self.head.len())) {
                text.push_str("  ");
                text.push_str(&fit(path, tail_width.unwrap_or(usize::MAX), true));
            }
            let _ = writeln!(out, "{style}{}{style:#}", text.trim_end());
        };
        let mut head: Vec<String> = self.head.iter().map(|h| h.to_string()).collect();
        head.extend(self.tail.map(String::from));
        line(&head, DIM, out);
        for (row, style) in &self.rows {
            line(row, *style, out);
        }
    }
}

fn is_numeric(head: &str) -> bool {
    matches!(head, "Files" | "Code" | "Comments" | "Blanks" | "Lines")
}

/// Truncates to `width` characters with an ellipsis, keeping the end for paths.
fn fit(s: &str, width: usize, keep_end: bool) -> Cow<'_, str> {
    let len = s.chars().count();
    if len <= width {
        return Cow::Borrowed(s);
    }
    let keep = width.saturating_sub(1);
    Cow::Owned(if keep_end {
        format!("…{}", s.chars().skip(len - keep).collect::<String>())
    } else {
        format!("{}…", s.chars().take(keep).collect::<String>())
    })
}

fn numbers(t: &Totals) -> Vec<String> {
    [t.files, t.code, t.comments, t.blanks, t.lines]
        .iter()
        .map(u64::to_string)
        .collect()
}

pub fn text(source: &Source, summary: &Summary, width: Option<usize>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", heading(source));

    if !summary.files.is_empty() {
        out.push('\n');
        let mut parts: Vec<_> = summary.files.iter().collect();
        parts.sort_by(|a, b| a.path.cmp(b.path).then(a.inline.cmp(&b.inline)));
        Table {
            head: vec![
                "Category", "Language", "Code", "Comments", "Blanks", "Lines",
            ],
            rows: parts
                .iter()
                .map(|p| {
                    let path = sanitize(p.path);
                    let path = if p.inline {
                        format!("{path} (inline tests)")
                    } else {
                        path.into()
                    };
                    let l = p.lines;
                    let row = [p.category.name(), p.language]
                        .into_iter()
                        .map(String::from)
                        .chain([l.code, l.comments, l.blanks, l.total()].map(|n| n.to_string()))
                        .chain([path])
                        .collect();
                    (row, Style::new())
                })
                .collect(),
            tail: Some("Path"),
        }
        .render(&mut out, width);
    }

    if summary.total.files == 0 && summary.total.lines == 0 {
        let _ = writeln!(out, "\n{DIM}no files with a recognized language{DIM:#}");
        return out;
    }

    out.push('\n');
    let mut rows: Vec<_> = Category::ALL
        .iter()
        .map(|c| (c, &summary.categories[c].totals))
        .filter(|(_, t)| t.files > 0 || t.lines > 0)
        .map(|(c, t)| {
            let mut row = vec![c.name().to_string()];
            row.extend(numbers(t));
            (row, Style::new())
        })
        .collect();
    let mut total = vec!["total".to_string()];
    total.extend(numbers(&summary.total));
    rows.push((total, BOLD));
    Table {
        head: vec!["Category", "Files", "Code", "Comments", "Blanks", "Lines"],
        rows,
        tail: None,
    }
    .render(&mut out, width);

    out.push('\n');
    let mut languages: Vec<_> = summary.languages.iter().collect();
    languages.sort_by(|a, b| {
        b.1.code
            .cmp(&a.1.code)
            .then(b.1.lines.cmp(&a.1.lines))
            .then(a.0.cmp(b.0))
    });
    Table {
        head: vec!["Language", "Files", "Code", "Comments", "Blanks", "Lines"],
        rows: languages
            .into_iter()
            .map(|(name, t)| {
                let mut row = vec![name.to_string()];
                row.extend(numbers(t));
                (row, Style::new())
            })
            .collect(),
        tail: None,
    }
    .render(&mut out, width);

    let notes = notes(summary);
    if !notes.is_empty() {
        out.push('\n');
        for note in notes {
            let _ = writeln!(out, "{DIM}{note}{DIM:#}");
        }
    }
    out
}

fn heading(source: &Source) -> String {
    match source {
        Source::Github {
            repository,
            reference,
            commit,
            ..
        } => {
            let short = &commit[..12.min(commit.len())];
            let reference = sanitize(reference);
            if commit.starts_with(&*reference.to_ascii_lowercase()) && reference.len() >= 7 {
                format!("{BOLD}{repository}{BOLD:#} {DIM}@{DIM:#} {short}")
            } else {
                format!("{BOLD}{repository}{BOLD:#} {DIM}@{DIM:#} {reference} {DIM}{short}{DIM:#}")
            }
        }
        Source::Directory { path } => format!("{BOLD}{}{BOLD:#}", sanitize(path)),
        Source::Archive { path, commit } => match commit {
            Some(commit) => format!(
                "{BOLD}{}{BOLD:#} {DIM}{}{DIM:#}",
                sanitize(path),
                sanitize(&commit[..12.min(commit.len())])
            ),
            None => format!("{BOLD}{}{BOLD:#}", sanitize(path)),
        },
    }
}

fn notes(summary: &Summary) -> Vec<String> {
    let mut notes = Vec::new();
    let inline = &summary.inline_tests;
    if inline.lines > 0 {
        notes.push(format!(
            "tests include {} lines of inline Rust tests from {} production file{}",
            inline.lines,
            inline.files,
            plural(inline.files)
        ));
    }
    let unsplit = summary.unsplit_inline_test_languages();
    if !unsplit.is_empty() {
        notes.push(format!(
            "inline tests in {} are not detected and count as production",
            unsplit.join(" and ")
        ));
    }
    let skipped = &summary.skipped.counted;
    for (n, what) in [
        (skipped.too_large, "over 64 MiB"),
        (skipped.unsafe_paths, "with unsafe archive paths"),
        (skipped.unreadable, "that could not be read"),
    ] {
        if n > 0 {
            notes.push(format!("skipped {n} file{} {what}", plural(n)));
        }
    }
    notes
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}
