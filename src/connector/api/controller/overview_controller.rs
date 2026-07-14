use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::application::{ChatClient, OverviewOptions, OverviewReport};
use crate::cli::{LlmTarget, OutputFormatTextJson, OverviewSection};
use crate::domain::{community_label, ChannelEndpoint, ExecutionFeature};

use super::super::Container;
use super::build_chat_client;

/// Communities / coupling hotspots shown at most in the text rendering.
const MAX_COUPLING_ROWS: usize = 5;
/// Dangling (unmatched) channel endpoints listed per direction.
const MAX_DANGLING_ROWS: usize = 5;
/// Upper bound on the digest handed to the LLM for the closing summary.
const MAX_SUMMARY_DIGEST_CHARS: usize = 12_000;

pub struct OverviewController<'a> {
    container: &'a Container,
}

impl<'a> OverviewController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// Build the combined repository overview report.
    pub async fn overview(
        &self,
        repository: Option<String>,
        format: OutputFormatTextJson,
        top: usize,
        skip: Vec<OverviewSection>,
        llm: LlmTarget,
        no_llm: bool,
    ) -> Result<String> {
        let repository_id = self
            .container
            .resolve_repository_id(repository.as_deref())
            .await;

        // Channel links need both ends of an edge, so the join runs over the
        // current namespace's repositories (mirroring the `channels` command).
        let namespace = self.container.namespace();
        let channel_scope: Vec<String> = self
            .container
            .list_use_case()
            .execute()
            .await
            .context("listing repositories for channel scope")?
            .iter()
            .filter(|r| r.namespace() == Some(namespace))
            .map(|r| r.id().to_string())
            .collect();

        let options = OverviewOptions {
            top,
            include_modules: !skip.contains(&OverviewSection::Modules),
            include_symbol_communities: !skip.contains(&OverviewSection::Communities),
            include_couplings: !skip.contains(&OverviewSection::Couplings),
            include_features: !skip.contains(&OverviewSection::Features),
            include_channels: !skip.contains(&OverviewSection::Channels),
            channel_scope,
        };

        let use_case = self.container.repository_overview_use_case();
        let mut report = use_case
            .execute(&repository_id, &options)
            .await
            .context("building repository overview")?;

        // LLM enrichment (best-effort): display names for both community
        // levels, then the closing executive summary. `--no-llm` skips all of
        // it; cached names still appear because the analyses load them.
        if !no_llm {
            match build_chat_client(llm, self.container.data_dir()) {
                Ok(chat) => {
                    let naming = self.container.community_naming_use_case();
                    if let Some(modules) = report.modules.as_mut() {
                        naming
                            .name_clusters(&mut modules.graph.clusters, chat.as_ref())
                            .await;
                    }
                    if let Some(communities) = report.symbol_communities.as_mut() {
                        naming
                            .name_symbol_communities(&mut communities.communities, chat.as_ref())
                            .await;
                    }
                    if !skip.contains(&OverviewSection::Summary) {
                        report.summary = generate_summary(&report, top, chat.as_ref()).await;
                    }
                }
                Err(e) => tracing::warn!("LLM disabled for overview, showing ids: {e}"),
            }
        }

        match format {
            OutputFormatTextJson::Json => {
                serde_json::to_string_pretty(&report).context("serializing overview report")
            }
            OutputFormatTextJson::Text => Ok(render_markdown(&report, top)),
        }
    }
}

/// Ask the LLM for a short executive summary of the assembled report.
/// Best-effort: any failure logs a warning and leaves the section out.
async fn generate_summary(
    report: &OverviewReport,
    top: usize,
    chat: &dyn ChatClient,
) -> Option<String> {
    let mut digest = render_markdown(report, top);
    digest.truncate(MAX_SUMMARY_DIGEST_CHARS);
    let system = "You are a senior software architect. You receive an auto-generated \
                  static-analysis overview of one code repository: index statistics, \
                  architectural modules (file clusters), behavioural symbol communities, \
                  coupling hotspots (god nodes), critical execution features, and \
                  cross-service channels. Write an executive summary in 5-10 sentences of \
                  plain prose: what the repository does, how it is shaped, where the risk \
                  concentrates, and the highest-leverage refactor target. No headings, no \
                  bullet lists, no restating of raw numbers unless they matter.";
    match chat.complete(system, &digest).await {
        Ok(text) => {
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(text)
        }
        Err(e) => {
            tracing::warn!("overview summary generation failed: {e}");
            None
        }
    }
}

// ── Markdown rendering ────────────────────────────────────────────────────

