//! Terminal and JSON output.

use std::{
    borrow::Cow,
    fmt::Write as _,
    io::{self, Write},
};

use anstyle::{AnsiColor, Color, Style};
use serde::Serialize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    classify::Category,
    summary::{Part, Summary, Totals},
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

/// Text width for pipes and wide terminals. Rules run one column further, to
/// tokei's familiar 80 columns.
const REPORT_WIDTH: usize = 79;
/// Numeric cells (gap included) grow to this width when there is room.
const ROOMY_CELL: usize = 12;
const GAP: usize = 2;
/// Labels keep at least this many columns while numeric columns can still go.
const MIN_LABEL: usize = 12;
/// Paths keep at least this many columns in `--files` while other columns can still go.
const MIN_PATH: usize = 32;

const STRONG: char = '━';
const LIGHT: char = '─';

const HEADING: Style = Style::new().bold();
const COLUMNS: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Blue)));
const TOTAL: Style = Style::new().bold();
const COMMIT: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Files,
    Lines,
    Code,
    Comments,
    Blanks,
}

impl Metric {
    fn title(self) -> &'static str {
        match self {
            Metric::Files => "Files",
            Metric::Lines => "Lines",
            Metric::Code => "Code",
            Metric::Comments => "Comments",
            Metric::Blanks => "Blanks",
        }
    }

    fn of(self, totals: &Totals) -> u64 {
        match self {
            Metric::Files => totals.files,
            Metric::Lines => totals.lines,
            Metric::Code => totals.code,
            Metric::Comments => totals.comments,
            Metric::Blanks => totals.blanks,
        }
    }
}

/// Summary columns, in tokei's order.
const SUMMARY_METRICS: [Metric; 5] = [
    Metric::Files,
    Metric::Lines,
    Metric::Code,
    Metric::Comments,
    Metric::Blanks,
];
const FILE_METRICS: [Metric; 4] = [
    Metric::Lines,
    Metric::Code,
    Metric::Comments,
    Metric::Blanks,
];
/// Narrow terminals lose numeric columns in this order; Code always stays.
const DROP_ORDER: [Metric; 4] = [
    Metric::Blanks,
    Metric::Comments,
    Metric::Lines,
    Metric::Files,
];

fn category_title(category: Category) -> &'static str {
    match category {
        Category::Production => "Production",
        Category::Tests => "Tests",
        Category::Docs => "Docs",
        Category::Examples => "Examples",
        Category::Fixtures => "Fixtures",
        Category::Vendor => "Vendor",
        Category::Other => "Other",
    }
}

/// `1234567` as `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Terminal columns taken by `s` (wide CJK characters count twice).
fn columns(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Shortens `s` to at most `max` columns, keeping its start or its end.
fn truncate(s: &str, max: usize, keep_end: bool) -> Cow<'_, str> {
    if columns(s) <= max {
        return Cow::Borrowed(s);
    }
    let mut room = max.saturating_sub(1); // one column for the ellipsis
    let mut fits = |c: char| {
        let width = c.width().unwrap_or(0);
        (width <= room).then(|| {
            room -= width;
            c
        })
    };
    if keep_end {
        let mut tail: Vec<char> = s.chars().rev().map_while(&mut fits).collect();
        tail.reverse();
        Cow::Owned(std::iter::once('…').chain(tail).collect())
    } else {
        let head: String = s.chars().map_while(fits).collect();
        Cow::Owned(head + "…")
    }
}

/// Width of a column titled `title` whose values take these widths.
fn column_width(title: &str, values: impl Iterator<Item = usize>) -> usize {
    values.chain([columns(title)]).max().unwrap_or_default()
}

fn push_padded(line: &mut String, text: &str, width: usize) {
    line.push_str(text);
    line.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(columns(text)),
    ));
}

/// A rule under text `width` columns wide; it runs one column past the text,
/// mirroring the text's left margin.
fn rule(out: &mut String, glyph: char, width: usize) {
    let _ = writeln!(out, "{}", glyph.to_string().repeat(width + 1));
}

/// Column widths of the language and category tables, which share one grid so
/// their numbers line up.
struct Grid {
    label: usize,
    /// Shown numeric columns and their cell widths, gap included.
    cells: Vec<(Metric, usize)>,
}

