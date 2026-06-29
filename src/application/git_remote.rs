//! Git remote detection and normalisation.
//!
//! Used to attach a stable, portable identifier to each indexed repository.
//! Unlike the on-disk path (which changes when a repository is cloned to a
//! different location or machine), the git remote survives clones, so it is the
//! key used to auto-resolve which namespace a repository was indexed under.
//!
//! This module only touches the filesystem (parsing `.git/config`); it performs
//! no network access and shells out to nothing. It lives in the application
//! layer alongside the indexing use case, which already reads the filesystem
//! directly.

use std::fs;
use std::path::{Path, PathBuf};

/// Detect the normalised git remote for the repository containing `start`.
///
/// Walks up from `start` looking for a `.git` directory (or a `.git` file, as
/// used by worktrees and submodules), parses its `config`, and returns the
/// normalised `origin` remote (falling back to the first remote found).
///
/// Returns `None` when the path is not inside a git repository, the config has
/// no remotes, or the remote URL cannot be parsed.
pub fn detect_remote(start: &Path) -> Option<String> {
    let config_dir = find_git_config_dir(start)?;
    let config = fs::read_to_string(config_dir.join("config")).ok()?;
    let remotes = parse_remote_urls(&config);

    // Prefer `origin`; otherwise take the first remote in declaration order.
    let chosen = remotes
        .iter()
        .find(|(name, _)| name == "origin")
        .or_else(|| remotes.first())?;

    normalize_remote(&chosen.1)
}

/// Normalise a git remote URL into a canonical `host/path` form so that the same
/// repository matches regardless of the protocol used to clone it.
///
/// Examples (all normalise to `github.com/owner/repo`):
/// - `git@github.com:owner/repo.git`
/// - `https://github.com/owner/repo.git`
/// - `ssh://git@github.com:22/owner/repo`
/// - `git://github.com/owner/repo.git`
///
/// Returns `None` for an empty input.
pub fn normalize_remote(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }

    let has_scheme = s.contains("://");

    // 1. Strip the `scheme://` prefix if present.
    let after_scheme = match s.split_once("://") {
        Some((_scheme, rest)) => rest,
        None => s,
    };

    // 2. Strip any `user@` / `git@` userinfo.
    let after_user = match after_scheme.split_once('@') {
        Some((_user, rest)) => rest,
        None => after_scheme,
    };

    // 3. Split host from path. Scheme URLs and `host/path` use `/`; scp-style
    //    URLs (`git@host:owner/repo`) use the first `:` as the separator.
    let (host, path) = if has_scheme {
        after_user.split_once('/').unwrap_or((after_user, ""))
    } else if let Some(parts) = after_user.split_once(':') {
        parts
    } else {
        after_user.split_once('/').unwrap_or((after_user, ""))
    };

    // 4. Drop any `:port` suffix from the host and lower-case it (hosts are
    //    case-insensitive; paths are left untouched).
    let host = host
        .split(':')
        .next()
        .unwrap_or(host)
        .trim()
        .to_ascii_lowercase();

    // 5. Trim surrounding slashes and a trailing `.git` from the path.
    let path = path.trim().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);

    match (host.is_empty(), path.is_empty()) {
        (true, true) => None,
        (false, true) => Some(host),
        (true, false) => Some(path.to_string()),
        (false, false) => Some(format!("{host}/{path}")),
    }
}

/// Walk up from `start` to locate the directory that holds the git `config`
/// file for the enclosing repository.
fn find_git_config_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start
    } else {
        start.parent()?
    };

    loop {
        let git_path = dir.join(".git");
        if git_path.is_dir() {
            return Some(resolve_common_config_dir(&git_path));
        }
        if git_path.is_file() {
            // Worktree / submodule: `.git` is a file containing `gitdir: <path>`.
            if let Some(gitdir) = read_gitdir_file(&git_path, dir) {
                return Some(resolve_common_config_dir(&gitdir));
            }
        }
        dir = dir.parent()?;
    }
}