fn render_markdown(report: &OverviewReport, top: usize) -> String {
    let mut out = String::new();

    let title = report
        .stats
        .as_ref()
        .map(|s| s.name.as_str())
        .unwrap_or(report.repository_id.as_str());
    out.push_str(&format!("# Repository Overview — `{}`\n\n", title));

    if let Some(stats) = &report.stats {
        out.push_str(&format!(
            "`{}` — {} files, {} chunks · call graph: {} references ({} callers → {} callees)\n",
            stats.path,
            stats.file_count,
            stats.chunk_count,
            stats.call_graph_references,
            stats.call_graph_callers,
            stats.call_graph_callees,
        ));
        if !stats.languages.is_empty() {
            let total_chunks: u64 = stats.languages.iter().map(|l| l.chunk_count).sum();
            let shares: Vec<String> = stats
                .languages
                .iter()
                .map(|l| {
                    if total_chunks > 0 {
                        let pct = l.chunk_count as f64 / total_chunks as f64 * 100.0;
                        format!("{} {:.0}%", l.language, pct)
                    } else {
                        l.language.clone()
                    }
                })
                .collect();
            out.push_str(&format!("Languages: {}\n", shares.join(", ")));
        }
        out.push('\n');
    }

    if let Some(modules) = &report.modules {
        out.push_str("## Architectural Modules\n\n");
        render_modules(&mut out, modules, top);
    }

    if let Some(communities) = &report.symbol_communities {
        out.push_str("## Behavioural Communities (symbol level)\n\n");
        render_communities(&mut out, communities, top);
    }

    if let Some(couplings) = &report.couplings {
        out.push_str("## Coupling Hotspots\n\n");
        render_couplings(&mut out, couplings, report.symbol_communities.as_ref());
    }

    if let Some(features) = &report.features {
        out.push_str("## Critical Execution Features\n\n");
        render_features(&mut out, features);
    }

    if let Some(channels) = &report.channels {
        out.push_str("## Cross-Service Channels\n\n");
        render_channels(&mut out, channels);
    }

    if let Some(summary) = &report.summary {
        out.push_str("## Summary\n\n");
        out.push_str(summary);
        out.push_str("\n\n");
    }

    if !report.skipped.is_empty() {
        out.push_str("## Skipped Sections\n\n");
        for s in &report.skipped {
            out.push_str(&format!("- **{}**: {}\n", s.section, s.reason));
        }
        out.push('\n');
    }

    out
}

fn render_modules(out: &mut String, modules: &crate::application::ModuleOverview, top: usize) {
    let cg = &modules.graph;
    if cg.clusters.is_empty() {
        out.push_str("No clusters detected (graph may be too small or have no edges).\n\n");
        return;
    }

    let id_to_name: HashMap<&str, &str> = cg
        .clusters
        .iter()
        .map(|c| (c.id.as_str(), community_label(&c.display_name, &c.id)))
        .collect();

    out.push_str(&format!(
        "{} clusters over {} files and {} dependency edges\n\n",
        cg.clusters.len(),
        cg.total_files,
        cg.total_edges
    ));
    out.push_str("| Module | Files | Language | Cohesion | Top Dependencies |\n");
    out.push_str("|--------|-------|----------|----------|------------------|\n");
    for cluster in cg.clusters.iter().take(top) {
        let deps: Vec<String> = modules
            .dependencies
            .iter()
            .filter(|d| d.from_cluster_id == cluster.id)
            .take(3)
            .map(|d| {
                let name = id_to_name
                    .get(d.to_cluster_id.as_str())
                    .copied()
                    .unwrap_or(d.to_cluster_id.as_str());
                format!("{} ({:.0})", name, d.weight)
            })
            .collect();
        let deps_str = if deps.is_empty() {
            "—".to_string()
        } else {
            deps.join(", ")
        };
        out.push_str(&format!(
            "| {} | {} | {} | {:.2} | {} |\n",
            community_label(&cluster.display_name, &cluster.id),
            cluster.size,
            cluster.dominant_language,
            cluster.cohesion,
            deps_str
        ));
    }
    if cg.clusters.len() > top {
        out.push_str(&format!(
            "\n… and {} more (see `codesearch clusters list`)\n",
            cg.clusters.len() - top
        ));
    }
    out.push('\n');
}

fn render_communities(out: &mut String, graph: &crate::domain::SymbolCommunityGraph, top: usize) {
    if graph.communities.is_empty() {
        out.push_str(
            "No symbol communities detected (the call graph may be empty — \
             index with SCIP first).\n\n",
        );
        return;
    }
    out.push_str(&format!(
        "{} communities over {} symbols and {} call edges\n\n",
        graph.communities.len(),
        graph.total_symbols,
        graph.total_edges
    ));
    out.push_str("| Community | Symbols | Language | Cohesion |\n");
    out.push_str("|-----------|---------|----------|----------|\n");
    for c in graph.communities.iter().take(top) {
        out.push_str(&format!(
            "| {} | {} | {} | {:.2} |\n",
            community_label(&c.display_name, &c.id),
            c.size,
            c.dominant_language,
            c.cohesion
        ));
    }
    if graph.communities.len() > top {
        out.push_str(&format!(
            "\n… and {} more (see `codesearch symbol-clusters list`)\n",
            graph.communities.len() - top
        ));
    }
    out.push('\n');
}

