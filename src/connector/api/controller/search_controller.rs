use anyhow::Result;
use serde::Serialize;

use crate::cli::OutputFormat;
use crate::{ImpactAnalysis, Repository, SearchQuery, SearchResult, SymbolContext};

use super::super::Container;

pub struct SearchController<'a> {
    container: &'a Container,
}

#[derive(Serialize)]
struct JsonSearchResult<'a> {
    file_path: &'a str,
    start_line: u32,
    end_line: u32,
    score: f32,
    language: String,
    node_type: &'a str,
    symbol_name: Option<&'a str>,
    content: &'a str,
}

impl<'a> SearchController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn search(
        &self,
        query: String,
        num: usize,
        min_score: Option<f32>,
        languages: Option<Vec<String>>,
        repositories: Option<Vec<String>>,
        format: OutputFormat,
        hybrid: bool,
    ) -> Result<String> {
        let mut search_query = SearchQuery::new(&query).with_limit(num).with_hybrid(hybrid);

        if let Some(score) = min_score {
            search_query = search_query.with_min_score(score);
        }
        if let Some(langs) = languages {
            search_query = search_query.with_languages(langs);
        }
        if let Some(repos) = repositories {
            search_query = search_query.with_repositories(repos);
        }

        let use_case = self.container.search_use_case();
        let results = use_case.execute(search_query).await?;

        Ok(match format {
            OutputFormat::Text => self.format_search_results(&results),
            OutputFormat::Json => self.format_search_results_json(&results),
            OutputFormat::Vimgrep => self.format_search_results_vimgrep(&results),
        })
    }

    pub async fn impact(
        &self,
        symbol: String,
        depth: usize,
        repository: Option<String>,
        format: OutputFormat,
    ) -> Result<String> {
        let use_case = self.container.impact_use_case();
        let analysis = use_case
            .analyze(&symbol, depth, repository.as_deref())
            .await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&analysis).unwrap_or_else(|e| {
                eprintln!("Failed to serialize impact analysis: {e}");
                "{}".to_string()
            }),
            _ => self.format_impact(&analysis),
        })
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
            OutputFormat::Json => serde_json::to_string_pretty(&ctx).unwrap_or_else(|e| {
                eprintln!("Failed to serialize symbol context: {e}");
                "{}".to_string()
            }),
            _ => self.format_context(&ctx),
        })
    }

    pub async fn stats(&self) -> Result<String> {
        let use_case = self.container.list_use_case();
        let repos = use_case.execute().await?;
        Ok(self.format_stats(&repos))
    }

    // ── formatting helpers ────────────────────────────────────────────────────

    fn format_search_results(&self, results: &[SearchResult]) -> String {
        if results.is_empty() {
            return "No results found.".to_string();
        }

        let mut output = format!("Found {} results:\n\n", results.len());

        for (i, result) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {} (score: {:.3})\n",
                i + 1,
                result.chunk().location(),
                result.score()
            ));

            if let Some(name) = result.chunk().symbol_name() {
                output.push_str(&format!(
                    "   Symbol: {} ({})\n",
                    name,
                    result.chunk().node_type()
                ));
            }

            let preview: String = result
                .chunk()
                .content()
                .lines()
                .take(10)
                .map(|l| format!("   | {}", l))
                .collect::<Vec<_>>()
                .join("\n");
            output.push_str(&preview);
            output.push_str("\n\n");
        }

        output
    }

    fn format_search_results_json(&self, results: &[SearchResult]) -> String {
        let json_results: Vec<JsonSearchResult> = results
            .iter()
            .map(|r| JsonSearchResult {
                file_path: r.chunk().file_path(),
                start_line: r.chunk().start_line(),
                end_line: r.chunk().end_line(),
                score: r.score(),
                language: r.chunk().language().to_string(),
                node_type: r.chunk().node_type().as_str(),
                symbol_name: r.chunk().symbol_name(),
                content: r.chunk().content(),
            })
            .collect();

        serde_json::to_string_pretty(&json_results).unwrap_or_else(|e| {
            eprintln!("Failed to serialize search results: {e}");
            "[]".to_string()
        })
    }

    /// Format results in vimgrep-compatible format: `file:line:col:text`
    /// This is directly consumable by Neovim's quickfix list and Telescope.
    fn format_search_results_vimgrep(&self, results: &[SearchResult]) -> String {
        results
            .iter()
            .map(|r| {
                let symbol = r
                    .chunk()
                    .symbol_name()
                    .unwrap_or(r.chunk().node_type().as_str());
                let first_line = r.chunk().content().lines().next().unwrap_or("");
                format!(
                    "{}:{}:1:[{:.3}] {} - {}",
                    r.chunk().file_path(),
                    r.chunk().start_line(),
                    r.score(),
                    symbol,
                    first_line.trim(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn format_impact(&self, analysis: &ImpactAnalysis) -> String {
        if analysis.total_affected == 0 {
            return format!(
                "No callers found for '{}'. Either the symbol is a root entry point or \
                 it hasn't been indexed yet.",
                analysis.root_symbol
            );
        }

        let mut out = format!(
            "Impact analysis for '{}'\n\
             ─────────────────────────────────────────\n\
             Total affected symbols : {}\n\
             Max depth reached      : {}\n\n",
            analysis.root_symbol, analysis.total_affected, analysis.max_depth_reached
        );

        for (depth_idx, nodes) in analysis.by_depth.iter().enumerate() {
            if nodes.is_empty() {
                continue;
            }
            out.push_str(&format!("Depth {} ({} symbol(s)):\n", depth_idx + 1, nodes.len()));
            for node in nodes {
                out.push_str(&format!(
                    "  • {} [{}]  {}\n",
                    node.symbol, node.reference_kind, node.file_path
                ));
            }
            out.push('\n');
        }

        out
    }

    fn format_context(&self, ctx: &SymbolContext) -> String {
        let mut out = format!(
            "Context for '{}'\n\
             ─────────────────────────────────────────\n",
            ctx.symbol
        );

        out.push_str(&format!(
            "\nCallers ({} total) — who uses this symbol:\n",
            ctx.caller_count
        ));
        if ctx.callers.is_empty() {
            out.push_str("  (none found)\n");
        } else {
            for edge in &ctx.callers {
                out.push_str(&format!(
                    "  ← {} [{}]  {}:{}\n",
                    edge.symbol, edge.reference_kind, edge.file_path, edge.line
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
                out.push_str(&format!(
                    "  → {} [{}]  {}:{}\n",
                    edge.symbol, edge.reference_kind, edge.file_path, edge.line
                ));
            }
        }

        out
    }

    fn format_stats(&self, repos: &[Repository]) -> String {
        let total_repos = repos.len();
        let total_files: u64 = repos.iter().map(|r| r.file_count()).sum();
        let total_chunks: u64 = repos.iter().map(|r| r.chunk_count()).sum();

        format!(
            "CodeSearch Statistics\n=====================\nRepositories: {}\nTotal Files:  {}\nTotal Chunks: {}\nData Dir:     {}",
            total_repos,
            total_files,
            total_chunks,
            self.container.data_dir()
        )
    }
}
