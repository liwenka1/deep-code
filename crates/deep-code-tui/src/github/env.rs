//! Probing the surroundings: which repository are we in, and is `gh` usable.
//!
//! Everything here degrades rather than aborts — a missing `gh` still lets the
//! workflow file be written, with the leftover manual steps printed. Being
//! told "here are the two secrets to set yourself" beats being refused.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// `owner/repo` parsed off the git remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoSlug {
    pub owner: String,
    pub name: String,
}

impl RepoSlug {
    pub fn full(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// Extract `owner/repo` from any spelling of a GitHub remote URL.
///
/// Pure so the URL shapes are testable without a repository: HTTPS, SSH
/// (`git@…:owner/repo`), `ssh://`, and each with or without `.git`.
pub(crate) fn parse_remote(url: &str) -> Option<RepoSlug> {
    let url = url.trim();
    let rest = if let Some(rest) = url.strip_prefix("git@") {
        // git@github.com:owner/repo.git
        rest.split_once(':').map(|(_, path)| path)?
    } else {
        // https://github.com/owner/repo.git, ssh://git@github.com/owner/repo
        let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
        let (host, path) = after_scheme.split_once('/')?;
        if !host.contains("github") {
            return None;
        }
        path
    };

    let path = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(RepoSlug { owner, name })
}

fn capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Absolute path to the enclosing work tree, or `None` outside a repository.
pub(crate) fn git_root() -> Option<PathBuf> {
    capture("git", &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// The GitHub repository this work tree pushes to. Tries `origin` first, then
/// any remote, so a fork checkout with a differently-named remote still works.
pub(crate) fn detect_repo() -> Option<RepoSlug> {
    if let Some(slug) =
        capture("git", &["remote", "get-url", "origin"]).and_then(|u| parse_remote(&u))
    {
        return Some(slug);
    }
    let remotes = capture("git", &["remote"])?;
    remotes
        .lines()
        .filter_map(|remote| capture("git", &["remote", "get-url", remote.trim()]))
        .find_map(|url| parse_remote(&url))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GhStatus {
    Ready,
    NotAuthenticated,
    NotInstalled,
}

pub(crate) fn gh_status() -> GhStatus {
    if capture("gh", &["--version"]).is_none() {
        return GhStatus::NotInstalled;
    }
    // `gh auth status` exits non-zero when logged out; it prints to stderr, so
    // the exit code is the only signal worth reading.
    match Command::new("gh")
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => GhStatus::Ready,
        _ => GhStatus::NotAuthenticated,
    }
}

/// Store a repository secret. The value goes in on stdin, never in argv —
/// a command line is visible to every other process on the machine.
pub(crate) fn set_secret(repo: &RepoSlug, name: &str, value: &str) -> Result<(), String> {
    use std::io::Write;

    let mut child = Command::new("gh")
        .args(["secret", "set", name, "--repo", &repo.full()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run gh: {error}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("gh stdin unavailable")?
        .write_all(value.as_bytes())
        .map_err(|error| format!("cannot write the secret to gh: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("gh did not finish: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Names of the secrets already configured on the repository.
pub(crate) fn list_secrets(repo: &RepoSlug) -> Option<Vec<String>> {
    let json = capture(
        "gh",
        &["secret", "list", "--repo", &repo.full(), "--json", "name"],
    )?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(|entry| Some(entry.get("name")?.as_str()?.to_string()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_github_remote_spelling() {
        let expected = RepoSlug {
            owner: "liwenka1".to_string(),
            name: "deep-code".to_string(),
        };
        for url in [
            "https://github.com/liwenka1/deep-code.git",
            "https://github.com/liwenka1/deep-code",
            "git@github.com:liwenka1/deep-code.git",
            "git@github.com:liwenka1/deep-code",
            "ssh://git@github.com/liwenka1/deep-code.git",
            "  https://github.com/liwenka1/deep-code/  ",
        ] {
            assert_eq!(
                parse_remote(url).as_ref(),
                Some(&expected),
                "failed on {url}"
            );
        }
    }

    #[test]
    fn rejects_non_github_and_malformed_remotes() {
        for url in [
            "https://gitlab.com/liwenka1/deep-code.git",
            "https://github.com/liwenka1",
            "",
            "not a url",
        ] {
            assert_eq!(parse_remote(url), None, "should not parse: {url}");
        }
    }

    #[test]
    fn enterprise_hosts_named_github_still_parse() {
        // github.example.com is a real deployment shape; the pipeline works
        // there, so the slug parser must not insist on the public hostname.
        assert_eq!(
            parse_remote("https://github.example.com/team/repo.git"),
            Some(RepoSlug {
                owner: "team".to_string(),
                name: "repo".to_string()
            })
        );
    }

    #[test]
    fn slug_renders_as_owner_slash_name() {
        assert_eq!(
            parse_remote("git@github.com:a/b.git").unwrap().full(),
            "a/b"
        );
    }
}
