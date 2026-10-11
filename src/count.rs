//! Counting with tokei, from local directories or streamed tar archives.
//!
//! Archives are never unpacked: entries are read into memory one at a time and
//! counted, so link and traversal entries cannot touch the file system.

use std::{
    borrow::Cow,
    fs,
    io::{self, BufRead, Read, Write},
    path::Path,
    sync::{
        Condvar, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
        mpsc::sync_channel,
    },
    thread,
};

use encoding_rs_io::DecodeReaderBytesBuilder;
use ignore::{WalkBuilder, WalkState};
use memchr::memmem;
use rayon::iter::{ParallelBridge, ParallelIterator};
use serde::{Deserialize, Serialize};
use tar::EntryType;
use tokei::{CodeStats, Config, LanguageType};

use crate::rust_tests;

static CONFIG: LazyLock<Config> = LazyLock::new(Config::default);

/// Resource limits for reading untrusted archives.
pub struct Limits {
    pub max_unpacked: u64,
    pub max_file: u64,
    pub max_entries: u64,
}

pub const LIMITS: Limits = Limits {
    max_unpacked: 16 << 30,
    max_file: 64 << 20,
    max_entries: 2_000_000,
};

/// File contents waiting to be counted, at most.
const QUEUE_BYTES: usize = 256 << 20;
const QUEUE_LEN: usize = 4096;
const MAX_GITATTRIBUTES: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lines {
    pub code: u64,
    pub comments: u64,
    pub blanks: u64,
}

impl Lines {
    fn of(stats: &CodeStats) -> Self {
        let s = stats.summarise();
        Lines {
            code: s.code as u64,
            comments: s.comments as u64,
            blanks: s.blanks as u64,
        }
    }

    pub fn total(self) -> u64 {
        self.code + self.comments + self.blanks
    }

    /// Clamps `self` to `whole` field by field so `whole - self` never underflows.
    fn within(self, whole: Lines) -> Lines {
        Lines {
            code: self.code.min(whole.code),
            comments: self.comments.min(whole.comments),
            blanks: self.blanks.min(whole.blanks),
        }
    }

