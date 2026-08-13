use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;

use crate::cli::OutputFormat;
use crate::{CallGraphNode, SymbolContext};

use super::super::Container;

pub struct SymbolContextController<'a> {
    container: &'a Container,
}

impl<'a> SymbolContextController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn context(
        &self,
        symbol: String,
        repository: Option<String>,
        format: OutputFormat,
        is_regex: bool,
    ) -> Result<String> {
        let use_case = self.container.context_use_case();
        let ctx = use_case
            .get_context(&symbol, repository.as_deref(), is_regex)
            .await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&ctx)?,
            OutputFormat::Vimgrep => Self::format_vimgrep(&ctx),
            OutputFormat::Text => Self::format_text(&ctx),
        })
    }

    fn format_vimgrep(ctx: &SymbolContext) -> String {
        let callers = ctx.callers_by_depth.iter().flatten().map(|n| {
            format!(
                "{}:{}:1:← {} [{}]",
                n.file_path, n.line, n.symbol, n.reference_kind
            )
        });
        let callees = ctx.callees_by_depth.iter().flatten().map(|n| {
            format!(
                "{}:{}:1:→ {} [{}]",
                n.file_path, n.line, n.symbol, n.reference_kind
            )
        });
        callers.chain(callees).collect::<Vec<_>>().join("\n")
    }

    fn format_text(ctx: &SymbolContext) -> String {
        let mut out = format!(
            "Context for '{}'\n\
             ─────────────────────────────────────────\n",
            ctx.symbol
        );

        let has_callers = ctx.total_callers > 0;
        let has_callees = ctx.total_callees > 0;

        // Build callee children map once, reused per caller chain.
        let callee_children = ctx.callee_children();

        if !has_callers && !has_callees {
            out.push_str("No callers or callees found for this symbol.\n");
            return out;
        }

        if has_callers {
            // One chain per entry point, each traced back down to the queried
            // symbol: path[0] = top-most caller, path[last] = direct caller.
            let chains = ctx.caller_chains();

            for (idx, path) in chains.iter().enumerate() {
                Self::render_chain(path, &ctx.symbol, &callee_children, &mut out);
                if idx < chains.len() - 1 {
                    out.push('\n');
                }
            }
        } else {
            // No callers: render callees subtree rooted at the symbol directly.
            out.push_str(&format!("{}\n", ctx.symbol));
            let mut visited = HashSet::new();
            Self::render_callees_subtree(&ctx.symbol, &callee_children, "", &mut out, &mut visited);
        }

        out
    }

    /// Render one caller chain (top-most entry → direct caller) then the queried symbol
    /// with its callees subtree hanging off it.
    fn render_chain(
        path: &[&CallGraphNode],
        root_symbol: &str,
        callee_children: &HashMap<String, Vec<&CallGraphNode>>,
        out: &mut String,
    ) {
        if path.is_empty() {
            return;
        }
        // path[0] is the leaf (top-most caller), rendered at indent 0.
        for (depth, node) in path.iter().enumerate() {
            let alias = node
                .import_alias
                .as_ref()
                .map(|a| format!(", as {}", a))
                .unwrap_or_default();
            if depth == 0 {
                out.push_str(&format!(
                    "{} [{}{}]  {}:{}\n",
                    node.symbol, node.reference_kind, alias, node.file_path, node.line,
                ));
            } else {
                let indent = "    ".repeat(depth - 1);
                out.push_str(&format!(
                    "{}└── {} [{}{}]  {}:{}\n",
                    indent, node.symbol, node.reference_kind, alias, node.file_path, node.line,
                ));
            }
        }
        // Queried symbol is the terminal node of the caller chain.
        let caller_indent = "    ".repeat(path.len() - 1);
        out.push_str(&format!("{}└── {}\n", caller_indent, root_symbol));

        // Hang callees subtree off the queried symbol.
        let callee_prefix = "    ".repeat(path.len());
        let mut visited = HashSet::new();
        Self::render_callees_subtree(
            root_symbol,
            callee_children,
            &callee_prefix,
            out,
            &mut visited,
        );
    }

    /// Recursively render the callees subtree rooted at `parent_symbol`.
    fn render_callees_subtree(
        parent_symbol: &str,
        callee_children: &HashMap<String, Vec<&CallGraphNode>>,
        prefix: &str,
        out: &mut String,
        visited: &mut HashSet<String>,
    ) {
        let children = match callee_children.get(parent_symbol) {
            Some(c) => c,
            None => return,
        };
        let count = children.len();
        for (i, node) in children.iter().enumerate() {
            if !visited.insert(node.symbol.clone()) {
                continue; // cycle guard
            }
            let alias = node
                .import_alias
                .as_ref()
                .map(|a| format!(", as {}", a))
                .unwrap_or_default();
            let is_last = i == count - 1;
            let branch = if is_last { "└──" } else { "├──" };
            out.push_str(&format!(
                "{}{} {} [{}{}]  {}:{}\n",
                prefix, branch, node.symbol, node.reference_kind, alias, node.file_path, node.line,
            ));
            // Continuation prefix for this node's children:
            // - non-last: "│   " keeps the vertical bar connected
            // - last:     "    " (no bar)
            let child_prefix = if is_last {
                format!("{}    ", prefix)
            } else {
                format!("{}│   ", prefix)
            };
            Self::render_callees_subtree(
                &node.symbol,
                callee_children,
                &child_prefix,
                out,
                visited,
            );
        }
    }
}
