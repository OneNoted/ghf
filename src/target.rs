//! Interpreting the TARGET argument.

use std::path::PathBuf;

use crate::github::Repo;

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    GitHub {
        repo: Repo,
        reference: Option<String>,
    },
    Dir(PathBuf),
    Archive(PathBuf),
}

pub fn parse(input: Option<&str>, reference: Option<&str>) -> Result<Target, String> {
    let Some(input) = input else {
        return local(".", reference);
    };
    let target = if let Some(path) = github_path(input)? {
        url(path, input)?
    } else if let Some(repo) = repo(input).filter(|_| !input.starts_with('.')) {
        Target::GitHub {
            repo,
            reference: None,
        }
    } else {
        return local(input, reference);
    };
    match (target, reference) {
        (
            Target::GitHub {
                repo,
                reference: None,
            },
            explicit,
        ) => Ok(Target::GitHub {
            repo,
            reference: explicit.map(checked_ref).transpose()?,
        }),
        (
            Target::GitHub {
                reference: Some(a), ..
            },
            Some(b),
        ) if a != b => Err(format!(
            "conflicting refs: '{a}' in the URL and --ref '{b}'"
        )),
        (target, _) => Ok(target),
    }
}

fn local(input: &str, reference: Option<&str>) -> Result<Target, String> {
    if reference.is_some() {
        return Err("--ref only applies to GitHub repositories".into());
    }
    let path = PathBuf::from(input);
    let lower = input.to_ascii_lowercase();
    if path.is_dir() {
        Ok(Target::Dir(path))
    } else if path.is_file()
        && [".tar", ".tar.gz", ".tgz"]
            .iter()
            .any(|ext| lower.ends_with(ext))
    {
        Ok(Target::Archive(path))
    } else if path.is_file() {
        Err(format!(
            "{input}: expected a directory or a .tar/.tar.gz archive"
        ))
    } else {
        Err(format!("{input}: no such directory, archive or owner/repo"))
    }
}

/// Parses the path part of a GitHub URL: owner/repo[.git][/tree/REF|/commit/SHA|/releases/tag/TAG].
fn url(path: &str, input: &str) -> Result<Target, String> {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let mut parts = path.trim_end_matches('/').splitn(3, '/');
    let (owner, name) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    let name = name.strip_suffix(".git").unwrap_or(name);
    let repo = repo(&format!("{owner}/{name}"))
        .ok_or_else(|| format!("not a GitHub repository URL: {input}"))?;
    let reference = match parts.next() {
        None => None,
        Some(rest) => {
            let reference = ["tree/", "commit/", "releases/tag/"]
                .iter()
                .find_map(|p| rest.strip_prefix(p))
                .ok_or_else(|| format!("unsupported GitHub URL: {input}"))?;
            Some(checked_ref(&percent_decode(reference))?)
        }
    };
    Ok(Target::GitHub { repo, reference })
}

/// The `owner/repo/...` part of a GitHub URL, or `None` if `input` is no URL.
fn github_path(input: &str) -> Result<Option<&str>, String> {
    let rest = ["https://", "http://", "ssh://git@", "git@"]
        .iter()
        .find_map(|scheme| input.strip_prefix(scheme))
        .or_else(|| input.starts_with("github.com/").then_some(input));
    let Some(rest) = rest else { return Ok(None) };
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    rest.strip_prefix("github.com/")
        .or_else(|| rest.strip_prefix("github.com:"))
        .map(Some)
        .ok_or_else(|| format!("not a GitHub repository URL: {input}"))
}

fn repo(input: &str) -> Option<Repo> {
    let (owner, name) = input.split_once('/')?;
    let owner_ok = (1..=39).contains(&owner.len())
        && !owner.starts_with('-')
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let name_ok = (1..=100).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b));
    (owner_ok && name_ok).then(|| Repo {
        owner: owner.into(),
        name: name.into(),
    })
}

/// Rejects strings that cannot be Git refs, keeping API paths unambiguous.
fn checked_ref(reference: &str) -> Result<String, String> {
    let bad = reference.is_empty()
        || reference.len() > 255
        || reference.starts_with(['-', '/'])
        || reference.contains("..")
        || reference
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c));
    if bad {
        Err(format!(
            "invalid ref: {}",
            crate::report::sanitize(reference)
        ))
    } else {
        Ok(reference.to_string())
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh(owner: &str, name: &str, reference: Option<&str>) -> Target {
        Target::GitHub {
            repo: Repo {
                owner: owner.into(),
                name: name.into(),
            },
            reference: reference.map(Into::into),
        }
    }

    #[test]
    fn targets() {
        for (input, expected) in [
            ("OneNoted/ghf", gh("OneNoted", "ghf", None)),
            (
                "https://github.com/OneNoted/ghf.git",
                gh("OneNoted", "ghf", None),
            ),
            (
                "github.com/a/b/tree/feature/x",
                gh("a", "b", Some("feature/x")),
            ),
            (
                "https://www.github.com/a/b/commit/abc123?x=1",
                gh("a", "b", Some("abc123")),
            ),
            (
                "https://github.com/a/b/releases/tag/v1.0",
                gh("a", "b", Some("v1.0")),
            ),
            ("git@github.com:a/b.git", gh("a", "b", None)),
        ] {
            assert_eq!(parse(Some(input), None).unwrap(), expected, "{input}");
        }
        assert_eq!(
            parse(Some("a/b"), Some("v2")).unwrap(),
            gh("a", "b", Some("v2"))
        );
        assert!(parse(Some("https://gitlab.com/a/b"), None).is_err());
        assert!(parse(Some("https://github.com/a/b/issues"), None).is_err());
        assert!(parse(Some("a/b"), Some("x..y")).is_err());
        assert!(parse(Some("github.com/a/b/tree/v1"), Some("v2")).is_err());
        assert_eq!(parse(Some("."), None).unwrap(), Target::Dir(".".into()));
    }
}
