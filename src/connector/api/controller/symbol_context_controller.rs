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
        limit: Option<u32>,
        format: OutputFormat,
    ) -> Result<String> {
        let use_case = self.container.context_use_case();
        let ctx = use_case
            .get_context(&symbol, repository.as_deref(), limit)
            .await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&ctx)?,
            OutputFormat::Vimgrep => {
                anyhow::bail!("--format vimgrep is not supported for context; use text or json")
            }
            OutputFormat::Text => self.format_context(&ctx),
        })
    }

    fn format_context(&self, ctx: &SymbolContext) -> String {
        let mut out = format!(
            "Context for '{}'\n\
             ─────────────────────────────────────────\n",
            ctx.symbol
        );

        out.push_str(&format!(
            "Callers ({} total) — who uses this symbol:\n",
            ctx.caller_count
        ));
        if ctx.callers.is_empty() {
            out.push_str("  (none found)\n");
        } else {
            for edge in &ctx.callers {
                let alias_suffix = edge
                    .import_alias
                    .as_ref()
                    .map(|a| format!(", as {}", a))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  ← {} [{}{}]  {}:{}\n",
                    edge.symbol, edge.reference_kind, alias_suffix, edge.file_path, edge.line
                ));
            }
        }

        out.push_str(&format!(
            "\nCallees ({} total) — what this symbol uses:\n",
            ctx.callee_count
        ));
        if ctx.callees.is_empty() {
            out.push_str("  (none found)\n");
        } else {
            for edge in &ctx.callees {
                let alias_suffix = edge
                    .import_alias
                    .as_ref()
                    .map(|a| format!(", as {}", a))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  → {} [{}{}]  {}:{}\n",
                    edge.symbol, edge.reference_kind, alias_suffix, edge.file_path, edge.line
                ));
            }
        }

        out
    }
}
