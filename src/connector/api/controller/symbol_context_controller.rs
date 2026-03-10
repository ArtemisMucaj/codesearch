use anyhow::Result;

use crate::cli::OutputFormat;
use crate::SymbolContext;

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
            OutputFormat::Vimgrep => Self::format_context_vimgrep(&ctx),
            OutputFormat::Text => self.format_context(&ctx),
        })
    }

    fn format_context_vimgrep(ctx: &SymbolContext) -> String {
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

    fn format_context(&self, ctx: &SymbolContext) -> String {
        let mut out = format!(
            "Context for '{}'\n\
             ─────────────────────────────────────────\n",
            ctx.symbol
        );

        out.push_str(&format!(
            "Callers ({} total) — who uses this symbol:\n",
            ctx.total_callers
        ));
        let all_callers: Vec<_> = ctx.callers_by_depth.iter().flatten().collect();
        if all_callers.is_empty() {
            out.push_str("  (none found)\n");
        } else {
            for node in &all_callers {
                let alias_suffix = node
                    .import_alias
                    .as_ref()
                    .map(|a| format!(", as {}", a))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  ← {} [{}{}]  {}:{}\n",
                    node.symbol, node.reference_kind, alias_suffix, node.file_path, node.line
                ));
            }
        }

        out.push_str(&format!(
            "\nCallees ({} total) — what this symbol uses:\n",
            ctx.total_callees
        ));
        let all_callees: Vec<_> = ctx.callees_by_depth.iter().flatten().collect();
        if all_callees.is_empty() {
            out.push_str("  (none found)\n");
        } else {
            for node in &all_callees {
                let alias_suffix = node
                    .import_alias
                    .as_ref()
                    .map(|a| format!(", as {}", a))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  → {} [{}{}]  {}:{}\n",
                    node.symbol, node.reference_kind, alias_suffix, node.file_path, node.line
                ));
            }
        }

        out
    }
}
