//! Git hooks that keep the index fresh.
//!
//! `post-commit` and `post-checkout` launch a background, incremental
//! `codesearch index .` so the data the agent hooks point at never goes stale.
//! Re-indexing runs detached (the commit returns immediately), is skipped during
//! rebase/merge/cherry-pick, and is a no-op when only the marker changed.
//!
//! These operate on the git repository containing the current directory; there
//! is no global variant.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::HooksSubcommand;

const COMMIT_HOOK: &str = "post-commit";
const CHECKOUT_HOOK: &str = "post-checkout";
const COMMIT_MARKER: &str = "# codesearch-hook-start";
const COMMIT_MARKER_END: &str = "# codesearch-hook-end";
const CHECKOUT_MARKER: &str = "# codesearch-checkout-hook-start";
const CHECKOUT_MARKER_END: &str = "# codesearch-checkout-hook-end";

const COMMIT_SCRIPT: &str = r#"# codesearch-hook-start
# Re-index the repository in the background after each commit (incremental).
# Installed by: codesearch hooks install
[ "${CODESEARCH_SKIP_HOOK:-0}" = "1" ] && exit 0
GIT_DIR=$(git rev-parse --git-dir 2>/dev/null)
[ -d "$GIT_DIR/rebase-merge" ] && exit 0
[ -d "$GIT_DIR/rebase-apply" ] && exit 0
[ -f "$GIT_DIR/MERGE_HEAD" ] && exit 0
[ -f "$GIT_DIR/CHERRY_PICK_HEAD" ] && exit 0
command -v codesearch >/dev/null 2>&1 || exit 0
CHANGED=$(git diff --name-only HEAD~1 HEAD 2>/dev/null || git diff --name-only HEAD 2>/dev/null)
NON_MARKER=$(echo "$CHANGED" | grep -v '^.codesearch/' || true)
[ -z "$NON_MARKER" ] && exit 0
LOG="${HOME}/.cache/codesearch-reindex.log"
mkdir -p "$(dirname "$LOG")"
echo "[codesearch hook] launching background re-index (log: $LOG)"
nohup codesearch index . >> "$LOG" 2>&1 < /dev/null &
disown 2>/dev/null || true
# codesearch-hook-end
"#;

const CHECKOUT_SCRIPT: &str = r#"# codesearch-checkout-hook-start
# Re-index the repository in the background after switching branches.
# Installed by: codesearch hooks install
BRANCH_SWITCH=$3
[ "$BRANCH_SWITCH" != "1" ] && exit 0
[ "${CODESEARCH_SKIP_HOOK:-0}" = "1" ] && exit 0
[ -f ".codesearch/project.json" ] || exit 0
command -v codesearch >/dev/null 2>&1 || exit 0
LOG="${HOME}/.cache/codesearch-reindex.log"
mkdir -p "$(dirname "$LOG")"
echo "[codesearch hook] branch switch - launching background re-index (log: $LOG)"
nohup codesearch index . >> "$LOG" 2>&1 < /dev/null &
disown 2>/dev/null || true
# codesearch-checkout-hook-end
"#;

/// Resolve the git hooks directory for the repository containing the cwd.
fn hooks_dir() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .context("running `git rev-parse` (is git installed?)")?;
    if !output.status.success() {
        anyhow::bail!("not inside a git repository");
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() || raw.contains('\n') {
        anyhow::bail!("could not determine git hooks directory");
    }
    let path = PathBuf::from(&raw);
    let resolved = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    std::fs::create_dir_all(&resolved)
        .with_context(|| format!("creating {}", resolved.display()))?;
    Ok(resolved)
}

/// Install one hook, appending to any existing user hook of the same name.
fn install_one(dir: &Path, name: &str, script: &str, marker: &str) -> Result<String> {
    let path = dir.join(name);
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading hook {}", path.display()))?;
        if content.contains(marker) {
            return Ok(format!("{name}: already installed"));
        }
        let merged = format!("{}\n\n{}", content.trim_end(), script);
        std::fs::write(&path, merged)
            .with_context(|| format!("writing hook {}", path.display()))?;
        // An existing hook may not be executable (e.g. created by another tool);
        // ensure git can run the section we just appended.
        set_executable(&path)?;
        Ok(format!("{name}: appended to existing hook"))
    } else {
        std::fs::write(&path, format!("#!/bin/sh\n{script}"))
            .with_context(|| format!("writing hook {}", path.display()))?;
        set_executable(&path)?;
        Ok(format!("{name}: installed"))
    }
}

