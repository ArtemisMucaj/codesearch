//! The `codesearch pre-tool-call` command.
//!
//! Installed agent hooks pipe their pre-tool-call payload (the tool name and its
//! arguments, as JSON) into `codesearch pre-tool-call` on stdin. When the current
//! project is indexed and the agent is about to grep or read source files to
//! answer a question, we print a Claude-Code-style `hookSpecificOutput` blob
//! that steers it toward `codesearch search`/`context`/`impact` instead.
//!
//! Everything here *fails open*: any parse error, missing field, or absent
//! marker results in no output at all, so a legitimate tool call always
//! proceeds untouched. The hook only ever *adds* context; it never blocks.

use std::path::Path;

use serde_json::Value;

use super::marker;

/// Search-oriented executables that, when invoked through a shell, indicate the
/// agent is hunting for code the index could surface faster.
const SEARCH_COMMANDS: &[&str] = &["grep", "rg", "ripgrep", "ag", "ack", "find", "fd"];

/// File extensions we consider "source or docs" — reading one of these to
/// *understand* the codebase is what the index is for.
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "go", "java", "rb", "c", "h", "cpp", "hpp", "cc",
    "cs", "kt", "swift", "php", "scala", "lua", "sh", "md", "mdx", "rst", "txt",
];

/// Nudge text for search-style actions (shell grep/find, or the Grep/Glob tools).
const SEARCH_NUDGE: &str = "codesearch: this repository is indexed. For intent or concept questions, prefer `codesearch search \"<what you're looking for>\"` — it fuses semantic (embedding) and keyword (BM25) matching and is usually faster and more relevant than grepping raw files. Use plain grep only for exact-string matches when you already know the identifier.";

/// Nudge text for reading a source/doc file to understand the codebase.
const READ_NUDGE: &str = "codesearch: this repository is indexed. To understand code by meaning, find callers/callees, or gauge the blast radius of a change, prefer `codesearch search \"<question>\"`, `codesearch context <symbol>`, or `codesearch impact <symbol>` over reading files one by one. Read raw files when editing or debugging specific code, or when the index lacks the detail you need.";

/// Decide whether the given `PreToolUse` payload warrants a nudge.
///
/// `marker_present` is whether `find_marker` located an indexed project for the
/// working directory. Returns the `additionalContext` string to emit, or `None`
/// when no nudge should fire.
pub fn evaluate(payload: &Value, marker_present: bool) -> Option<String> {
    // Never nudge in a project that has not been indexed.
    if !marker_present {
        return None;
    }

    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    // Claude nests arguments under "tool_input"; other callers may pass them at
    // the root. Accept either.
    let input = payload.get("tool_input").unwrap_or(payload);

    match tool {
        "Bash" => {
            let command = input.get("command").and_then(Value::as_str).unwrap_or("");
            if command_is_search(command) {
                Some(SEARCH_NUDGE.to_string())
            } else {
                None
            }
        }
        // The dedicated search tools are themselves the signal.
        "Grep" | "Glob" => Some(SEARCH_NUDGE.to_string()),
        "Read" => {
            let path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
            if path_is_source(path) {
                Some(READ_NUDGE.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// True when a shell command line is (or contains) a code-search invocation that
/// the index could answer, and is not itself a codesearch call.
fn command_is_search(command: &str) -> bool {
    if command.contains("codesearch") {
        return false;
    }
    // Tokenize on shell word boundaries so "grep" matches but "pgrep" /
    // "foofind" do not.
    command
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .any(|word| SEARCH_COMMANDS.contains(&word))
}

/// True when a path points at a source or documentation file outside the
/// marker directory (reading the marker itself must never trigger a loop).
fn path_is_source(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    if normalized.contains(&format!("/{}/", marker::MARKER_DIR))
        || normalized.starts_with(&format!("{}/", marker::MARKER_DIR))
    {
        return false;
    }
    Path::new(&normalized)
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| SOURCE_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Wrap `additional_context` in the Claude Code `PreToolUse` hook output shape.
pub fn render_output(additional_context: &str) -> String {
    let blob = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": additional_context,
        }
    });
    blob.to_string()
}

/// Entry point for `codesearch pre-tool-call`: read stdin, evaluate, print.
///
/// Always returns successfully — the hook must never fail the tool call.
pub fn run() {
    use std::io::Read;

    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return;
    }
    let payload: Value = match serde_json::from_str(&buf) {
        Ok(v) => v,
        Err(_) => return,
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let marker_present = marker::find_marker(&cwd).is_some();

    if let Some(context) = evaluate(&payload, marker_present) {
        println!("{}", render_output(&context));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_nudge_without_marker() {
        let payload = json!({"tool_name": "Grep", "tool_input": {"pattern": "foo"}});
        assert!(evaluate(&payload, false).is_none());
    }

    #[test]
    fn grep_and_glob_tools_nudge() {
        let grep = json!({"tool_name": "Grep", "tool_input": {"pattern": "foo"}});
        let glob = json!({"tool_name": "Glob", "tool_input": {"pattern": "**/*.rs"}});
        assert_eq!(evaluate(&grep, true).as_deref(), Some(SEARCH_NUDGE));
        assert_eq!(evaluate(&glob, true).as_deref(), Some(SEARCH_NUDGE));
    }

    #[test]
    fn bash_grep_nudges_but_plain_bash_does_not() {
        let grep = json!({"tool_name": "Bash", "tool_input": {"command": "grep -r auth src/"}});
        let ls = json!({"tool_name": "Bash", "tool_input": {"command": "ls -la"}});
        assert_eq!(evaluate(&grep, true).as_deref(), Some(SEARCH_NUDGE));
        assert!(evaluate(&ls, true).is_none());
    }

    #[test]
    fn bash_codesearch_call_does_not_nudge() {
        // Avoid recommending codesearch when the agent already ran it (even if
        // the command happens to mention grep elsewhere).
        let cmd = json!({"tool_name": "Bash", "tool_input": {"command": "codesearch search 'grep usage'"}});
        assert!(evaluate(&cmd, true).is_none());
    }

    #[test]
    fn word_boundary_avoids_false_positives() {
        let pgrep = json!({"tool_name": "Bash", "tool_input": {"command": "pgrep node"}});
        assert!(evaluate(&pgrep, true).is_none());
    }

    #[test]
    fn read_source_file_nudges() {
        let rs = json!({"tool_name": "Read", "tool_input": {"file_path": "src/main.rs"}});
        assert_eq!(evaluate(&rs, true).as_deref(), Some(READ_NUDGE));
    }

    #[test]
    fn read_non_source_or_marker_does_not_nudge() {
        let png = json!({"tool_name": "Read", "tool_input": {"file_path": "logo.png"}});
        let marker =
            json!({"tool_name": "Read", "tool_input": {"file_path": ".codesearch/project.json"}});
        assert!(evaluate(&png, true).is_none());
        assert!(evaluate(&marker, true).is_none());
    }

    #[test]
    fn root_level_arguments_are_accepted() {
        // Some callers pass tool args at the root rather than under tool_input.
        let payload = json!({"tool_name": "Read", "file_path": "src/lib.rs"});
        assert_eq!(evaluate(&payload, true).as_deref(), Some(READ_NUDGE));
    }

    #[test]
    fn unknown_tool_is_ignored() {
        let payload = json!({"tool_name": "Write", "tool_input": {"file_path": "src/x.rs"}});
        assert!(evaluate(&payload, true).is_none());
    }

    #[test]
    fn render_output_is_valid_pretooluse_json() {
        let s = render_output("hello");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "hello");
    }
}
