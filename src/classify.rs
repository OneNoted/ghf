//! Path-based classification of counted files into disjoint categories.

use std::{fmt, str::FromStr};

use clap::ValueEnum;
use globset::{GlobBuilder, GlobMatcher};
use serde::Serialize;
use tokei::LanguageType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Production,
    Tests,
    Docs,
    Examples,
    Fixtures,
    Vendor,
    Other,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Production,
        Category::Tests,
        Category::Docs,
        Category::Examples,
        Category::Fixtures,
        Category::Vendor,
        Category::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Category::Production => "production",
            Category::Tests => "tests",
            Category::Docs => "docs",
            Category::Examples => "examples",
            Category::Fixtures => "fixtures",
            Category::Vendor => "vendor",
            Category::Other => "other",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A `--class CATEGORY=GLOB` rule.
#[derive(Clone, Debug)]
pub struct ClassRule {
    pub category: Category,
    pub glob: String,
}

impl FromStr for ClassRule {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (category, glob) = s
            .split_once('=')
            .ok_or("expected CATEGORY=GLOB, e.g. tests=integration/**")?;
        let category = Category::from_str(category, true).map_err(|_| {
            let names: Vec<_> = Category::ALL.iter().map(|c| c.name()).collect();
            format!("unknown category '{category}' (use {})", names.join(", "))
        })?;
        compile(glob, true)?;
        Ok(ClassRule {
            category,
            glob: glob.to_string(),
        })
    }
}

/// A gitignore-style glob: patterns without a slash match a name at any depth,
/// patterns with one are anchored at the root.
struct Pattern {
    glob: GlobMatcher,
    /// Also match when any parent directory matches, like .gitignore does.
    dirs: bool,
}

impl Pattern {
    fn is_match(&self, path: &str) -> bool {
        self.glob.is_match(path)
            || (self.dirs
                && path
                    .match_indices('/')
                    .any(|(i, _)| self.glob.is_match(&path[..i])))
    }
}

fn compile(pattern: &str, dirs: bool) -> Result<Pattern, String> {
    let trimmed = pattern.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(format!("empty pattern '{pattern}'"));
    }
    let glob = match trimmed.strip_prefix('/') {
        Some(anchored) => anchored.to_string(),
        None if trimmed.contains('/') => trimmed.to_string(),
        None => format!("**/{trimmed}"),
    };
    let glob = GlobBuilder::new(&glob)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("invalid pattern '{pattern}': {}", e.kind()))?;
    Ok(Pattern {
        glob: glob.compile_matcher(),
        dirs,
    })
}

#[derive(Clone, Copy)]
enum Attr {
    Generated,
    Vendored,
    Documentation,
}

/// Linguist overrides from the repository's root `.gitattributes`.
#[derive(Default)]
struct Attrs {
    generated: Option<bool>,
    vendored: Option<bool>,
    documentation: Option<bool>,
}

impl Attrs {
    fn slot(&mut self, attr: Attr) -> &mut Option<bool> {
        match attr {
            Attr::Generated => &mut self.generated,
            Attr::Vendored => &mut self.vendored,
            Attr::Documentation => &mut self.documentation,
        }
    }
}

type AttrRule = (Pattern, Vec<(Attr, Option<bool>)>);

fn parse_gitattributes(text: &str) -> Vec<AttrRule> {
    let mut rules = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(pattern) = words.next() else {
            continue;
        };
        if pattern.starts_with('#') || pattern.starts_with("[attr]") || pattern.starts_with('!') {
            continue;
        }
        let changes: Vec<_> = words.filter_map(parse_attr).collect();
        if changes.is_empty() {
            continue;
        }
        // Like Git, a pattern only matches files, never whole directories.
        if let Ok(pattern) = compile(pattern, false) {
            rules.push((pattern, changes));
        }
    }
    rules
}

fn parse_attr(word: &str) -> Option<(Attr, Option<bool>)> {
    let (name, value) = if let Some(name) = word.strip_prefix('-') {
        (name, Some(false))
    } else if let Some(name) = word.strip_prefix('!') {
        (name, None)
    } else if let Some((name, value)) = word.split_once('=') {
        (name, Some(!matches!(value, "false" | "0")))
    } else {
        (word, Some(true))
    };
    let attr = match name {
        "linguist-generated" => Attr::Generated,
        "linguist-vendored" => Attr::Vendored,
        "linguist-documentation" => Attr::Documentation,
        _ => return None,
    };
    Some((attr, value))
}

pub struct Classifier {
    rules: Vec<(Category, Pattern)>,
    excludes: Vec<Pattern>,
    attributes: Vec<AttrRule>,
}