impl Grid {
    /// Fits the grid into the terminal `width`, or into [`REPORT_WIDTH`] when
    /// output is not a terminal. Only terminals drop columns or truncate
    /// labels; piped output keeps everything and may grow wider instead.
    fn fit(label: usize, rows: &[&Totals], width: Option<usize>) -> Grid {
        let budget = width.map_or(REPORT_WIDTH, |w| w.min(REPORT_WIDTH));
        let value = |metric: Metric| {
            let values = rows.iter().map(|t| grouped(metric.of(t)).len());
            column_width(metric.title(), values)
        };
        let mut cells: Vec<(Metric, usize)> = SUMMARY_METRICS
            .iter()
            .map(|&m| (m, GAP + value(m)))
            .collect();
        let needed = |cells: &[(Metric, usize)], label: usize| {
            1 + label + cells.iter().map(|(_, w)| w).sum::<usize>()
        };
        if width.is_some() {
            for metric in DROP_ORDER {
                if needed(&cells, label.min(MIN_LABEL)) <= budget {
                    break;
                }
                cells.retain(|&(m, _)| m != metric);
            }
        }
        let used = needed(&cells, label);
        if used > budget {
            let label = match width {
                Some(_) => budget.saturating_sub(needed(&cells, 0)).max(3),
                None => label,
            };
            return Grid { label, cells };
        }
        let mut spare = budget - used;
        while spare > 0 && cells.iter().any(|&(_, w)| w < ROOMY_CELL) {
            for (_, w) in cells.iter_mut().filter(|(_, w)| *w < ROOMY_CELL) {
                if spare > 0 {
                    *w += 1;
                    spare -= 1;
                }
            }
        }
        Grid {
            label: label + spare,
            cells,
        }
    }

    fn width(&self) -> usize {
        1 + self.label + self.cells.iter().map(|(_, w)| w).sum::<usize>()
    }

    fn header(&self, out: &mut String, title: &str) {
        let mut line = String::from(" ");
        push_padded(&mut line, title, self.label);
        for &(metric, width) in &self.cells {
            let _ = write!(line, "{:>width$}", metric.title());
        }
        let _ = writeln!(out, "{COLUMNS}{line}{COLUMNS:#}");
    }

    fn row(&self, out: &mut String, label: &str, totals: &Totals, style: Style) {
        let mut line = String::from(" ");
        push_padded(&mut line, &truncate(label, self.label, false), self.label);
        for &(metric, width) in &self.cells {
            let _ = write!(line, "{:>width$}", grouped(metric.of(totals)));
        }
        let _ = writeln!(out, "{style}{line}{style:#}");
    }
}

/// Renders the human report. `width` is the terminal width, or `None` when
/// stdout is not a terminal.
pub fn text(source: &Source, summary: &Summary, width: Option<usize>) -> String {
    // Text keeps one column clear of the right edge, like its left margin.
    let width = width.map(|w| w.saturating_sub(1));
    let mut out = String::new();
    heading(&mut out, source, width);

    if !summary.files.is_empty() {
        files(&mut out, summary, width);
    }
    if summary.total.files == 0 && summary.total.lines == 0 {
        notes(
            &mut out,
            &["No files with a recognized language.".to_string()],
            width,
        );
        return out;
    }

    let mut languages: Vec<_> = summary.languages.iter().collect();
    languages.sort_by(|a, b| {
        b.1.code
            .cmp(&a.1.code)
            .then(b.1.lines.cmp(&a.1.lines))
            .then(a.0.cmp(b.0))
    });
    let categories: Vec<_> = Category::ALL
        .iter()
        .map(|&c| (category_title(c), &summary.categories[&c].totals))
        .filter(|(_, t)| t.files > 0 || t.lines > 0)
        .collect();

    let label = languages
        .iter()
        .map(|(name, _)| columns(name))
        .chain(categories.iter().map(|(title, _)| title.len()))
        .chain(["Language".len(), "Category".len()])
        .max()
        .unwrap_or_default();
    let rows: Vec<&Totals> = languages
        .iter()
        .map(|(_, t)| *t)
        .chain(categories.iter().map(|(_, t)| *t))
        .chain([&summary.total])
        .collect();
    let grid = Grid::fit(label, &rows, width);

    rule(&mut out, STRONG, grid.width());
    grid.header(&mut out, "Language");
    rule(&mut out, STRONG, grid.width());
    for (name, totals) in languages {
        grid.row(&mut out, name, totals, Style::new());
    }
    rule(&mut out, STRONG, grid.width());
    grid.header(&mut out, "Category");
    rule(&mut out, LIGHT, grid.width());
    for (title, totals) in categories {
        grid.row(&mut out, title, totals, Style::new());
    }
    rule(&mut out, STRONG, grid.width());
    grid.row(&mut out, "Total", &summary.total, TOTAL);
    rule(&mut out, STRONG, grid.width());

    notes(&mut out, &note_lines(summary), width);
    out
}

/// Column widths of the `--files` table.
struct FileGrid {
    category: usize,
    /// `None` when the terminal is too narrow for languages.
    language: Option<usize>,
    cells: Vec<(Metric, usize)>,
    path: usize,
}

