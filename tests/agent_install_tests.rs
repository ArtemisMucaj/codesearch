//! End-to-end tests for the `install` / `uninstall` / `hooks` / `pre-tool-call`
//! commands. These drive the real binary in a child process with a scratch
//! working directory, so they exercise the actual CLI wiring without mutating
//! the test process's environment.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_codesearch")
}

/// Run codesearch in `dir` and return (stdout, success).
fn run(dir: &Path, args: &[&str]) -> (String, bool) {
    let out = Command::new(bin())
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn codesearch");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

#[test]
fn install_all_project_writes_every_platform() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let (out, ok) = run(dir, &["install", "all", "--project"]);
    assert!(ok, "install failed: {out}");

    // Claude: PreToolUse hooks in .claude/settings.json
    let claude = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    assert!(claude.contains("PreToolUse"));
    assert!(claude.contains("codesearch pre-tool-call"));

    // OpenCode: auto-loaded plugin
    assert!(dir.join(".opencode/plugins/codesearch.js").exists());

    // Pi: live extension
    assert!(dir.join(".pi/extensions/codesearch.ts").exists());

    // Zed: rules block + MCP registration
    let rules = std::fs::read_to_string(dir.join(".rules")).unwrap();
    assert!(rules.contains("codesearch search"));
    let zed = std::fs::read_to_string(dir.join(".zed/settings.json")).unwrap();
    assert!(zed.contains("context_servers"));
    assert!(zed.contains("codesearch"));

    // Uninstall removes them again.
    let (out, ok) = run(dir, &["uninstall", "all", "--project"]);
    assert!(ok, "uninstall failed: {out}");
    assert!(!dir.join(".opencode/plugins/codesearch.js").exists());
    assert!(!dir.join(".pi/extensions/codesearch.ts").exists());
    let claude = std::fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
    assert!(!claude.contains("codesearch pre-tool-call"));
}

#[test]
fn pre_tool_call_nudges_only_when_indexed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let payload = r#"{"tool_name":"Grep","tool_input":{"pattern":"auth"}}"#;

    // No marker yet → no output.
    let out = pipe_pre_tool_call(dir, payload);
    assert!(
        out.trim().is_empty(),
        "expected no nudge without marker, got: {out}"
    );

    // Create the marker → nudge fires.
    std::fs::create_dir_all(dir.join(".codesearch")).unwrap();
    std::fs::write(
        dir.join(".codesearch/project.json"),
        r#"{"repository_id":"r","name":"n","indexed_at":0}"#,
    )
    .unwrap();

    let out = pipe_pre_tool_call(dir, payload);
    assert!(
        out.contains("hookSpecificOutput"),
        "expected nudge, got: {out}"
    );
    assert!(out.contains("codesearch search"));
}

fn pipe_pre_tool_call(dir: &Path, payload: &str) -> String {
    let mut child = Command::new(bin())
        .arg("pre-tool-call")
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pre-tool-call");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn hooks_install_requires_git_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Not a git repo → command fails cleanly.
    let (_out, ok) = run(dir, &["hooks", "install"]);
    assert!(!ok, "hooks install should fail outside a git repo");

    // Initialise a repo, then it succeeds and writes hooks.
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());

    let (out, ok) = run(dir, &["hooks", "install"]);
    assert!(ok, "hooks install failed: {out}");
    assert!(dir.join(".git/hooks/post-commit").exists());
    assert!(dir.join(".git/hooks/post-checkout").exists());

    let (status, ok) = run(dir, &["hooks", "status"]);
    assert!(ok);
    assert!(status.contains("installed"));
}