impl Classifier {
    pub fn new(
        rules: &[ClassRule],
        excludes: &[String],
        gitattributes: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Classifier {
            rules: rules
                .iter()
                .map(|r| Ok((r.category, compile(&r.glob, true)?)))
                .collect::<Result<_, String>>()?,
            excludes: excludes
                .iter()
                .map(|p| compile(p, true))
                .collect::<Result<_, _>>()?,
            attributes: gitattributes.map(parse_gitattributes).unwrap_or_default(),
        })
    }

    pub fn is_excluded(&self, path: &str) -> bool {
        self.excludes.iter().any(|p| p.is_match(path))
    }

    pub fn classify(&self, path: &str, language: LanguageType, generated: bool) -> Category {
        if let Some((category, _)) = self.rules.iter().find(|(_, p)| p.is_match(path)) {
            return *category;
        }

        let mut attrs = Attrs::default();
        for (pattern, changes) in &self.attributes {
            if pattern.is_match(path) {
                for &(attr, value) in changes {
                    *attrs.slot(attr) = value;
                }
            }
        }
        if attrs.generated == Some(true) || attrs.vendored == Some(true) {
            return Category::Vendor;
        }
        if attrs.documentation == Some(true) {
            return Category::Docs;
        }

        let (dirs, file) = match path.rsplit_once('/') {
            Some((dirs, file)) => (dirs, file),
            None => ("", path),
        };
        let in_dir = |names: &[&str]| {
            dirs.split('/')
                .any(|d| names.iter().any(|n| d.eq_ignore_ascii_case(n)))
        };
        let docs_allowed = attrs.documentation != Some(false);

        if attrs.vendored != Some(false) && in_dir(VENDOR_DIRS) {
            return Category::Vendor;
        }
        if attrs.generated != Some(false) && (generated || is_generated_name(file)) {
            return Category::Vendor;
        }
        if in_dir(FIXTURE_DIRS) {
            return Category::Fixtures;
        }
        if dirs.split('/').any(|d| d.starts_with('.')) {
            return Category::Other;
        }
        if docs_allowed && (DOC_LANGUAGES.contains(&language) || is_doc_name(file)) {
            return Category::Docs;
        }
        if in_dir(TEST_DIRS) || is_test_name(file) {
            return Category::Tests;
        }
        if in_dir(EXAMPLE_DIRS) {
            return Category::Examples;
        }
        // A nested `doc/` is usually a code module (Go's go/doc, Rust `mod doc`).
        let top_doc = path
            .split_once('/')
            .is_some_and(|(top, _)| top.eq_ignore_ascii_case("doc"));
        if docs_allowed && (top_doc || in_dir(DOC_DIRS)) {
            return Category::Docs;
        }
        if file.starts_with('.') || OTHER_LANGUAGES.contains(&language) {
            return Category::Other;
        }
        Category::Production
    }
}

const VENDOR_DIRS: &[&str] = &[
    "vendor",
    "vendored",
    "third_party",
    "third-party",
    "thirdparty",
    "3rdparty",
    "3rd_party",
    "3rd-party",
    "external",
    "extern",
    "deps",
    "node_modules",
    "bower_components",
    "Pods",
    "Godeps",
    "dist",
    "generated",
    "__generated__",
];
const FIXTURE_DIRS: &[&str] = &[
    "fixtures",
    "fixture",
    "__fixtures__",
    "test-fixtures",
    "test_fixtures",
    "testdata",
    "test-data",
    "test_data",
    "__snapshots__",
    "golden",
    "goldens",
];
const TEST_DIRS: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "__test__",
    "spec",
    "specs",
    "e2e",
    "t",
    "testutil",
    "testutils",
    "test-utils",
    "test_utils",
    "__mocks__",
    "bench",
    "benches",
    "benchmark",
    "benchmarks",
];
const EXAMPLE_DIRS: &[&str] = &[
    "example",
    "examples",
    "_examples",
    "demo",
    "demos",
    "sample",
    "samples",
];
const DOC_DIRS: &[&str] = &["docs", "documentation"];
const DOC_NAMES: &[&str] = &[
    "README",
    "CHANGELOG",
    "CHANGES",
    "HISTORY",
    "NEWS",
    "CONTRIBUTING",
    "LICENSE",
    "LICENCE",
    "COPYING",
    "AUTHORS",
    "CONTRIBUTORS",
    "CODE_OF_CONDUCT",
    "SECURITY",
    "NOTICE",
    "MAINTAINERS",
    "GOVERNANCE",
    "SUPPORT",
];
const DOC_LANGUAGES: &[LanguageType] = &[
    LanguageType::Markdown,
    LanguageType::ReStructuredText,
    LanguageType::AsciiDoc,
    LanguageType::Org,
];
const OTHER_LANGUAGES: &[LanguageType] = &[
    LanguageType::Json,
    LanguageType::Yaml,
    LanguageType::Toml,
    LanguageType::Ini,
    LanguageType::Xml,
    LanguageType::Svg,
    LanguageType::Text,
];
const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "deno.lock",
    "composer.lock",
    "Gemfile.lock",
    "Pipfile.lock",
    "poetry.lock",
    "uv.lock",
    "go.sum",
    "flake.lock",
    "mix.lock",
    "Package.resolved",
];
const GENERATED_SUFFIXES: &[&str] = &[
    ".min.js",
    ".min.mjs",
    ".min.css",
    ".pb.go",
    ".pb.cc",
    ".pb.h",
    "_pb2.py",
    "_pb2_grpc.py",
    ".g.dart",
    ".freezed.dart",
    ".designer.cs",
];