impl FileGrid {
    /// Paths matter most, then languages (which may be shortened); numbers go
    /// first on narrow terminals. Piped output keeps every column whole.
    fn fit(rows: &[FileRow], width: Option<usize>) -> FileGrid {
        let category = column_width("Category", rows.iter().map(|r| r.category.len()));
        let language = column_width("Language", rows.iter().map(|r| columns(r.language)));
        let path = column_width("Path", rows.iter().map(|r| columns(&r.path)));
        let mut grid = FileGrid {
            category,
            language: Some(language),
            cells: FILE_METRICS
                .iter()
                .map(|&m| {
                    let values = rows.iter().map(|r| grouped(m.of(&r.totals)).len());
                    (m, GAP + column_width(m.title(), values))
                })
                .collect(),
            path,
        };
        if let Some(width) = width {
            let shortest_language = language.min(MIN_LABEL);
            grid.language = Some(shortest_language);
            grid.path = path.min(MIN_PATH);
            for metric in DROP_ORDER {
                if grid.width() <= width {
                    break;
                }
                grid.cells.retain(|&(m, _)| m != metric);
            }
            if grid.width() > width {
                grid.language = None;
            }
            grid.path = 0;
            let spare = width.saturating_sub(grid.width());
            grid.path = spare.min(path).max(8);
            let leftover = spare.saturating_sub(grid.path);
            grid.language = grid
                .language
                .map(|shortest| language.min(shortest + leftover));
        }
        // Short paths still pad the table to the report width, so its rules
        // line up with the summary below.
        let floor = width.map_or(REPORT_WIDTH, |w| w.min(REPORT_WIDTH));
        grid.path += floor.saturating_sub(grid.width());
        grid
    }

    fn width(&self) -> usize {
        1 + self.category
            + self.language.map_or(0, |w| GAP + w)
            + self.cells.iter().map(|(_, w)| w).sum::<usize>()
            + GAP
            + self.path
    }

    fn line(&self, category: &str, language: &str, numbers: &[String], path: &str) -> String {
        let mut line = String::from(" ");
        push_padded(&mut line, category, self.category);
        if let Some(width) = self.language {
            line.push_str("  ");
            push_padded(&mut line, &truncate(language, width, false), width);
        }
        for ((_, width), number) in self.cells.iter().zip(numbers) {
            let _ = write!(line, "{number:>width$}");
        }
        line.push_str("  ");
        line.push_str(&truncate(path, self.path, true));
        line.truncate(line.trim_end().len());
        line
    }
}

/// One `--files` row: a whole file, or the inline Rust tests split off it.
struct FileRow<'a> {
    category: &'static str,
    language: &'a str,
    totals: Totals,
    path: Cow<'a, str>,
}

/// Lists every counted file part, sorted by path; inline Rust tests follow
/// their file under their own label.
fn files(out: &mut String, summary: &Summary, width: Option<usize>) {
    let mut parts: Vec<&Part> = summary.files.iter().collect();
    parts.sort_by(|a, b| a.path.cmp(b.path).then(a.inline.cmp(&b.inline)));
    let rows: Vec<FileRow> = parts
        .iter()
        .map(|part| FileRow {
            category: match part.inline {
                true => "Inline tests",
                false => category_title(part.category),
            },
            language: part.language,
            totals: Totals {
                files: 1,
                code: part.lines.code,
                comments: part.lines.comments,
                blanks: part.lines.blanks,
                lines: part.lines.total(),
            },
            path: sanitize(part.path),
        })
        .collect();
    let grid = FileGrid::fit(&rows, width);

    rule(out, STRONG, grid.width());
    let titles: Vec<String> = grid
        .cells
        .iter()
        .map(|(m, _)| m.title().to_string())
        .collect();
    let header = grid.line("Category", "Language", &titles, "Path");
    let _ = writeln!(out, "{COLUMNS}{header}{COLUMNS:#}");
    rule(out, STRONG, grid.width());
    for row in &rows {
        let numbers: Vec<String> = grid
            .cells
            .iter()
            .map(|(m, _)| grouped(m.of(&row.totals)))
            .collect();
        let line = grid.line(row.category, row.language, &numbers, &row.path);
        let _ = writeln!(out, "{line}");
    }
}

