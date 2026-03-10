use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;

use crate::{ContextNode, SymbolContext};
use crate::cli::OutputFormat;

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
        let callee_children = Self::build_callee_children_map(ctx);

        if !has_callers && !has_callees {
            out.push_str("No callers or callees found for this symbol.\n");
            return out;
        }

        if has_callers {
            let all_callers: Vec<&ContextNode> = ctx.callers_by_depth.iter().flatten().collect();

            // Build caller children map: via_symbol → [nodes that list it as via_symbol].
            let mut caller_children: HashMap<&str, Vec<&ContextNode>> = HashMap::new();
            for node in &all_callers {
                if let Some(via) = node.via_symbol.as_deref() {
                    caller_children.entry(via).or_default().push(node);
                }
            }

            // Leaf = top-most entry-point: no other node lists this symbol as its via_symbol.
            let leaf_nodes: Vec<&ContextNode> = all_callers
                .iter()
                .copied()
                .filter(|n| !caller_children.contains_key(n.symbol.as_str()))
                .collect();

            // Lookup by (depth, symbol) for unambiguous path tracing.
            let mut node_by_depth_sym: HashMap<(usize, &str), &ContextNode> = HashMap::new();
            for node in &all_callers {
                node_by_depth_sym
                    .entry((node.depth, node.symbol.as_str()))
                    .or_insert(node);
            }

            for (idx, &leaf) in leaf_nodes.iter().enumerate() {
                // Trace from leaf back toward the queried symbol via via_symbol links.
                let mut path: Vec<&ContextNode> = vec![leaf];
                let mut current = leaf;
                while let Some(via) = current.via_symbol.as_deref() {
                    let parent_depth = current.depth.saturating_sub(1);
                    if let Some(&parent) = node_by_depth_sym.get(&(parent_depth, via)) {
                        path.push(parent);
                        current = parent;
                    } else {
                        break;
                    }
                }
                // path[0] = leaf (top-most caller), path[last] = direct caller of queried symbol.
                Self::render_chain(&path, &ctx.symbol, &callee_children, &mut out);
                if idx < leaf_nodes.len() - 1 {
                    out.push('\n');
                }
            }
        } else {
            // No callers: render callees subtree rooted at the symbol directly.
            out.push_str(&format!("{}\n", ctx.symbol));
            let mut visited = HashSet::new();
            Self::render_callees_subtree(&ctx.symbol, &callee_children, 0, &mut out, &mut visited);
        }

        out
    }

    /// Build a map from parent_symbol → direct callee nodes (keyed by via_symbol).
    fn build_callee_children_map(ctx: &SymbolContext) -> HashMap<String, Vec<&ContextNode>> {
        let mut map: HashMap<String, Vec<&ContextNode>> = HashMap::new();
        for node in ctx.callees_by_depth.iter().flatten() {
            let key = node
                .via_symbol
                .clone()
                .unwrap_or_else(|| ctx.symbol.clone());
            map.entry(key).or_default().push(node);
        }
        map
    }

    /// Render one caller chain (top-most entry → direct caller) then the queried symbol
    /// with its callees subtree hanging off it.
    fn render_chain(
        path: &[&ContextNode],
        root_symbol: &str,
        callee_children: &HashMap<String, Vec<&ContextNode>>,
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
        let callee_base_depth = path.len(); // indent level for depth-1 callees
        let mut visited = HashSet::new();
        Self::render_callees_subtree(
            root_symbol,
            callee_children,
            callee_base_depth,
            out,
            &mut visited,
        );
    }

    /// Recursively render the callees subtree rooted at `parent_symbol`.
    fn render_callees_subtree(
        parent_symbol: &str,
        callee_children: &HashMap<String, Vec<&ContextNode>>,
        indent_depth: usize,
        out: &mut String,
        visited: &mut HashSet<String>,
    ) {
        let children: &Vec<&ContextNode> = match callee_children.get(parent_symbol) {
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
            let indent = "    ".repeat(indent_depth);
            out.push_str(&format!(
                "{}{} {} [{}{}]  {}:{}\n",
                indent, branch, node.symbol, node.reference_kind, alias, node.file_path, node.line,
            ));
            // Recurse into this node's children.
            Self::render_callees_subtree(
                &node.symbol,
                callee_children,
                indent_depth + 1,
                out,
                visited,
            );
        }
    }
}