    pub fn minus(self, other: Lines) -> Lines {
        Lines {
            code: self.code - other.code,
            comments: self.comments - other.comments,
            blanks: self.blanks - other.blanks,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub language: LanguageType,
    pub lines: Lines,
    /// The part of `lines` inside Rust test-only items (`#[cfg(test)]`, `#[test]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_tests: Option<Lines>,
    /// The first lines carry a generated-code marker.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub generated: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Skipped {
    /// Files tokei has no language for.
    pub unrecognized: u64,
    pub too_large: u64,
    /// Symlinks and hard links (never followed).
    pub links: u64,
    /// Archive entries with absolute or `..` paths.
    pub unsafe_paths: u64,
    pub unreadable: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Counted {
    pub files: Vec<FileRecord>,
    pub gitattributes: Option<String>,
    pub skipped: Skipped,
}

/// Counts one file's contents.
pub fn count(path: String, language: LanguageType, bytes: &[u8]) -> FileRecord {
    let text = decode(bytes);
    let lines = Lines::of(&language.parse_from_slice(&text, &CONFIG));
    let inline_tests = if language == LanguageType::Rust {
        rust_inline_tests(&text, lines)
    } else {
        None
    };
    FileRecord {
        path,
        language,
        lines,
        inline_tests,
        generated: has_generated_marker(&text),
    }
}

/// Mirrors `LanguageType::parse`, which transcodes BOM-marked text first.
fn decode(bytes: &[u8]) -> Cow<'_, [u8]> {
    const BOMS: [&[u8]; 3] = [b"\xEF\xBB\xBF", b"\xFF\xFE", b"\xFE\xFF"];
    if !BOMS.iter().any(|bom| bytes.starts_with(bom)) {
        return Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    match DecodeReaderBytesBuilder::new()
        .build(bytes)
        .read_to_end(&mut out)
    {
        Ok(_) => Cow::Owned(out),
        Err(_) => Cow::Borrowed(bytes),
    }
}

fn rust_inline_tests(text: &[u8], whole: Lines) -> Option<Lines> {
    // Every test attribute contains "test"; most files can skip the scan.
    memmem::find(text, b"test")?;
    let Some(regions) = rust_tests::test_regions(text) else {
        return Some(whole);
    };
    if regions.is_empty() {
        return None;
    }
    let starts: Vec<usize> = std::iter::once(0)
        .chain(memchr::memchr_iter(b'\n', text).map(|i| i + 1))
        .collect();
    let mut tests = CodeStats::new();
    for (first, last) in regions {
        let start = starts[first];
        let end = starts.get(last + 1).copied().unwrap_or(text.len());
        tests += LanguageType::Rust.parse_from_slice(&text[start..end], &CONFIG);
    }
    Some(Lines::of(&tests).within(whole))
}

fn has_generated_marker(text: &[u8]) -> bool {
    const MARKERS: [&[u8]; 8] = [
        b"@generated",
        b"do not edit",
        b"code generated",
        b"auto-generated",
        b"autogenerated",
        b"automatically generated",
        b"file is generated",
        b"file was generated",
    ];
    let head = &text[..text.len().min(1024)];
    let end = memchr::memchr_iter(b'\n', head)
        .nth(4)
        .unwrap_or(head.len());
    let head = head[..end].to_ascii_lowercase();
    MARKERS.iter().any(|m| memmem::find(&head, m).is_some())
}

/// Language from the file name alone; `None` when it would need the contents.
fn language_by_name(path: &str) -> Option<LanguageType> {
    // tokei reads extensionless files to look for a shebang. The NUL byte keeps
    // that lookup from ever opening a real file with this relative path.
    LanguageType::from_path(Path::new("\0").join(path), &CONFIG)
}

fn language_by_shebang(head: &[u8]) -> Option<LanguageType> {
    if !head.starts_with(b"#!") {
        return None;
    }
    // tokei only exposes shebang detection for paths.
    let mut file = tempfile::NamedTempFile::new().ok()?;
    file.write_all(&head[..head.len().min(128)]).ok()?;
    LanguageType::from_shebang(file.path())
}

/// Counts every regular file in a tar stream, which may be gzip-compressed.
///
/// One thread reads the stream while the rayon pool counts. All counting stays
/// inside the pool: tokei uses rayon itself and must not block a foreign thread
/// on workers that are waiting for this queue.
pub fn count_archive(reader: impl Read + Send, limits: &Limits) -> io::Result<Archive> {
    let (tx, rx) = sync_channel::<(String, LanguageType, Vec<u8>)>(QUEUE_LEN);
    let budget = &Budget::default();
    thread::scope(|scope| {
        let reader = scope.spawn(move || {
            read_archive(reader, limits, |path, language, bytes| {
                budget.take(bytes.len());
                tx.send((path, language, bytes)).is_ok()
            })
        });
        let queued: Vec<FileRecord> = rx
            .into_iter()
            .par_bridge()
            .map(|(path, language, bytes)| {
                let record = count(path, language, &bytes);
                budget.give(bytes.len());
                record
            })
            .collect();
        let mut archive = reader.join().expect("archive reader panicked")?;
        archive.counted.files.extend(queued);
        archive.finish();
        Ok(archive)
    })
}

/// Bytes queued for counting; the reader waits while the budget is spent.
#[derive(Default)]
struct Budget {
    used: Mutex<usize>,
    freed: Condvar,
}

impl Budget {
    fn take(&self, n: usize) {
        let mut used = self.used.lock().unwrap();
        while *used > 0 && *used + n > QUEUE_BYTES {
            used = self.freed.wait(used).unwrap();
        }
        *used += n;
    }

    fn give(&self, n: usize) {
        *self.used.lock().unwrap() -= n;
        self.freed.notify_one();
    }
}

pub struct Archive {
    pub counted: Counted,
    /// Commit ID from the pax global header written by `git archive`.
    pub commit: Option<String>,
    root: Root,
    gitattributes: Vec<(String, String)>,
}

/// Tracks whether every entry shares one top-level directory (GitHub's
/// `repo-sha/` prefix), which is stripped before classification.
enum Root {
    Unknown,
    Shared(String),
    None,
}

impl Root {
    fn update(self, path: &str, is_dir: bool) -> Root {
        let top = match path.split_once('/') {
            Some((top, _)) => top,
            None if is_dir => path,
            None => return Root::None,
        };
        match self {
            Root::Unknown => Root::Shared(top.to_string()),
            Root::Shared(root) if root == top => Root::Shared(root),
            _ => Root::None,
        }
    }
}

impl Archive {
    fn finish(&mut self) {
        let prefix = match &self.root {
            Root::Shared(root) => format!("{root}/"),
            _ => String::new(),
        };
        for file in &mut self.counted.files {
            if let Some(rest) = file.path.strip_prefix(&prefix) {
                file.path = rest.to_string();
            }
        }
        self.counted.gitattributes = self
            .gitattributes
            .drain(..)
            .find(|(path, _)| path.strip_prefix(&prefix) == Some(".gitattributes"))
            .map(|(_, text)| text);
        self.counted
            .files
            .sort_unstable_by(|a, b| a.path.cmp(&b.path));
    }
}

fn read_archive(
    reader: impl Read,
    limits: &Limits,
    mut queue: impl FnMut(String, LanguageType, Vec<u8>) -> bool,
) -> io::Result<Archive> {
    let mut archive = Archive {
        counted: Counted::default(),
        commit: None,
        root: Root::Unknown,
        gitattributes: Vec::new(),
    };
    let mut reader = io::BufReader::with_capacity(1 << 16, reader);
    let gzip = reader.fill_buf()?.starts_with(b"\x1f\x8b");
    let decoded: Box<dyn Read> = if gzip {
        Box::new(flate2::bufread::MultiGzDecoder::new(reader))
    } else {
        Box::new(reader)
    };
    let mut tar = tar::Archive::new(Limited {
        inner: decoded,
        left: limits.max_unpacked,
        limit: limits.max_unpacked,
    });
    let skipped = &mut archive.counted.skipped;

    for (n, entry) in tar.entries()?.enumerate() {
        if n as u64 >= limits.max_entries {
            return Err(io::Error::other(format!(
                "archive has more than {} entries",
                limits.max_entries
            )));
        }
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind == EntryType::XGlobalHeader {
            for ext in entry.pax_extensions()?.into_iter().flatten().flatten() {
                if ext.key() == Ok("comment") {
                    archive.commit = ext.value().ok().map(str::to_string);
                }
            }
            continue;
        }
        let Some(path) = safe_path(&entry.path_bytes()) else {
            skipped.unsafe_paths += 1;
            continue;
        };
        if path.is_empty() {
            continue;
        }
        archive.root = archive.root.update(&path, kind.is_dir());
        if kind.is_symlink() || kind.is_hard_link() {
            skipped.links += 1;
            continue;
        }
        if !(kind.is_file() || kind.is_contiguous()) {
            continue;
        }
        let size = entry.size();
        if path.rsplit('/').next() == Some(".gitattributes")
            && path.matches('/').count() <= 1
            && size <= MAX_GITATTRIBUTES
        {
            // tokei has no language for .gitattributes; it is only kept for classification.
            let mut text = Vec::new();
            entry.read_to_end(&mut text)?;
            let text = String::from_utf8_lossy(&text).into_owned();
            archive.gitattributes.push((path, text));
            skipped.unrecognized += 1;
            continue;
        }
        let mut bytes = Vec::new();
        let language = match language_by_name(&path) {
            Some(language) => language,
            None if Path::new(&path).extension().is_none() => {
                (&mut entry).take(128).read_to_end(&mut bytes)?;
                match language_by_shebang(&bytes) {
                    Some(language) => language,
                    None => {
                        skipped.unrecognized += 1;
                        continue;
                    }
                }
            }
            None => {
                skipped.unrecognized += 1;
                continue;
            }
        };
        if size > limits.max_file {
            skipped.too_large += 1;
            continue;
        }
        bytes.reserve_exact((size as usize).saturating_sub(bytes.len()));
        entry.read_to_end(&mut bytes)?;
        if !queue(path, language, bytes) {
            break;
        }
    }
    Ok(archive)
}

/// Normalizes an archive path (`./a//b` is `a/b`, `./` is empty); rejects
/// absolute paths and `..` components.
fn safe_path(raw: &[u8]) -> Option<String> {
    let raw = String::from_utf8_lossy(raw);
    if raw.starts_with('/') {
        return None;
    }
    let mut parts = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// Fails the read once more than `left` bytes come out of the decoder.
struct Limited<R> {
    inner: R,
    left: u64,
    limit: u64,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.left = self.left.checked_sub(n as u64).ok_or_else(|| {
            io::Error::other(format!(
                "archive unpacks to more than {} MiB",
                self.limit >> 20
            ))
        })?;
        Ok(n)
    }
}

/// Counts a local directory, honoring .gitignore, .ignore and .tokeignore like
/// tokei, but including hidden files and skipping `.git`.
pub fn count_dir(root: &Path, limits: &Limits) -> Counted {
    let (tx, rx) = sync_channel::<ignore::DirEntry>(256);
    let unrecognized = AtomicU64::new(0);
    let too_large = AtomicU64::new(0);
    let links = AtomicU64::new(0);
    let unreadable = AtomicU64::new(0);

    let mut files: Vec<FileRecord> = thread::scope(|scope| {
        scope.spawn(|| {
            WalkBuilder::new(root)
                .hidden(false)
                .add_custom_ignore_filename(".tokeignore")
                .filter_entry(|e| e.file_name() != ".git")
                .build_parallel()
                .run(|| {
                    let tx = tx.clone();
                    let (links, unreadable) = (&links, &unreadable);
                    Box::new(move |entry| {
                        match entry {
                            Ok(e) if e.file_type().is_some_and(|t| t.is_file()) => {
                                if tx.send(e).is_err() {
                                    return WalkState::Quit;
                                }
                            }
                            Ok(e) if e.path_is_symlink() => {
                                links.fetch_add(1, Relaxed);
                            }
                            Ok(_) => {}
                            Err(_) => {
                                unreadable.fetch_add(1, Relaxed);
                            }
                        }
                        WalkState::Continue
                    })
                });
            drop(tx);
        });
        rx.into_iter()
            .par_bridge()
            .filter_map(|entry| {
                let path = entry.path();
                let Some(language) = LanguageType::from_path(path, &CONFIG) else {
                    unrecognized.fetch_add(1, Relaxed);
                    return None;
                };
                if entry.metadata().is_ok_and(|m| m.len() > limits.max_file) {
                    too_large.fetch_add(1, Relaxed);
                    return None;
                }
                let Ok(bytes) = fs::read(path) else {
                    unreadable.fetch_add(1, Relaxed);
                    return None;
                };
                let relative = path.strip_prefix(root).unwrap_or(path);
                let relative = relative.to_string_lossy().replace('\\', "/");
                Some(count(relative, language, &bytes))
            })
            .collect()
    });
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));

    let gitattributes = root.join(".gitattributes");
    let gitattributes = fs::metadata(&gitattributes)
        .is_ok_and(|m| m.is_file() && m.len() <= MAX_GITATTRIBUTES)
        .then(|| fs::read_to_string(&gitattributes).ok())
        .flatten();
    Counted {
        files,
        gitattributes,
        skipped: Skipped {
            unrecognized: unrecognized.into_inner(),
            too_large: too_large.into_inner(),
            links: links.into_inner(),
            unsafe_paths: 0,
            unreadable: unreadable.into_inner(),
        },
    }
}

#[cfg(test)]
mod tests {
    use flate2::{Compression, write::GzEncoder};
    use tar::Header;

