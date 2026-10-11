//! Per-commit cache of counted files.
//!
//! Commits are immutable, so an entry keyed by commit never goes stale; refs
//! are always resolved live. Entries embed the ghf version (which pins the
//! tokei version) and are ignored after an upgrade. Tokens are never stored.

use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::PathBuf,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::{count::Counted, github::Repo};

const MAX_BYTES: u64 = 256 << 20;
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Cache {
    dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Entry<C> {
    ghf: String,
    commit: String,
    counted: C,
}

impl Cache {
    pub fn open() -> Option<Cache> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::home_dir().map(|h| h.join(".cache")))?;
        Some(Cache {
            dir: base.join("ghf"),
        })
    }

    fn path(&self, repo: &Repo, commit: &str) -> PathBuf {
        // Owner names cannot contain '_', so the key is unambiguous.
        let key = format!("{}_{}_{commit}.json", repo.owner, repo.name).to_ascii_lowercase();
        self.dir.join(key)
    }

    pub fn load(&self, repo: &Repo, commit: &str) -> Option<Counted> {
        let path = self.path(repo, commit);
        let file = File::open(&path).ok()?;
        let entry: Entry<Counted> = serde_json::from_reader(BufReader::new(&file)).ok()?;
        if entry.ghf != VERSION || entry.commit != commit {
            return None;
        }
        // Recently used entries survive pruning.
        let _ = File::options()
            .append(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        Some(entry.counted)
    }

    pub fn store(&self, repo: &Repo, commit: &str, counted: &Counted) -> io::Result<()> {
        create_private_dir(&self.dir)?;
        let entry = Entry {
            ghf: VERSION.into(),
            commit: commit.into(),
            counted,
        };
        // Write a private temporary file, then rename: readers never see partial entries.
        let mut tmp = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(&self.dir)?;
        {
            let mut out = BufWriter::new(tmp.as_file_mut());
            serde_json::to_writer(&mut out, &entry)?;
            out.flush()?;
        }
        tmp.persist(self.path(repo, commit)).map_err(|e| e.error)?;
        self.prune();
        Ok(())
    }

    /// Deletes least recently used entries beyond the size budget.
    fn prune(&self) {
        let Ok(dir) = fs::read_dir(&self.dir) else {
            return;
        };
        let stale = SystemTime::now() - Duration::from_secs(3600);
        let mut entries: Vec<_> = dir
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let modified = meta.modified().ok()?;
                let name = e.file_name();
                let name = name.to_string_lossy();
                // Leftovers of interrupted writes.
                if name.starts_with(".tmp-") && modified < stale {
                    let _ = fs::remove_file(e.path());
                }
                name.ends_with(".json")
                    .then(|| (modified, meta.len(), e.path()))
            })
            .collect();
        let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();
        entries.sort();
        for (_, len, path) in entries {
            if total <= MAX_BYTES {
                break;
            }
            if fs::remove_file(path).is_ok() {
                total -= len;
            }
        }
    }
}

fn create_private_dir(dir: &PathBuf) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}
