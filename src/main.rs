mod cache;
mod classify;
mod cli;
mod count;
mod github;
mod report;
mod rust_tests;
mod summary;
mod target;

use std::{
    fs::File,
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
    time::{Duration, Instant},
};

use anstyle::{AnsiColor, Style};
use clap::{CommandFactory, Parser};

use crate::{
    cache::Cache,
    classify::Classifier,
    cli::{Cli, ColorWhen},
    count::{Counted, LIMITS},
    github::{Client, Repo, describe_io, is_full_sha},
    report::{Source, sanitize},
    summary::Summary,
    target::Target,
};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.color {
        ColorWhen::Always => anstream::ColorChoice::Always.write_global(),
        ColorWhen::Never => anstream::ColorChoice::Never.write_global(),
        ColorWhen::Auto => {}
    }
    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "ghf", &mut io::stdout());
        return ExitCode::SUCCESS;
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            let red = Style::new().bold().fg_color(Some(AnsiColor::Red.into()));
            let message = match e {
                Error::Io(e) => e.to_string(),
                Error::Message(m) => m,
            };
            let _ = writeln!(
                anstream::stderr(),
                "{red}error:{red:#} {}",
                sanitize(&message)
            );
            ExitCode::FAILURE
        }
    }
}

enum Error {
    Io(io::Error),
    Message(String),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<String> for Error {
    fn from(m: String) -> Self {
        Error::Message(m)
    }
}

/// `-v` diagnostics on stderr.
struct Log {
    enabled: bool,
    start: Instant,
}

impl Log {
    fn note(&self, what: &str, detail: impl std::fmt::Display) {
        if self.enabled {
            let ms = self.start.elapsed().as_secs_f64() * 1000.0;
            let dim = Style::new().dimmed();
            let _ = writeln!(
                anstream::stderr(),
                "{dim}{ms:>8.1} ms{dim:#}  {what:<9} {detail}"
            );
        }
    }
}

fn run(cli: &Cli) -> Result<(), Error> {
    let log = Log {
        enabled: cli.verbose,
        start: Instant::now(),
    };
    // Validate patterns before doing any work.
    Classifier::new(&cli.class, &cli.exclude, None)?;
    let target = target::parse(cli.target.as_deref(), cli.r#ref.as_deref())?;

    let (source, counted) = match target {
        Target::Dir(path) => {
            let counted = count::count_dir(&path, &LIMITS);
            let path = path.display().to_string();
            (Source::Directory { path }, counted)
        }
        Target::Archive(path) => {
            let file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let archive = count::count_archive(file, &LIMITS)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let path = path.display().to_string();
            let commit = archive.commit;
            (Source::Archive { path, commit }, archive.counted)
        }
        Target::GitHub { repo, reference } => remote(cli, &log, repo, reference)?,
    };
    log.note("counted", format_args!("{} files", counted.files.len()));

    let classifier = Classifier::new(&cli.class, &cli.exclude, counted.gitattributes.as_deref())?;
    let summary = Summary::new(&counted, &classifier, &cli.category, cli.files);
    log.note("classify", format_args!("{} files", summary.total.files));

    let mut out = anstream::stdout().lock();
    if cli.json {
        report::json(&mut out, &source, &summary)?;
    } else {
        let width = io::stdout()
            .is_terminal()
            .then(|| terminal_size::terminal_size().map(|(w, _)| w.0 as usize))
            .flatten();
        out.write_all(report::text(&source, &summary, width).as_bytes())?;
    }
    out.flush()?;
    log.note("done", "");
    Ok(())
}

fn remote(
    cli: &Cli,
    log: &Log,
    repo: Repo,
    reference: Option<String>,
) -> Result<(Source, Counted), Error> {
    let cache = if cli.no_cache { None } else { Cache::open() };
    let source = |repo: &Repo, reference: &str, default_branch, commit: &str| Source::Github {
        repository: repo.to_string(),
        reference: reference.to_string(),
        default_branch,
        commit: commit.to_string(),
    };

    // A full commit ID is immutable: a cached result needs no network at all.
    if let (Some(cache), Some(sha)) = (&cache, reference.as_deref().filter(|r| is_full_sha(r))) {
        let sha = sha.to_ascii_lowercase();
        if let Some(counted) = cache.load(&repo, &sha) {
            log.note("cache", "hit");
            return Ok((source(&repo, &sha, false, &sha), counted));
        }
    }

    let client = Client::new();
    log.note("auth", client.auth_source());
    let snapshot = client.resolve(&repo, reference.as_deref())?;
    log.note(
        "resolved",
        format_args!(
            "{}@{} -> {}",
            snapshot.repo,
            sanitize(&snapshot.label),
            snapshot.commit
        ),
    );
    let source = source(
        &snapshot.repo,
        &snapshot.label,
        snapshot.default_branch,
        &snapshot.commit,
    );
    if let Some(counted) = cache
        .as_ref()
        .and_then(|c| c.load(&snapshot.repo, &snapshot.commit))
    {
        log.note("cache", "hit");
        return Ok((source, counted));
    }

    let stream = client.download(&snapshot.repo, &snapshot.commit)?;
    let mut progress = Progress::new(stream);
    let archive = count::count_archive(&mut progress, &LIMITS);
    progress.clear();
    let archive =
        archive.map_err(|e| format!("downloading {}: {}", snapshot.repo, describe_io(&e)))?;
    log.note(
        "fetched",
        format_args!(
            "{:.1} MiB (counted while streaming)",
            progress.bytes as f64 / 1048576.0
        ),
    );
    if archive
        .commit
        .as_deref()
        .is_some_and(|c| c != snapshot.commit)
    {
        return Err(format!(
            "{}: archive does not match commit {}",
            snapshot.repo, snapshot.commit
        )
        .into());
    }

    if let Some(cache) = &cache {
        match cache.store(&snapshot.repo, &snapshot.commit, &archive.counted) {
            Ok(()) => log.note("cache", "stored"),
            Err(e) => log.note("cache", format_args!("not stored: {e}")),
        }
    }
    let counted = archive.counted;
    Ok((source, counted))
}

/// Counts downloaded bytes and, on a terminal, shows a transient progress line
/// once a download takes longer than a second.
struct Progress<R> {
    inner: R,
    bytes: u64,
    started: Instant,
    shown: Option<Instant>,
    tty: bool,
}

impl<R> Progress<R> {
    fn new(inner: R) -> Self {
        Progress {
            inner,
            bytes: 0,
            started: Instant::now(),
            shown: None,
            tty: io::stderr().is_terminal(),
        }
    }

    fn clear(&mut self) {
        if self.shown.is_some() {
            let _ = write!(io::stderr(), "\r\x1b[2K");
        }
    }
}

impl<R: Read> Read for Progress<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        let due = self
            .shown
            .map_or(self.started.elapsed() > Duration::from_secs(1), |t| {
                t.elapsed() > Duration::from_millis(200)
            });
        if self.tty && due {
            let mib = self.bytes as f64 / 1048576.0;
            let _ = write!(io::stderr(), "\r\x1b[2Kdownloading {mib:.1} MiB");
            self.shown = Some(Instant::now());
        }
        Ok(n)
    }
}
