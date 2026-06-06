//! Claude Code integration: `PreToolUse` hooks in `settings.json`.
//!
//! Claude Code fires `PreToolUse` hooks before a tool runs and folds their
//! `additionalContext` output into the model's context at decision time. We
//! register two matchers — one for `Bash` (so shell greps are caught) and one
//! for the `Read`/`Grep`/`Glob` tools — both delegating to `codesearch
//! hook-check`, which decides whether to nudge. The hook only adds context and
//! never blocks.

use anyhow::Result;
use serde_json::{json, Value};

use super::{display_path, load_json_object, write_json, Scope, HOOK_COMMAND};

/// Matchers we register. Matching `Read|Grep|Glob` as well as `Bash` covers both
/// older Claude Code (dedicated Grep/Glob tools) and newer builds that route
/// searches through Bash.
const MATCHERS: &[&str] = &["Bash", "Read|Grep|Glob"];

fn settings_path(scope: Scope) -> Result<std::path::PathBuf> {
    Ok(super::scope_root(scope)?
        .join(".claude")
        .join("settings.json"))
}

/// Is this a PreToolUse entry that we installed (i.e. one of its hooks runs
/// `codesearch hook-check`)?
fn is_ours(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| c.contains(HOOK_COMMAND))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub fn install(scope: Scope) -> Result<Vec<String>> {
    let path = settings_path(scope)?;
    let mut settings = load_json_object(&path);

    // hooks → PreToolUse: keep every entry that is not ours, then append fresh.
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("`hooks` in {} is not an object", path.display()))?;

    let mut pre: Vec<Value> = hooks
        .get("PreToolUse")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|e| !is_ours(e)).cloned().collect())
        .unwrap_or_default();

    for matcher in MATCHERS {
        pre.push(json!({
            "matcher": matcher,
            "hooks": [{ "type": "command", "command": HOOK_COMMAND }],
        }));
    }

    hooks.insert("PreToolUse".into(), Value::Array(pre));
    write_json(&path, &Value::Object(settings))?;

    Ok(vec![format!(
        "wrote {} (PreToolUse hooks for Bash + Read/Grep/Glob)",
        display_path(&path)
    )])
}

pub fn uninstall(scope: Scope) -> Result<Vec<String>> {
    let path = settings_path(scope)?;
    if !path.exists() {
        return Ok(vec![format!(
            "no settings at {} (nothing to remove)",
            display_path(&path)
        )]);
    }
    let mut settings = load_json_object(&path);

    let removed = if let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) {
        if let Some(pre) = hooks.get("PreToolUse").and_then(Value::as_array) {
            let kept: Vec<Value> = pre.iter().filter(|e| !is_ours(e)).cloned().collect();
            let removed = pre.len() != kept.len();
            if kept.is_empty() {
                hooks.remove("PreToolUse");
            } else {
                hooks.insert("PreToolUse".into(), Value::Array(kept));
            }
            // Drop an empty hooks object so we leave the file tidy.
            if hooks.is_empty() {
                settings.remove("hooks");
            }
            removed
        } else {
            false
        }
    } else {
        false
    };

    write_json(&path, &Value::Object(settings))?;
    Ok(vec![if removed {
        format!("removed PreToolUse hooks from {}", display_path(&path))
    } else {
        format!("no codesearch hooks found in {}", display_path(&path))
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        // Tests run single-threaded for this module via a global lock to keep
        // the $HOME swap safe.
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", dir);
        let out = f();
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        out
    }

    #[test]
    fn install_is_idempotent_and_preserves_foreign_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let path = settings_path(Scope::Global).unwrap();
            // Seed a foreign hook the user already has.
            write_json(
                &path,
                &json!({"hooks": {"PreToolUse": [
                    {"matcher": "Write", "hooks": [{"type": "command", "command": "echo hi"}]}
                ]}}),
            )
            .unwrap();

            install(Scope::Global).unwrap();
            install(Scope::Global).unwrap(); // twice → no duplicates

            let settings = load_json_object(&path);
            let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
            // 1 foreign + 2 ours, no duplication on the second install.
            assert_eq!(pre.len(), 3);
            assert!(pre.iter().any(|e| e["matcher"] == "Write"));
            assert_eq!(pre.iter().filter(|e| is_ours(e)).count(), 2);

            // Uninstall removes only ours.
            uninstall(Scope::Global).unwrap();
            let settings = load_json_object(&path);
            let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
            assert_eq!(pre.len(), 1);
            assert_eq!(pre[0]["matcher"], "Write");
        });
    }
}
