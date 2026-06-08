//! Zed integration.
//!
//! Zed has no payload-bearing pre-tool hook, so we use its two always-on
//! mechanisms instead:
//!
//! * a marker-bracketed block in the rules file (project `.rules`, or the global
//!   `~/.config/zed/AGENTS.md`) telling the agent to prefer codesearch, and
//! * an MCP `context_servers` entry registering `codesearch mcp` so the agent
//!   has structured access to the index.

use anyhow::Result;
use serde_json::{json, Value};

use super::{display_path, load_json_object, write_json, Scope};

const RULES_START: &str = "<!-- codesearch:start -->";
const RULES_END: &str = "<!-- codesearch:end -->";

fn rules_block() -> String {
    format!(
        "{RULES_START}\n\
## codesearch\n\
This repository is indexed by codesearch. For codebase questions, prefer:\n\
- `codesearch search \"<question>\"` — semantic + keyword (BM25) hybrid search\n\
- `codesearch context <symbol>` — callers and callees of a symbol\n\
- `codesearch impact <symbol>` — blast radius of changing a symbol\n\n\
Use these over grepping or reading files one by one. Read raw files to edit or\n\
debug specific code, or when the index lacks the detail you need.\n\
{RULES_END}"
    )
}

fn rules_path(scope: Scope) -> Result<std::path::PathBuf> {
    Ok(match scope {
        Scope::Global => super::home_dir()?
            .join(".config")
            .join("zed")
            .join("AGENTS.md"),
        Scope::Project => std::env::current_dir()?.join(".rules"),
    })
}

fn settings_path(scope: Scope) -> Result<std::path::PathBuf> {
    Ok(match scope {
        Scope::Global => super::home_dir()?
            .join(".config")
            .join("zed")
            .join("settings.json"),
        Scope::Project => std::env::current_dir()?.join(".zed").join("settings.json"),
    })
}

/// Insert or replace the codesearch block in an existing rules file's text.
fn upsert_block(existing: &str, block: &str) -> String {
    if let (Some(start), Some(end)) = (existing.find(RULES_START), existing.find(RULES_END)) {
        let end = end + RULES_END.len();
        let mut out = String::with_capacity(existing.len());
        out.push_str(&existing[..start]);
        out.push_str(block);
        out.push_str(&existing[end..]);
        out
    } else {
        let mut out = existing.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(block);
        out.push('\n');
        out
    }
}

/// Remove the codesearch block (and surrounding blank lines) from rules text.
fn remove_block(existing: &str) -> Option<String> {
    let start = existing.find(RULES_START)?;
    let end = existing.find(RULES_END)? + RULES_END.len();
    let mut out = String::with_capacity(existing.len());
    out.push_str(existing[..start].trim_end());
    let tail = existing[end..].trim_start();
    if !out.is_empty() && !tail.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(tail);
    Some(out)
}

pub fn install(scope: Scope) -> Result<Vec<String>> {
    let mut lines = Vec::new();

    // 1. Rules block.
    let rules = rules_path(scope)?;
    let existing = std::fs::read_to_string(&rules).unwrap_or_default();
    let updated = upsert_block(&existing, &rules_block());
    if let Some(parent) = rules.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&rules, updated)?;
    lines.push(format!(
        "wrote {} (codesearch guidance block)",
        display_path(&rules)
    ));

    // 2. MCP context server registration.
    let settings = settings_path(scope)?;
    let mut obj = load_json_object(&settings)?;
    let servers = obj
        .entry("context_servers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "`context_servers` in {} is not an object",
                settings.display()
            )
        })?;
    servers.insert(
        "codesearch".into(),
        json!({ "command": "codesearch", "args": ["mcp"], "env": {} }),
    );
    write_json(&settings, &Value::Object(obj))?;
    lines.push(format!(
        "wrote {} (context_servers.codesearch -> `codesearch mcp`)",
        display_path(&settings)
    ));

    Ok(lines)
}

pub fn uninstall(scope: Scope) -> Result<Vec<String>> {
    let mut lines = Vec::new();

    let rules = rules_path(scope)?;
    if let Ok(existing) = std::fs::read_to_string(&rules) {
        if let Some(updated) = remove_block(&existing) {
            if updated.trim().is_empty() {
                std::fs::remove_file(&rules)?;
                lines.push(format!("removed {}", display_path(&rules)));
            } else {
                std::fs::write(&rules, updated)?;
                lines.push(format!(
                    "removed codesearch block from {}",
                    display_path(&rules)
                ));
            }
        } else {
            lines.push(format!("no codesearch block in {}", display_path(&rules)));
        }
    } else {
        lines.push(format!(
            "no rules file at {} (nothing to remove)",
            display_path(&rules)
        ));
    }

    let settings = settings_path(scope)?;
    if settings.exists() {
        let mut obj = load_json_object(&settings)?;
        let mut removed = false;
        if let Some(servers) = obj
            .get_mut("context_servers")
            .and_then(Value::as_object_mut)
        {
            removed = servers.remove("codesearch").is_some();
            if servers.is_empty() {
                obj.remove("context_servers");
            }
        }
        write_json(&settings, &Value::Object(obj))?;
        lines.push(if removed {
            format!(
                "removed context_servers.codesearch from {}",
                display_path(&settings)
            )
        } else {
            format!("no codesearch server in {}", display_path(&settings))
        });
    }

    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_remove_preserves_user_text() {
        let user = "# My rules\n\nBe concise.\n";
        let with_block = upsert_block(user, &rules_block());
        assert!(with_block.contains("# My rules"));
        assert!(with_block.contains("codesearch search"));

        // Re-upsert must not duplicate.
        let again = upsert_block(&with_block, &rules_block());
        assert_eq!(again.matches(RULES_START).count(), 1);

        let removed = remove_block(&again).unwrap();
        assert!(removed.contains("# My rules"));
        assert!(!removed.contains("codesearch search"));
    }

    #[test]
    fn remove_block_none_when_absent() {
        assert!(remove_block("nothing here").is_none());
    }
}
