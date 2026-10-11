//! Aggregating classified files into reconciled totals.

use std::collections::BTreeMap;

use serde::Serialize;
use tokei::LanguageType;

use crate::{
    classify::{Category, Classifier},
    count::{Counted, FileRecord, Lines, Skipped},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub files: u64,
    pub code: u64,
    pub comments: u64,
    pub blanks: u64,
    pub lines: u64,
}

impl Totals {
    fn add(&mut self, files: u64, lines: Lines) {
        self.files += files;
        self.code += lines.code;
        self.comments += lines.comments;
        self.blanks += lines.blanks;
        self.lines += lines.total();
    }
}

#[derive(Debug, Default, Serialize)]
pub struct CategoryTotals {
    #[serde(flatten)]
    pub totals: Totals,
    pub languages: BTreeMap<&'static str, Totals>,
}

/// One counted part of a file: a whole file, or its production or inline-test part.
#[derive(Debug, Serialize)]
pub struct Part<'a> {
    pub path: &'a str,
    pub language: &'static str,
    pub category: Category,
    /// Lines from inline Rust test items, split off their file.
    pub inline: bool,
    #[serde(flatten)]
    pub lines: Lines,
}

#[derive(Debug, Default, Serialize)]
pub struct Skips {
    #[serde(flatten)]
    pub counted: Skipped,
    /// Files removed by --exclude or --category.
    pub excluded: u64,
}

#[derive(Debug, Serialize)]
pub struct Summary<'a> {
    pub total: Totals,
    pub categories: BTreeMap<Category, CategoryTotals>,
    pub languages: BTreeMap<&'static str, Totals>,
    /// Lines of inline Rust tests moved from production files to tests.
    pub inline_tests: Totals,
    pub skipped: Skips,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<Part<'a>>,
}

impl<'a> Summary<'a> {
    /// Every file is counted once, in its category. Inline Rust test lines move
    /// from production to tests without adding a file, so all totals reconcile.
    pub fn new(
        counted: &'a Counted,
        classifier: &Classifier,
        selected: &[Category],
        list_files: bool,
    ) -> Self {
        let mut summary = Summary {
            total: Totals::default(),
            categories: Category::ALL
                .iter()
                .map(|&c| (c, CategoryTotals::default()))
                .collect(),
            languages: BTreeMap::new(),
            inline_tests: Totals::default(),
            skipped: Skips {
                counted: counted.skipped,
                excluded: 0,
            },
            files: Vec::new(),
        };
        let wanted = |c: Category| selected.is_empty() || selected.contains(&c);

        for file in &counted.files {
            if classifier.is_excluded(&file.path) {
                summary.skipped.excluded += 1;
                continue;
            }
            let category = classifier.classify(&file.path, file.language, file.generated);
            let inline = file
                .inline_tests
                .filter(|_| category == Category::Production);
            let main = inline.map_or(file.lines, |t| file.lines.minus(t));
            if !wanted(category) && !(inline.is_some() && wanted(Category::Tests)) {
                summary.skipped.excluded += 1;
            }
            if wanted(category) {
                summary.add(file, category, 1, main, false, list_files);
            }
            if let Some(tests) = inline.filter(|_| wanted(Category::Tests)) {
                summary.add(file, Category::Tests, 0, tests, true, list_files);
                summary.inline_tests.add(1, tests);
            }
        }
        summary
    }

    fn add(
        &mut self,
        file: &'a FileRecord,
        category: Category,
        files: u64,
        lines: Lines,
        inline: bool,
        list: bool,
    ) {
        let language = file.language.name();
        self.total.add(files, lines);
        self.languages
            .entry(language)
            .or_default()
            .add(files, lines);
        let category_totals = self.categories.entry(category).or_default();
        category_totals.totals.add(files, lines);
        category_totals
            .languages
            .entry(language)
            .or_default()
            .add(files, lines);
        if list {
            self.files.push(Part {
                path: &file.path,
                language,
                category,
                inline,
                lines,
            });
        }
    }

    /// Languages with any inline tests that ghf does not split out.
    pub fn unsplit_inline_test_languages(&self) -> Vec<&'static str> {
        const LANGUAGES: [LanguageType; 2] = [LanguageType::Zig, LanguageType::D];
        LANGUAGES
            .iter()
            .map(|l| l.name())
            .filter(|name| {
                self.categories[&Category::Production]
                    .languages
                    .get(name)
                    .is_some_and(|t| t.files > 0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::count::count;

    #[test]
    fn totals_reconcile() {
        let rust = "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    // check\n    #[test]\n    fn t() {}\n}\n";
        let counted = Counted {
            files: vec![
                count("src/main.rs".into(), LanguageType::Rust, rust.as_bytes()),
                count(
                    "tests/it.rs".into(),
                    LanguageType::Rust,
                    b"#[test]\nfn it() {}\n",
                ),
                count(
                    "README.md".into(),
                    LanguageType::Markdown,
                    b"# x\n\n```rust\nfn x() {}\n```\n",
                ),
                count("vendor/a.c".into(), LanguageType::C, b"int a;\n"),
                count("build/out.c".into(), LanguageType::C, b"int b;\n"),
            ],
            ..Counted::default()
        };
        let main = &counted.files[0];
        assert_eq!(
            main.lines,
            Lines {
                code: 6,
                comments: 1,
                blanks: 1
            }
        );
        assert_eq!(
            main.inline_tests,
            Some(Lines {
                code: 5,
                comments: 1,
                blanks: 0
            })
        );

        let classifier = Classifier::new(&[], &["build".into()], None).unwrap();
        let all = Summary::new(&counted, &classifier, &[], true);
        let sum = |m: &mut dyn Iterator<Item = Totals>| {
            m.fold(Totals::default(), |mut a, t| {
                a.files += t.files;
                a.add(
                    0,
                    Lines {
                        code: t.code,
                        comments: t.comments,
                        blanks: t.blanks,
                    },
                );
                a
            })
        };
        assert_eq!(
            sum(&mut all.categories.values().map(|c| c.totals)),
            all.total
        );
        assert_eq!(sum(&mut all.languages.values().copied()), all.total);
        let parts: u64 = all.files.iter().map(|p| p.lines.total()).sum();
        assert_eq!(parts, all.total.lines);
        let counted_lines: u64 = counted.files[..4].iter().map(|f| f.lines.total()).sum();
        assert_eq!(all.total.lines, counted_lines);
        assert_eq!(all.total.files, 4);
        assert_eq!(all.skipped.excluded, 1);
        assert_eq!(all.categories[&Category::Production].totals.code, 1);
        assert_eq!(all.categories[&Category::Tests].totals.files, 1);
        assert_eq!(all.categories[&Category::Tests].totals.code, 5 + 2);

        let prod = Summary::new(&counted, &classifier, &[Category::Production], false);
        assert_eq!(prod.total.lines, 2);
        assert_eq!(prod.skipped.excluded, 4);
    }
}