    use super::*;

    /// A gzipped tar with raw (unvalidated) entry names.
    fn tarball(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for &(name, kind, data) in entries {
            let mut header = Header::new_gnu();
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_entry_type(kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            if kind.is_symlink() || kind.is_hard_link() {
                header.set_link_name("/etc/passwd").unwrap();
            }
            header.set_cksum();
            builder.append(&header, data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn archive_entries_stay_contained() {
        let code: &[u8] = b"fn main() {}\n";
        let bytes = tarball(&[
            ("./", EntryType::Directory, b""),
            ("repo-abc/", EntryType::Directory, b""),
            ("repo-abc/src/ok.rs", EntryType::Regular, code),
            ("repo-abc/../evil.rs", EntryType::Regular, code),
            ("/abs.rs", EntryType::Regular, code),
            ("repo-abc/link.rs", EntryType::Symlink, b""),
            ("repo-abc/hard.rs", EntryType::Link, b""),
        ]);
        let archive = count_archive(bytes.as_slice(), &LIMITS).unwrap();
        let paths: Vec<_> = archive
            .counted
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(paths, ["src/ok.rs"]);
        assert_eq!(archive.counted.skipped.unsafe_paths, 2);
        assert_eq!(archive.counted.skipped.links, 2);

        let limits = Limits {
            max_unpacked: 4096,
            ..LIMITS
        };
        let big = vec![b'\n'; 8192];
        let bomb = tarball(&[("repo/a.rs", EntryType::Regular, &big)]);
        assert!(count_archive(bomb.as_slice(), &limits).is_err());
    }
}
