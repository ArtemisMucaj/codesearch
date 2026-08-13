use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::connector::api::container::Container;

/// Extract a human-readable short name from a SCIP/call-graph symbol string.
///
/// SCIP symbols look like: `rust-analyzer cargo pkg 0.1.0 file/Module#method().`
/// We want just `method` (or `Module::method` if there's an enclosing type).
fn short_symbol_name(symbol: &str) -> &str {
    // Strip common SCIP method-descriptor suffix `().`
    let s = symbol.trim_end_matches("().");
    // Take the portion after the last `#`, `::`, `.`, `/`, or `\`
    s.rfind(['#', ':', '.', '/', '\\'])
        .map(|i| &s[i + 1..])
        .filter(|part| !part.is_empty())
        .unwrap_or(s)
}

pub struct UsesController<'a> {
    container: &'a Container,
}

impl<'a> UsesController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn uses(&self, from: String, to: String) -> Result<String> {
        // Resolution, graph construction and edge filtering (including the sort
        // by target-then-source this rendering relies on) live in the use case,
        // shared with the MCP tool and the management endpoint.
        let uses = self
            .container
            .file_graph_use_case()
            .uses_between(&from, &to)
            .await
            .context("Failed to resolve repository dependencies")?;

        let (from_name, to_name) = (&uses.from_name, &uses.to_name);
        let edges = &uses.edges;

        if edges.is_empty() {
            return Ok(format!(
                "No dependencies found: '{from_name}' does not use any files from '{to_name}'."
            ));
        }

        // Group by target file
        let mut out = format!(
            "Files in '{}' that use files from '{}':\n\n",
            from_name, to_name
        );

        let mut current_target = "";
        let mut unique_sources: HashSet<&str> = HashSet::new();
        let mut unique_targets: HashSet<&str> = HashSet::new();
        for e in edges {
            unique_sources.insert(&e.from_file);
            if e.to_file != current_target {
                current_target = &e.to_file;
                unique_targets.insert(&e.to_file);
                out.push_str(&format!("  {}\n", e.to_file));
            }
            out.push_str(&format!("    ← {}", e.from_file));
            if !e.symbols.is_empty() {
                let names: Vec<&str> = e.symbols.iter().map(|s| short_symbol_name(s)).collect();
                out.push_str(&format!("  [{}]", names.join(", ")));
            }
            out.push('\n');
        }

        out.push_str(&format!(
            "\n{} file(s) in '{}' depend on {} file(s) in '{}'.",
            unique_sources.len(),
            from_name,
            unique_targets.len(),
            to_name
        ));

        Ok(out)
    }
}
