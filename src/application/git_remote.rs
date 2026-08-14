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

/// Derive a per-repository namespace name for `root`.
///
/// Returns `owner/repo` — the normalised remote with its host stripped — so
/// that two repositories sharing a short name under different owners do not
/// collide. Falls back to the directory basename when there is no git remote
/// (not a repository, or none configured), and to `None` when even that is
/// unavailable, leaving the caller on [`crate::cli::DEFAULT_NAMESPACE`].
///
/// A `/` in the name is safe: the namespace is never used as a SQL identifier
/// or a filename. Each namespace is backed by a generated `ns_<uuid>` schema
/// token, and the user-facing name is only ever stored and matched as data
/// via bound parameters.
pub fn derive_namespace(root: &Path) -> Option<String> {
    if let Some(remote) = detect_remote(root).as_deref().and_then(normalize_remote) {
        // `normalize_remote` yields `host/owner/repo`; drop the host so the
        // name reads as the project rather than where it happens to be hosted.
        let without_host = remote.split_once('/').map(|(_, rest)| rest);
        if let Some(path) = without_host.filter(|p| !p.is_empty()) {
            return Some(path.to_string());
        }
        // A remote with a host but no path is not a useful name; fall through.
    }

    root.file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
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

#[cfg(test)]
mod derive_namespace_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// A directory with a git config declaring `remotes`, as `[name, url]`.
    fn repo_with_remotes(remotes: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir_all(&git).unwrap();
        let mut config = String::from("[core]\n\trepositoryformatversion = 0\n");
        for (name, url) in remotes {
            config.push_str(&format!("[remote \"{name}\"]\n\turl = {url}\n"));
        }
        fs::write(git.join("config"), config).unwrap();
        dir
    }

    #[test]
    fn derives_owner_and_repo_from_origin() {
        let dir = repo_with_remotes(&[("origin", "git@github.com:acme/widget.git")]);
        assert_eq!(derive_namespace(dir.path()).as_deref(), Some("acme/widget"));
    }

    #[test]
    fn host_is_dropped_so_clone_protocol_does_not_matter() {
        // Every clone form of the same repo must derive the same namespace,
        // or re-cloning over https would strand the index under a new name.
        for url in [
            "git@github.com:acme/widget.git",
            "https://github.com/acme/widget.git",
            "ssh://git@github.com:22/acme/widget",
            "git://github.com/acme/widget.git",
        ] {
            let dir = repo_with_remotes(&[("origin", url)]);
            assert_eq!(
                derive_namespace(dir.path()).as_deref(),
                Some("acme/widget"),
                "{url}"
            );
        }
    }

    #[test]
    fn derives_from_a_non_origin_remote_when_origin_is_absent() {
        let dir = repo_with_remotes(&[("upstream", "git@gitlab.com:team/svc.git")]);
        assert_eq!(derive_namespace(dir.path()).as_deref(), Some("team/svc"));
    }

    #[test]
    fn same_repo_name_under_different_owners_does_not_collide() {
        // The reason the owner is kept rather than using the bare repo name.
        let a = repo_with_remotes(&[("origin", "git@github.com:one/api.git")]);
        let b = repo_with_remotes(&[("origin", "git@github.com:two/api.git")]);
        assert_ne!(derive_namespace(a.path()), derive_namespace(b.path()));
    }

    #[test]
    fn falls_back_to_the_directory_basename_without_a_git_remote() {
        let parent = tempdir().unwrap();
        let repo = parent.path().join("standalone-project");
        fs::create_dir_all(&repo).unwrap();
        assert_eq!(
            derive_namespace(&repo).as_deref(),
            Some("standalone-project")
        );
    }

    #[test]
    fn falls_back_to_the_basename_when_a_repo_has_no_remotes() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
        let expected = dir.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(derive_namespace(dir.path()).as_deref(), Some(expected));
    }

    #[test]
    fn derived_names_pass_namespace_validation() {
        // A `/` is legal: the namespace is stored as data and backed by a
        // generated `ns_<uuid>` schema token, never used as an identifier.
        let dir = repo_with_remotes(&[("origin", "git@github.com:acme/widget.git")]);
        let derived = derive_namespace(dir.path()).unwrap();
        assert!(crate::cli::validate_namespace(&derived).is_ok());
    }
}