/// `owner/repo @ ref  commit`, or the local path, shortened to fit a terminal.
fn heading(out: &mut String, source: &Source, width: Option<usize>) {
    let budget = width.unwrap_or(usize::MAX);
    match source {
        Source::Github {
            repository,
            reference,
            commit,
            ..
        } => {
            let short = &commit[..12.min(commit.len())];
            let reference = sanitize(reference);
            let pinned =
                reference.len() >= 7 && commit.starts_with(&*reference.to_ascii_lowercase());
            let fixed = 1 + " @ ".len() + short.len();
            if pinned {
                let repository = truncate(repository, budget.saturating_sub(fixed).max(8), false);
                let _ = writeln!(
                    out,
                    " {HEADING}{repository}{HEADING:#} @ {COMMIT}{short}{COMMIT:#}"
                );
            } else {
                // The ref keeps a few columns; the repository shortens first.
                let fixed = fixed + "  ".len();
                let repository =
                    truncate(repository, budget.saturating_sub(fixed + 6).max(8), false);
                let room = budget.saturating_sub(fixed + columns(&repository)).max(4);
                let reference = truncate(&reference, room, false);
                let _ = writeln!(
                    out,
                    " {HEADING}{repository}{HEADING:#} @ {reference}  {COMMIT}{short}{COMMIT:#}"
                );
            }
        }
        Source::Directory { path } | Source::Archive { path, commit: None } => {
            let path = sanitize(path);
            let path = truncate(&path, budget.saturating_sub(1).max(8), true);
            let _ = writeln!(out, " {HEADING}{path}{HEADING:#}");
        }
        Source::Archive {
            path,
            commit: Some(commit),
        } => {
            let short = sanitize(&commit[..12.min(commit.len())]).into_owned();
            let path = sanitize(path);
            let room = budget.saturating_sub(3 + columns(&short)).max(8);
            let path = truncate(&path, room, true);
            let _ = writeln!(
                out,
                " {HEADING}{path}{HEADING:#}  {COMMIT}{short}{COMMIT:#}"
            );
        }
    }
}

/// Writes notes below the report, word-wrapped to the report width.
fn notes(out: &mut String, notes: &[String], width: Option<usize>) {
    let limit = width.map_or(REPORT_WIDTH, |w| w.min(REPORT_WIDTH));
    for note in notes {
        let mut line = String::new();
        for word in note.split(' ') {
            if !line.is_empty() && 1 + columns(&line) + 1 + columns(word) > limit {
                let _ = writeln!(out, " {line}");
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        let _ = writeln!(out, " {line}");
    }
}

fn note_lines(summary: &Summary) -> Vec<String> {
    let mut notes = Vec::new();
    let inline = &summary.inline_tests;
    if inline.lines > 0 {
        notes.push(format!(
            "Tests include {} lines of inline Rust tests from {} production file{}.",
            grouped(inline.lines),
            grouped(inline.files),
            plural(inline.files)
        ));
    }
    let unsplit = summary.unsplit_inline_test_languages();
    if !unsplit.is_empty() {
        notes.push(format!(
            "Inline tests in {} are not detected and count as production.",
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
            notes.push(format!("Skipped {} file{} {what}.", grouped(n), plural(n)));
        }
    }
    notes
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use tokei::LanguageType;

    use super::*;
    use crate::{
        classify::Classifier,
        count::{Counted, FileRecord, Lines},
    };

    fn file(
        path: &str,
        language: LanguageType,
        code: u64,
        inline_tests: Option<Lines>,
    ) -> FileRecord {
        FileRecord {
            path: path.into(),
            language,
            lines: Lines {
                code,
                comments: code / 3,
                blanks: code / 7,
            },
            inline_tests,
            generated: false,
        }
    }

    #[test]
    fn report_lines_fit_phone_and_desktop_terminals() {
        let inline = Lines {
            code: 2,
            comments: 0,
            blanks: 0,
        };
        let counted = Counted {
            files: vec![
                file(
                    "源代码/主要模块/处理器/数据结构/实现细节.rs",
                    LanguageType::Rust,
                    12,
                    Some(inline),
                ),
                file("Solution.sln", LanguageType::VisualStudioSolution, 4, None),
                file("generated/table.c", LanguageType::C, 31_234_567, None),
                file(
                    "src/a/deeply/nested/directory/with/a/long/file_name.rs",
                    LanguageType::Rust,
                    7,
                    None,
                ),
            ],
            ..Counted::default()
        };
        let classifier = Classifier::new(&[], &[], None).unwrap();
        let summary = Summary::new(&counted, &classifier, &[], true);
        let source = Source::Github {
            repository: "an-organisation-name/a-repository-with-a-long-name".into(),
            reference: "feature/".repeat(12),
            default_branch: false,
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
        };

        for width in [40, 60, 80] {
            let report =
                anstream::adapter::strip_str(&text(&source, &summary, Some(width))).to_string();
            for line in report.lines() {
                let shown = UnicodeWidthStr::width(line);
                assert!(shown <= width, "{shown} > {width} columns: {line:?}");
            }
        }
    }
}
