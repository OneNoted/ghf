use clap::{Parser, ValueEnum};
use clap_complete::Shell;

use crate::classify::{Category, ClassRule};

const AFTER_HELP: &str = "\
TARGET is owner/repo, a GitHub URL (…/tree/REF and …/commit/SHA work), or a
local directory or .tar.gz archive. Prefix relative local paths with ./ when
they look like owner/repo.

Categories (every file gets exactly one; first matching rule wins):
  --class      your rules, in order
  attributes   root .gitattributes: linguist-generated / linguist-vendored
               -> vendor, linguist-documentation -> docs (=false disables)
  vendor       vendor/, third_party/, external/, deps/, node_modules/, dist/,
               generated/, lockfiles, *.min.js, *.pb.go, *_pb2.py, and files
               whose first lines say @generated, DO NOT EDIT, Code generated
  fixtures     fixtures/, testdata/, test_data/, __snapshots__/, golden/
  other        files inside hidden directories (.github/, .cargo/, ...)
  docs         Markdown, reStructuredText, AsciiDoc, Org; README, CHANGELOG,
               LICENSE, CONTRIBUTING and similar
  tests        test(s)/, spec(s)/, __tests__/, e2e/, t/, bench(es)/ and
               *_test.*, test_*.*, *.test.*, *.spec.*, FooTest.java, conftest.py
  examples     example(s)/, demo(s)/, sample(s)/
  docs         docs/, documentation/, and doc/ at the top level
  other        hidden files, JSON, YAML, TOML, INI, XML, SVG, plain text
  production   everything else

Rust items marked #[cfg(test)] or #[cfg(all(test, ..))] inside production files
count as tests; the file itself is still counted once, under production. Inline
tests in other languages are not detected and stay in their file's category.
Path rules are heuristics, not proof: audit with --files, correct with --class.

Remote repositories are downloaded as one tarball of the snapshot (no Git
history) and counted in memory without being unpacked. Branches and tags are
resolved to a commit on every run; results are cached by commit in
$XDG_CACHE_HOME/ghf (bounded to 256 MiB). Downloads are capped at 2 GiB
(16 GiB unpacked); files over 64 MiB are skipped and reported.

Private repositories use GH_TOKEN or GITHUB_TOKEN, else `gh auth token` when
the GitHub CLI is installed and logged in.";

/// Count code in GitHub repositories by language and category.
#[derive(Parser, Debug)]
#[command(name = "ghf", version, about, after_long_help = AFTER_HELP)]
pub struct Cli {
    /// owner/repo, GitHub URL, local directory or .tar.gz [default: .]
    pub target: Option<String>,

    /// Branch, tag or commit to count [default: the default branch]
    #[arg(short, long, value_name = "REF")]
    pub r#ref: Option<String>,

    /// Count only these categories
    #[arg(
        short,
        long,
        value_name = "CATEGORY",
        value_enum,
        value_delimiter = ','
    )]
    pub category: Vec<Category>,

    /// Put paths matching GLOB in CATEGORY (repeatable)
    #[arg(long = "class", value_name = "CATEGORY=GLOB")]
    pub class: Vec<ClassRule>,

    /// Skip paths matching GLOB (repeatable)
    #[arg(short = 'x', long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// List every counted file
    #[arg(long)]
    pub files: bool,

    /// Print JSON
    #[arg(long)]
    pub json: bool,

    /// When to use colors
    #[arg(long, value_name = "WHEN", value_enum, default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,

    /// Don't read or write the cache
    #[arg(long)]
    pub no_cache: bool,

    /// Print snapshot, cache and timing details to stderr
    #[arg(short, long)]
    pub verbose: bool,

    /// Print a completion script for SHELL
    #[arg(long, value_name = "SHELL", exclusive = true)]
    pub completions: Option<Shell>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ColorWhen {
    Auto,
    Always,
    Never,
}