/// Resolve the directory that actually contains the shared `config` file.
///
/// For a linked worktree, `git_dir` is `…/.git/worktrees/<name>` and the
/// remotes live in the common git directory pointed to by a `commondir` file.
fn resolve_common_config_dir(git_dir: &Path) -> PathBuf {
    if let Ok(common) = fs::read_to_string(git_dir.join("commondir")) {
        let common = common.trim();
        if !common.is_empty() {
            let candidate = git_dir.join(common);
            // Normalise away the `worktrees/<name>/../..` indirection when possible.
            return candidate.canonicalize().unwrap_or(candidate);
        }
    }
    git_dir.to_path_buf()
}

/// Parse the `gitdir: <path>` pointer from a `.git` file, resolving relative
/// paths against `base` (the directory containing the `.git` file).
fn read_gitdir_file(git_file: &Path, base: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(git_file).ok()?;
    let rest = content.trim().strip_prefix("gitdir:")?.trim();
    if rest.is_empty() {
        return None;
    }
    let path = Path::new(rest);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Some(resolved.canonicalize().unwrap_or(resolved))
}

/// Extract `(remote_name, url)` pairs from the contents of a git `config` file,
/// in declaration order.
fn parse_remote_urls(config: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;

    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            current = parse_remote_section(line);
        } else if let Some(name) = &current {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("url") {
                    out.push((name.clone(), value.trim().to_string()));
                }
            }
        }
    }
    out
}

/// Parse a section header, returning the remote name for `[remote "name"]`
/// headers and `None` for any other section.
fn parse_remote_section(line: &str) -> Option<String> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    let rest = inner.strip_prefix("remote")?;
    // Require a separator after `remote` so `[remotefoo]` is not misread.
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '"') {
        return None;
    }
    let name = rest.trim().trim_matches('"').trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_scp_form() {
        assert_eq!(
            normalize_remote("git@github.com:owner/repo.git").as_deref(),
            Some("github.com/owner/repo")
        );
    }

    #[test]
    fn normalizes_https_form() {
        assert_eq!(
            normalize_remote("https://github.com/owner/repo.git").as_deref(),
            Some("github.com/owner/repo")
        );
    }

    #[test]
    fn normalizes_ssh_form_with_port() {
        assert_eq!(
            normalize_remote("ssh://git@github.com:22/owner/repo").as_deref(),
            Some("github.com/owner/repo")
        );
    }

    #[test]
    fn normalizes_git_protocol() {
        assert_eq!(
            normalize_remote("git://github.com/owner/repo.git").as_deref(),
            Some("github.com/owner/repo")
        );
    }

    #[test]
    fn lowercases_host_only() {
        assert_eq!(
            normalize_remote("git@GitHub.com:Owner/Repo.git").as_deref(),
            Some("github.com/Owner/Repo")
        );
    }

    #[test]
    fn all_protocols_agree() {
        let forms = [
            "git@github.com:owner/repo.git",
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo",
            "ssh://git@github.com/owner/repo.git",
        ];
        let normalized: Vec<_> = forms.iter().filter_map(|f| normalize_remote(f)).collect();
        assert!(normalized.iter().all(|n| n == "github.com/owner/repo"));
        assert_eq!(normalized.len(), forms.len());
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(normalize_remote("   "), None);
    }

    #[test]
    fn parses_origin_preferred_over_others() {
        let config = r#"
[core]
    bare = false
[remote "upstream"]
    url = https://github.com/upstream/repo.git
    fetch = +refs/heads/*:refs/remotes/upstream/*
[remote "origin"]
    url = git@github.com:owner/repo.git
    fetch = +refs/heads/*:refs/remotes/origin/*
[branch "main"]
    remote = origin
"#;
        let remotes = parse_remote_urls(config);
        assert_eq!(remotes.len(), 2);
        let origin = remotes.iter().find(|(n, _)| n == "origin").unwrap();
        assert_eq!(origin.1, "git@github.com:owner/repo.git");
    }

    #[test]
    fn ignores_non_remote_sections() {
        let config = "[remotefoo]\n    url = nope\n[core]\n    url = also-nope\n";
        assert!(parse_remote_urls(config).is_empty());
    }

    #[test]
    fn detect_remote_reads_repo_config() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(
            git.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:owner/repo.git\n",
        )
        .unwrap();

        // Detect from a nested subdirectory to exercise the walk-up.
        let nested = dir.path().join("src").join("inner");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            detect_remote(&nested).as_deref(),
            Some("github.com/owner/repo")
        );
    }

    #[test]
    fn detect_remote_none_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_remote(dir.path()), None);
    }
}
