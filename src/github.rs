//! GitHub access: token lookup, ref resolution and snapshot downloads.

use std::{
    fmt,
    io::{self, Read},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use ureq::{
    Agent, Body,
    config::RedirectAuthHeaders,
    http::{Response, StatusCode},
};

const API: &str = "https://api.github.com";
pub const MAX_DOWNLOAD: u64 = 2 << 30;
const GH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A token and where it came from. Never printed, logged or cached.
struct Token {
    secret: String,
    source: &'static str,
}

pub struct Client {
    agent: Agent,
    token: OnceLock<Option<Token>>,
}

/// The commit a run counted, and how it was chosen.
pub struct Snapshot {
    pub repo: Repo,
    /// The requested ref, or the default branch's name.
    pub label: String,
    pub default_branch: bool,
    pub commit: String,
}

pub fn agent(https_only: bool) -> Agent {
    Agent::config_builder()
        .https_only(https_only)
        .http_status_as_error(false)
        .max_redirects(5)
        // Authorization only follows redirects to the same host over HTTPS
        // (renamed repositories). Tarball downloads redirect to another host
        // with their own short-lived credential and never see the token.
        .redirect_auth_headers(RedirectAuthHeaders::SameHost)
        .user_agent(concat!("ghf/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(120)))
        .timeout_recv_body(Some(Duration::from_secs(3600)))
        .build()
        .into()
}

impl Client {
    pub fn new() -> Self {
        Client {
            agent: agent(true),
            token: OnceLock::new(),
        }
    }

    /// Where the token came from, if one was needed so far.
    pub fn auth_source(&self) -> &'static str {
        match self.token.get() {
            None => "not needed",
            Some(None) => "none",
            Some(Some(token)) => token.source,
        }
    }

    fn token(&self) -> Option<&Token> {
        self.token
            .get_or_init(|| {
                for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
                    if let Some(secret) = std::env::var(var).ok().and_then(valid_token) {
                        return Some(Token {
                            secret,
                            source: var,
                        });
                    }
                }
                gh_auth_token().map(|secret| Token {
                    secret,
                    source: "gh auth token",
                })
            })
            .as_ref()
    }

    fn get(&self, url: &str, accept: &str, auth: bool) -> Result<Response<Body>, String> {
        let mut request = self
            .agent
            .get(url)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = auth.then(|| self.token()).flatten() {
            request = request.header("Authorization", format!("Bearer {}", token.secret));
        }
        request.call().map_err(|e| describe(&e))
    }

    fn api(&self, path: &str, accept: &str, what: &str) -> Result<Response<Body>, String> {
        let response = self.get(&format!("{API}{path}"), accept, true)?;
        match response.status() {
            StatusCode::OK => Ok(response),
            status => Err(self.status_error(status, &response, what)),
        }
    }

    fn status_error(&self, status: StatusCode, response: &Response<Body>, what: &str) -> String {
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let hint = match self.token() {
            Some(t) => format!("using the token from {}", t.source),
            None => "set GH_TOKEN or run `gh auth login` for private repositories".into(),
        };
        match status.as_u16() {
            404 => format!("{what} not found ({hint})"),
            422 => format!("{what} not found"),
            409 => format!("{what}: repository is empty"),
            401 => format!(
                "GitHub rejected the token from {}",
                self.token().map_or("the environment", |t| t.source)
            ),
            403 | 429 if header("x-ratelimit-remaining").as_deref() == Some("0") => {
                let reset = header("x-ratelimit-reset")
                    .and_then(|r| r.parse::<u64>().ok())
                    .and_then(|r| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()?
                            .as_secs();
                        Some(format!(
                            "; resets in {} min",
                            r.saturating_sub(now).div_ceil(60)
                        ))
                    })
                    .unwrap_or_default();
                format!("GitHub API rate limit exceeded{reset} ({hint})")
            }
            code => format!("{what}: GitHub returned HTTP {code} ({hint})"),
        }
    }

    /// Resolves `reference` (or the default branch) to a commit.
    pub fn resolve(&self, repo: &Repo, reference: Option<&str>) -> Result<Snapshot, String> {
        let base = format!("/repos/{}/{}", repo.owner, repo.name);
        if let Some(reference) = reference {
            let commit = self.commit(&base, reference, &format!("ref '{reference}' in {repo}"))?;
            return Ok(Snapshot {
                repo: repo.clone(),
                label: reference.to_string(),
                default_branch: false,
                commit,
            });
        }
        #[derive(Deserialize)]
        struct RepoInfo {
            full_name: String,
            default_branch: String,
        }
        self.token();
        let what = format!("repository {repo}");
        let (info, commit) = thread::scope(|s| {
            let commit = s.spawn(|| self.commit(&base, "HEAD", &what));
            let info = self
                .api(&base, "application/vnd.github+json", &what)
                .and_then(|mut r| {
                    let text = r
                        .body_mut()
                        .with_config()
                        .limit(1 << 20)
                        .read_to_string()
                        .map_err(|e| describe(&e))?;
                    serde_json::from_str::<RepoInfo>(&text)
                        .map_err(|_| format!("{what}: unexpected response from GitHub"))
                });
            (info, commit.join().expect("resolver panicked"))
        });
        let (info, commit) = (info?, commit?);
        let repo = match info.full_name.split_once('/') {
            Some((owner, name)) => Repo {
                owner: owner.into(),
                name: name.into(),
            },
            None => repo.clone(),
        };
        Ok(Snapshot {
            repo,
            label: info.default_branch,
            default_branch: true,
            commit,
        })
    }

    fn commit(&self, base: &str, reference: &str, what: &str) -> Result<String, String> {
        let path = format!("{base}/commits/{}", encode_ref(reference));
        let sha = self
            .api(&path, "application/vnd.github.sha", what)?
            .body_mut()
            .with_config()
            .limit(256)
            .read_to_string()
            .map_err(|e| describe(&e))?;
        let sha = sha.trim();
        if !is_full_sha(sha) {
            return Err(format!("{what}: unexpected commit ID from GitHub"));
        }
        Ok(sha.to_ascii_lowercase())
    }

    /// Streams the snapshot tarball (tar.gz) of `commit`.
    pub fn download(&self, repo: &Repo, commit: &str) -> Result<impl Read + Send + use<>, String> {
        let what = format!("snapshot {commit} of {repo}");
        // Public snapshots come straight from codeload: no token, no API rate
        // limit. Private ones go through the API, which redirects to a
        // pre-authorized codeload URL (the token is not forwarded there).
        let url = format!(
            "https://codeload.github.com/{}/{}/tar.gz/{commit}",
            repo.owner, repo.name
        );
        let mut response = self.get(&url, "*/*", false)?;
        if response.status() == StatusCode::NOT_FOUND && self.token().is_some() {
            response = self.api(
                &format!("/repos/{}/{}/tarball/{commit}", repo.owner, repo.name),
                "application/vnd.github+json",
                &what,
            )?;
        }
        if response.status() != StatusCode::OK {
            return Err(self.status_error(response.status(), &response, &what));
        }
        Ok(response
            .into_body()
            .into_with_config()
            .limit(MAX_DOWNLOAD)
            .reader())
    }
}

pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Percent-encodes a ref for a URL path, keeping `/` (refs may contain it).
fn encode_ref(reference: &str) -> String {
    let mut out = String::with_capacity(reference.len());
    for b in reference.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn valid_token(token: String) -> Option<String> {
    let token = token.trim();
    (!token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic())).then(|| token.to_string())
}

/// Asks the GitHub CLI for its token. The token arrives on a pipe, never in
/// arguments or the environment of another process.
fn gh_auth_token() -> Option<String> {
    let mut child = Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < GH_TIMEOUT => thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut out = String::new();
    child
        .stdout
        .take()?
        .take(4096)
        .read_to_string(&mut out)
        .ok()?;
    status.success().then_some(out).and_then(valid_token)
}

/// Describes a transport error without echoing URLs, which may carry
/// short-lived download credentials after a redirect.
pub fn describe(error: &ureq::Error) -> String {
    use ureq::Error as E;
    match error {
        E::Io(e) => format!("network error: {}", e.kind()),
        E::Timeout(t) => format!("timed out ({t})"),
        E::HostNotFound => "could not resolve host".into(),
        E::ConnectionFailed => "connection failed".into(),
        E::TooManyRedirects | E::RedirectFailed => "redirect failed".into(),
        E::RequireHttpsOnly(_) => "refused a non-HTTPS URL".into(),
        E::BodyExceedsLimit(limit) => format!("response larger than {} MiB", limit >> 20),
        E::Tls(e) => format!("TLS error: {e}"),
        E::StatusCode(code) => format!("HTTP {code}"),
        _ => "HTTP error".into(),
    }
}

/// Describes an I/O error from a download stream, unwrapping transport errors.
pub fn describe_io(error: &io::Error) -> String {
    match error
        .get_ref()
        .and_then(|e| e.downcast_ref::<ureq::Error>())
    {
        Some(e) => describe(e),
        None => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
    };

    use super::*;

    /// Serves one request on a local port and returns the request head.
    fn serve_once(host: &str, response: String) -> (u16, thread::JoinHandle<String>) {
        let listener = TcpListener::bind((host, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            while reader.read_line(&mut head).unwrap() > 2 && !head.ends_with("\r\n\r\n") {}
            (&stream).write_all(response.as_bytes()).unwrap();
            head
        });
        (port, handle)
    }

    // Linux routes all of 127/8 to loopback, giving a second local host.
    #[cfg(target_os = "linux")]
    #[test]
    fn authorization_is_not_forwarded_to_another_host() {
        let (target, target_head) = serve_once(
            "127.0.0.2",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
        );
        let (origin, origin_head) = serve_once(
            "127.0.0.1",
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:{target}/x?token=t\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        let body = agent(false)
            .get(format!("http://127.0.0.1:{origin}/"))
            .header("Authorization", "Bearer secret")
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap();
        assert_eq!(body, "ok");
        let (origin_head, target_head) = (origin_head.join().unwrap(), target_head.join().unwrap());
        assert!(origin_head.contains("Bearer secret"));
        assert!(!target_head.to_ascii_lowercase().contains("authorization"));
    }

    #[test]
    fn https_only_rejects_plain_redirects_without_echoing_them() {
        let error = agent(true)
            .get("http://127.0.0.1:9/?token=t")
            .call()
            .unwrap_err();
        assert!(!describe(&error).contains("token"));
        assert_eq!(encode_ref("feature/a b?#"), "feature/a%20b%3F%23");
    }
}
