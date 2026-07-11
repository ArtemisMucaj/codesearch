use std::collections::HashMap;

use anyhow::Result;

use crate::domain::ChannelEndpoint;

use super::super::Container;

pub struct StatsController<'a> {
    container: &'a Container,
}

impl<'a> StatsController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn stats(&self) -> Result<String> {
        let use_case = self.container.list_use_case();
        let repos = use_case.execute().await?;

        let call_graph_use_case = self.container.call_graph_use_case();
        let channel_repo = self.container.channel_endpoint_repository();
        let analysis_repo = self.container.analysis_repository();
        let memory_stats = self.fetch_memory_stats().await.unwrap_or_default();

        let mut repo_details = Vec::new();
        let mut globals = GlobalStats::default();

        for repo in &repos {
            let repo_id = repo.id();
            let mut lang_map = HashMap::<String, (u64, u64)>::new();
            for (lang, stats) in repo.languages() {
                lang_map.insert(lang.clone(), (stats.file_count, stats.chunk_count));
                // Accumulate across repos: HashMap::extend would overwrite the
                // running total for a shared language, so sum the counts instead.
                let entry = globals.languages.entry(lang.clone()).or_insert((0, 0));
                entry.0 += stats.file_count;
                entry.1 += stats.chunk_count;
            }

            // Call graph stats
            let cg_stats = call_graph_use_case.stats(repo_id).await.unwrap_or_default();
            globals.cg_refs += cg_stats.total_references;
            globals.cg_callers += cg_stats.unique_callers;
            globals.cg_callees += cg_stats.unique_callees;

            // Channel endpoints
            let endpoints = channel_repo
                .find_by_repository(repo_id)
                .await
                .unwrap_or_default();
            globals.channels += endpoints.len() as u64;

            // Analysis cache
            let cluster_graph = analysis_repo
                .load_cluster_graph(repo_id)
                .await
                .ok()
                .flatten();
            let community_graph = analysis_repo
                .load_symbol_community_graph(repo_id)
                .await
                .ok()
                .flatten();
            let features = analysis_repo
                .load_execution_features(repo_id)
                .await
                .ok()
                .flatten();

            let cluster_count = cluster_graph
                .as_ref()
                .map(|g| g.clusters.len() as u64)
                .unwrap_or(0);
            let community_count = community_graph
                .as_ref()
                .map(|g| g.communities.len() as u64)
                .unwrap_or(0);
            let feature_count = features.as_ref().map(|f| f.len() as u64).unwrap_or(0);

            globals.clusters += cluster_count;
            globals.communities += community_count;
            globals.features += feature_count;

            // Embedding config from namespace_config table
            let embedding_info = self.format_embedding_info(repo.namespace());

            repo_details.push(RepoDetail {
                name: repo.name(),
                path: repo.path(),
                git_remote: repo.git_remote(),
                file_count: repo.file_count(),
                chunk_count: repo.chunk_count(),
                languages: lang_map,
                namespace: repo.namespace(),
                embedding_info,
                call_graph: cg_stats,
                channel_count: endpoints.len() as u64,
                channels_by_protocol: count_by_protocol(&endpoints),
                cluster_count,
                community_count,
                feature_count,
            });
        }

        Ok(self.format_output(&repos, &repo_details, &globals, &memory_stats))
    }

    async fn fetch_memory_stats(&self) -> Result<crate::application::MemoryStats> {
        let repo = self.container.memory_repository()?;
        repo.stats().await.map_err(|e| anyhow::anyhow!("{}", e))
    }

    fn format_embedding_info(&self, namespace: Option<&str>) -> String {
        if let Some(ns) = namespace {
            if let Some(cfg) = crate::namespace_embedding_config(
                std::path::Path::new(self.container.data_dir()),
                ns,
            ) {
                return format!(
                    "target={}, model={}, dims={}",
                    cfg.embedding_target, cfg.embedding_model, cfg.dimensions
                );
            }
        }
        "no embeddings".to_string()
    }

    fn format_output(
        &self,
        repos: &[crate::Repository],
        repo_details: &[RepoDetail],
        globals: &GlobalStats,
        memory_stats: &crate::application::MemoryStats,
    ) -> String {
        let total_repos = repos.len();
        let total_files: u64 = repos.iter().map(|r| r.file_count()).sum();
        let total_chunks: u64 = repos.iter().map(|r| r.chunk_count()).sum();

        let mut lines = Vec::new();

        // Header
        lines.push("CodeSearch Statistics".to_string());
        lines.push("=".repeat(40));
        lines.push(String::new());

        // Global summary
        lines.push("Global Summary".to_string());
        lines.push("-".repeat(40));
        lines.push(format!("  Repositories:  {}", total_repos));
        lines.push(format!("  Total Files:   {}", total_files));
        lines.push(format!("  Total Chunks:  {}", total_chunks));
        lines.push(format!("  Data Dir:      {}", self.container.data_dir()));
        lines.push(String::new());

        // Languages (global)
        if !globals.languages.is_empty() {
            lines.push("Languages (all repos)".to_string());
            lines.push("-".repeat(40));
            let mut lang_keys: Vec<&String> = globals.languages.keys().collect();
            lang_keys.sort();
            for lang in lang_keys {
                if let Some((fc, cc)) = globals.languages.get(lang) {
                    lines.push(format!("  {}: {} files, {} chunks", lang, fc, cc));
                }
            }
            lines.push(String::new());
        }

        // Call graph summary
        lines.push("Call Graph".to_string());
        lines.push("-".repeat(40));
        lines.push(format!("  Total references: {}", globals.cg_refs));
        lines.push(format!("  Unique callers:   {}", globals.cg_callers));
        lines.push(format!("  Unique callees:   {}", globals.cg_callees));
        lines.push(String::new());

        // Analysis cache summary
        lines.push("Analysis Cache".to_string());
        lines.push("-".repeat(40));
        lines.push(format!("  File-level clusters:     {}", globals.clusters));
        lines.push(format!(
            "  Symbol communities:      {}",
            globals.communities
        ));
        lines.push(format!("  Execution features:      {}", globals.features));
        lines.push(String::new());

        // Channel endpoints summary
        if globals.channels > 0 {
            lines.push("Channel Endpoints".to_string());
            lines.push("-".repeat(40));
            lines.push(format!("  Total endpoints: {}", globals.channels));
            lines.push(String::new());
        }

        // Memory store summary
        lines.push("Memory Store".to_string());
        lines.push("-".repeat(40));
        lines.push(format!("  Total items:     {}", memory_stats.total_items));
        if !memory_stats.items_by_kind.is_empty() {
            lines.push("  Items by kind:".to_string());
            for (kind, count) in &memory_stats.items_by_kind {
                lines.push(format!("    {}: {}", kind, count));
            }
        }
        lines.push(format!(
            "  Total sessions:  {}",
            memory_stats.total_sessions
        ));
        lines.push(format!("  Total nodes:     {}", memory_stats.total_nodes));
        if !memory_stats.nodes_by_kind.is_empty() {
            lines.push("  Nodes by kind:".to_string());
            for (kind, count) in &memory_stats.nodes_by_kind {
                lines.push(format!("    {}: {}", kind, count));
            }
        }
        lines.push(String::new());

        // Per-repository detail
        if !repo_details.is_empty() {
            lines.push("Per-Repository Details".to_string());
            lines.push("=".repeat(40));
            lines.push(String::new());

            for detail in repo_details {
                lines.push(format!("  Repository: {}", detail.name));
                lines.push(format!("    Path:            {}", detail.path));
                if let Some(remote) = detail.git_remote {
                    lines.push(format!("    Git Remote:      {}", remote));
                }
                lines.push(format!(
                    "    Namespace:       {}",
                    detail.namespace.unwrap_or("(none)")
                ));
                lines.push(format!("    Embedding:       {}", detail.embedding_info));
                lines.push(format!("    Files:           {}", detail.file_count));
                lines.push(format!("    Chunks:          {}", detail.chunk_count));
                lines.push(String::new());

                // Languages (per repo)
                if !detail.languages.is_empty() {
                    lines.push("    Languages:".to_string());
                    let mut lang_keys: Vec<&String> = detail.languages.keys().collect();
                    lang_keys.sort();
                    for lang in lang_keys {
                        if let Some((fc, cc)) = detail.languages.get(lang) {
                            lines.push(format!("      {}: {} files, {} chunks", lang, fc, cc));
                        }
                    }
                    lines.push(String::new());
                }

                // Call graph (per repo)
                if detail.call_graph.total_references > 0 {
                    lines.push("    Call Graph:".to_string());
                    lines.push(format!(
                        "      Total references: {}",
                        detail.call_graph.total_references
                    ));
                    lines.push(format!(
                        "      Unique callers:   {}",
                        detail.call_graph.unique_callers
                    ));
                    lines.push(format!(
                        "      Unique callees:   {}",
                        detail.call_graph.unique_callees
                    ));

                    if !detail.call_graph.by_reference_kind.is_empty() {
                        lines.push("      By reference kind:".to_string());
                        for (kind, count) in &detail.call_graph.by_reference_kind {
                            lines.push(format!("        {}: {}", kind, count));
                        }
                    }

                    if !detail.call_graph.by_language.is_empty() {
                        lines.push("      By language:".to_string());
                        for (lang, count) in &detail.call_graph.by_language {
                            lines.push(format!("        {}: {}", lang, count));
                        }
                    }
                    lines.push(String::new());
                }

                // Channel endpoints (per repo)
                if detail.channel_count > 0 {
                    lines.push("    Channels:".to_string());
                    lines.push(format!("      Total endpoints: {}", detail.channel_count));
                    if !detail.channels_by_protocol.is_empty() {
                        lines.push("      By protocol:".to_string());
                        for (proto, count) in &detail.channels_by_protocol {
                            lines.push(format!("        {}: {}", proto, count));
                        }
                    }
                    lines.push(String::new());
                }

                // Analysis cache (per repo)
                lines.push("    Analysis Cache:".to_string());
                lines.push(format!("      Clusters:       {}", detail.cluster_count));
                lines.push(format!("      Communities:    {}", detail.community_count));
                lines.push(format!("      Features:       {}", detail.feature_count));
                lines.push(String::new());

                lines.push(String::new());
            }
        }

        lines.join("\n")
    }
}

/// Aggregated totals across all repositories, shown in the global summary.
#[derive(Default)]
struct GlobalStats {
    /// Per-language (file_count, chunk_count) summed over every repo.
    languages: HashMap<String, (u64, u64)>,
    cg_refs: u64,
    cg_callers: u64,
    cg_callees: u64,
    channels: u64,
    clusters: u64,
    communities: u64,
    features: u64,
}

struct RepoDetail<'a> {
    name: &'a str,
    path: &'a str,
    git_remote: Option<&'a str>,
    file_count: u64,
    chunk_count: u64,
    languages: HashMap<String, (u64, u64)>,
    namespace: Option<&'a str>,
    embedding_info: String,
    call_graph: crate::application::CallGraphStats,
    channel_count: u64,
    channels_by_protocol: Vec<(String, u64)>,
    cluster_count: u64,
    community_count: u64,
    feature_count: u64,
}

fn count_by_protocol(endpoints: &[ChannelEndpoint]) -> Vec<(String, u64)> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for ep in endpoints {
        let proto = ep.protocol().as_str().to_string();
        *counts.entry(proto).or_insert(0) += 1;
    }
    let mut result: Vec<(String, u64)> = counts.into_iter().collect();
    result.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    result
}