fn render_couplings(
    out: &mut String,
    report: &crate::domain::CouplingReport,
    communities: Option<&crate::domain::SymbolCommunityGraph>,
) {
    out.push_str(&format!(
        "{} of {} symbol communities are internally fragile; {} held together by a \
         verified coupler\n\n",
        report.fragile_communities,
        report.total_communities,
        report.communities.len()
    ));
    if report.communities.is_empty() {
        out.push_str(
            "No god nodes found: no community is glued together by a single symbol \
             or call edge at the probed resolutions.\n\n",
        );
        return;
    }

    // Coupling community ids are the same stable ids the symbol-clusters
    // command reports, so LLM display names can be joined in.
    let labels: HashMap<&str, &str> = communities
        .map(|g| {
            g.communities
                .iter()
                .map(|c| (c.id.as_str(), community_label(&c.display_name, &c.id)))
                .collect()
        })
        .unwrap_or_default();

    for (i, c) in report
        .communities
        .iter()
        .take(MAX_COUPLING_ROWS)
        .enumerate()
    {
        let label = labels
            .get(c.community_id.as_str())
            .copied()
            .unwrap_or(c.community_id.as_str());
        out.push_str(&format!(
            "{}. **{}** ({} symbols) — splits into blocks of {} + {} at γ={}\n",
            i + 1,
            label,
            c.size,
            c.sub_block_a.len(),
            c.sub_block_b.len(),
            c.gamma_split
        ));
        for coupler in &c.couplers {
            let element = coupler.elements.join(" ↔ ");
            out.push_str(&format!(
                "   - `{}` — strength {:.2} (split {:.2} vs baseline {:.2})\n",
                element,
                coupler.coupling_strength,
                coupler.split_probability,
                coupler.baseline_split_probability
            ));
        }
    }
    if report.communities.len() > MAX_COUPLING_ROWS {
        out.push_str(&format!(
            "… and {} more (see `codesearch couplings --level symbol`)\n",
            report.communities.len() - MAX_COUPLING_ROWS
        ));
    }
    out.push('\n');
}

fn render_features(out: &mut String, features: &[ExecutionFeature]) {
    if features.is_empty() {
        out.push_str(
            "No entry-point features found (the call graph may have no execution \
             edges — import a SCIP index first).\n\n",
        );
        return;
    }
    out.push_str("| Feature | Criticality | Reach | Depth | Files | Entry Point |\n");
    out.push_str("|---------|-------------|-------|-------|-------|-------------|\n");
    for f in features {
        out.push_str(&format!(
            "| {} | {:.2} | {} | {} | {} | `{}` |\n",
            f.name, f.criticality, f.reach, f.depth, f.file_count, f.entry_point
        ));
    }
    out.push('\n');
}

fn render_channels(out: &mut String, channels: &crate::application::ChannelOverview) {
    let report = &channels.report;
    let repo_label = |id: &str| -> String {
        channels
            .repository_names
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    };

    if report.edges.is_empty()
        && report.unmatched_producers.is_empty()
        && report.unmatched_consumers.is_empty()
    {
        out.push_str("No channel endpoints detected in this repository.\n\n");
        return;
    }

    if !report.edges.is_empty() {
        // Collapse the file-pair edges down to repository level for the
        // overview: (protocol, channel, producer repo, consumer repo).
        let mut collapsed: HashMap<(String, String, String, String), usize> = HashMap::new();
        for edge in &report.edges {
            let key = (
                edge.producer.protocol().to_string(),
                edge.channel().to_string(),
                edge.producer.repository_id().to_string(),
                edge.consumer.repository_id().to_string(),
            );
            *collapsed.entry(key).or_insert(0) += edge.weight;
        }
        let mut rows: Vec<_> = collapsed.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        out.push_str(&format!("{} channel link(s):\n\n", rows.len()));
        for ((protocol, channel, from, to), weight) in rows {
            out.push_str(&format!(
                "- {} `{}`: {} → {} ({} call site{})\n",
                protocol,
                channel,
                repo_label(&from),
                repo_label(&to),
                weight,
                if weight == 1 { "" } else { "s" }
            ));
        }
        out.push('\n');
    }

    render_dangling(out, "producer", &report.unmatched_producers);
    render_dangling(out, "consumer", &report.unmatched_consumers);

    if !report.noisy_channels.is_empty() {
        out.push_str(&format!(
            "Noisy channels excluded: {}\n\n",
            report.noisy_channels.join(", ")
        ));
    }
}

fn render_dangling(out: &mut String, role: &str, endpoints: &[ChannelEndpoint]) {
    if endpoints.is_empty() {
        return;
    }
    out.push_str(&format!(
        "⚠ {} dangling {} endpoint(s) (no known counterpart):\n",
        endpoints.len(),
        role
    ));
    for e in endpoints.iter().take(MAX_DANGLING_ROWS) {
        out.push_str(&format!(
            "- {} `{}` at {}:{}\n",
            e.protocol(),
            e.channel_normalized(),
            e.file_path(),
            e.line()
        ));
    }
    if endpoints.len() > MAX_DANGLING_ROWS {
        out.push_str(&format!(
            "  … and {} more\n",
            endpoints.len() - MAX_DANGLING_ROWS
        ));
    }
    out.push('\n');
}