/// Remove our marked section from one hook, deleting the file if nothing else
/// remains.
fn uninstall_one(dir: &Path, name: &str, marker: &str, marker_end: &str) -> Result<String> {
    let path = dir.join(name);
    if !path.exists() {
        return Ok(format!("{name}: not present"));
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading hook {}", path.display()))?;
    let (Some(start), Some(end)) = (content.find(marker), content.find(marker_end)) else {
        return Ok(format!("{name}: no codesearch section"));
    };
    let end = end + marker_end.len();
    let mut remaining = String::new();
    remaining.push_str(content[..start].trim_end());
    let tail = content[end..].trim_start();
    if !remaining.is_empty() && !tail.is_empty() {
        remaining.push_str("\n\n");
    }
    remaining.push_str(tail);
    let trimmed = remaining.trim();
    if trimmed.is_empty() || trimmed == "#!/bin/sh" || trimmed == "#!/bin/bash" {
        std::fs::remove_file(&path).with_context(|| format!("removing hook {}", path.display()))?;
        Ok(format!("{name}: removed"))
    } else {
        std::fs::write(&path, format!("{}\n", remaining.trim_end()))
            .with_context(|| format!("writing hook {}", path.display()))?;
        Ok(format!(
            "{name}: codesearch section removed (other content kept)"
        ))
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("reading permissions of {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting permissions on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn status_one(dir: &Path, name: &str, marker: &str) -> String {
    let path = dir.join(name);
    let state = match std::fs::read_to_string(&path) {
        Ok(c) if c.contains(marker) => "installed",
        Ok(_) => "not installed (hook exists, codesearch section absent)",
        Err(_) => "not installed",
    };
    format!("{name}: {state}")
}

/// Route the `hooks` subcommand.
pub fn dispatch(subcommand: &HooksSubcommand) -> Result<String> {
    let dir = hooks_dir()?;
    let lines = match subcommand {
        HooksSubcommand::Install => vec![
            install_one(&dir, COMMIT_HOOK, COMMIT_SCRIPT, COMMIT_MARKER)?,
            install_one(&dir, CHECKOUT_HOOK, CHECKOUT_SCRIPT, CHECKOUT_MARKER)?,
        ],
        HooksSubcommand::Uninstall => vec![
            uninstall_one(&dir, COMMIT_HOOK, COMMIT_MARKER, COMMIT_MARKER_END)?,
            uninstall_one(&dir, CHECKOUT_HOOK, CHECKOUT_MARKER, CHECKOUT_MARKER_END)?,
        ],
        HooksSubcommand::Status => vec![
            status_one(&dir, COMMIT_HOOK, COMMIT_MARKER),
            status_one(&dir, CHECKOUT_HOOK, CHECKOUT_MARKER),
        ],
    };
    Ok(format!(
        "git hooks ({}):\n  {}",
        dir.display(),
        lines.join("\n  ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_append_and_remove_keeps_user_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let path = dir.join(COMMIT_HOOK);
        std::fs::write(&path, "#!/bin/sh\necho user hook\n").unwrap();

        let msg = install_one(dir, COMMIT_HOOK, COMMIT_SCRIPT, COMMIT_MARKER).unwrap();
        assert!(msg.contains("appended"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("echo user hook"));
        assert!(content.contains(COMMIT_MARKER));

        // Idempotent.
        let again = install_one(dir, COMMIT_HOOK, COMMIT_SCRIPT, COMMIT_MARKER).unwrap();
        assert!(again.contains("already installed"));

        uninstall_one(dir, COMMIT_HOOK, COMMIT_MARKER, COMMIT_MARKER_END).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("echo user hook"));
        assert!(!content.contains(COMMIT_MARKER));
    }

    #[test]
    fn install_then_remove_deletes_solo_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        install_one(dir, CHECKOUT_HOOK, CHECKOUT_SCRIPT, CHECKOUT_MARKER).unwrap();
        assert!(dir.join(CHECKOUT_HOOK).exists());
        uninstall_one(dir, CHECKOUT_HOOK, CHECKOUT_MARKER, CHECKOUT_MARKER_END).unwrap();
        assert!(!dir.join(CHECKOUT_HOOK).exists());
    }
}