fn is_generated_name(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    LOCKFILES.iter().any(|f| file.eq_ignore_ascii_case(f))
        || GENERATED_SUFFIXES.iter().any(|s| lower.ends_with(s))
        || stem(&lower).ends_with(".generated")
}

fn is_doc_name(file: &str) -> bool {
    let first = file.split('.').next().unwrap_or(file);
    DOC_NAMES.iter().any(|n| first.eq_ignore_ascii_case(n))
}

fn is_test_name(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    let stem_lower = stem(&lower);
    if matches!(stem_lower, "test" | "tests" | "conftest") {
        return true;
    }
    if ["test_", "tests_"]
        .iter()
        .any(|p| stem_lower.starts_with(p))
        || [
            "_test",
            "_tests",
            "_spec",
            "_unittest",
            "-test",
            "-tests",
            "-spec",
            ".test",
            ".spec",
        ]
        .iter()
        .any(|s| stem_lower.ends_with(s))
    {
        return true;
    }
    // FooTest.java, FooTests.cs, FooSpec.scala
    let stem = stem(file);
    ["Test", "Tests", "Spec"].iter().any(|suffix| {
        stem.strip_suffix(suffix)
            .and_then(|rest| rest.chars().last())
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// The file name without its last extension.
fn stem(file: &str) -> &str {
    match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => file,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use LanguageType as L;

    fn classify(path: &str, language: LanguageType) -> Category {
        Classifier::new(&[], &[], None)
            .unwrap()
            .classify(path, language, false)
    }

    #[test]
    fn default_rules() {
        for (path, language, expected) in [
            ("src/main.rs", L::Rust, Category::Production),
            ("src/tests.rs", L::Rust, Category::Tests),
            ("pkg/server_test.go", L::Go, Category::Tests),
            ("tests/fixtures/input.rs", L::Rust, Category::Fixtures),
            ("web/app.test.tsx", L::Tsx, Category::Tests),
            ("src/main/java/FooTest.java", L::Java, Category::Tests),
            ("src/Contest.java", L::Java, Category::Production),
            ("benches/parse.rs", L::Rust, Category::Tests),
            ("examples/demo.rs", L::Rust, Category::Examples),
            ("examples/README.md", L::Markdown, Category::Docs),
            ("docs/conf.py", L::Python, Category::Docs),
            ("doc/gen.py", L::Python, Category::Docs),
            ("src/go/doc/reader.go", L::Go, Category::Production),
            ("third_party/zlib/inflate.c", L::C, Category::Vendor),
            ("dist/index.js", L::JavaScript, Category::Vendor),
            ("static/app.min.js", L::JavaScript, Category::Vendor),
            ("package-lock.json", L::Json, Category::Vendor),
            (".github/workflows/test.yml", L::Yaml, Category::Other),
            ("Cargo.toml", L::Toml, Category::Other),
            (".eslintrc.js", L::JavaScript, Category::Other),
            ("README.txt", L::Text, Category::Docs),
        ] {
            assert_eq!(classify(path, language), expected, "{path}");
        }
        let c = Classifier::new(&[], &[], None).unwrap();
        assert_eq!(c.classify("src/api.rs", L::Rust, true), Category::Vendor);
    }

    #[test]
    fn overrides() {
        let rules = ["tests=integration/", "production=src/vendor/**"]
            .map(|r| r.parse::<ClassRule>().unwrap());
        let attrs = "lib/gen/** linguist-generated\n\
                     vendor/ours/** -linguist-vendored\n\
                     *.md -linguist-documentation\n";
        let c = Classifier::new(&rules, &["*.snap".into(), "build".into()], Some(attrs)).unwrap();
        assert_eq!(
            c.classify("integration/a.py", L::Python, false),
            Category::Tests
        );
        assert_eq!(
            c.classify("src/vendor/x.rs", L::Rust, false),
            Category::Production
        );
        assert_eq!(
            c.classify("lib/gen/a.ts", L::TypeScript, false),
            Category::Vendor
        );
        assert_eq!(
            c.classify("vendor/ours/a.c", L::C, false),
            Category::Production
        );
        assert_eq!(
            c.classify("guide.md", L::Markdown, false),
            Category::Production
        );
        assert!(c.is_excluded("a/b.snap") && c.is_excluded("x/build/out.c"));
        assert!(!c.is_excluded("builder.c"));
        assert!("nope=x".parse::<ClassRule>().is_err());
    }
}
