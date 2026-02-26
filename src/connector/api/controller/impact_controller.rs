use std::collections::HashMap;

use anyhow::Result;

use crate::cli::OutputFormat;
use crate::{ImpactAnalysis, ImpactNode};

use super::super::Container;

pub struct ImpactController<'a> {
    container: &'a Container,
}

impl<'a> ImpactController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
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
            OutputFormat::Json => serde_json::to_string_pretty(&analysis)?,
            OutputFormat::Vimgrep => {
                anyhow::bail!("--format vimgrep is not supported for impact; use text or json")
            }
            OutputFormat::Text => {
                // Resolve repository UUIDs to human-readable names.
                let repo_names = self.build_repo_name_map(&analysis).await;
                self.format_impact(&analysis, &repo_names)
            }
        })
    }

    /// Build a map from repository UUID → human-readable name by looking up
    /// all unique repository IDs that appear in the analysis results.
    async fn build_repo_name_map(&self, analysis: &ImpactAnalysis) -> HashMap<String, String> {
        let list_uc = self.container.list_use_case();
        let mut map = HashMap::new();

        // Collect unique repo IDs from all impact nodes.
        let unique_ids: std::collections::HashSet<&str> = analysis
            .by_depth
            .iter()
            .flatten()
            .map(|n| n.repository_id.as_str())
            .collect();

        for id in unique_ids {
            if let Ok(Some(repo)) = list_uc.get_by_id(id).await {
                map.insert(id.to_string(), repo.name().to_string());
            }
        }
        map
    }

    fn format_impact(
        &self,
        analysis: &ImpactAnalysis,
        repo_names: &HashMap<String, String>,
    ) -> String {
        if analysis.total_affected == 0 {
            return format!(
                "No callers found for '{}'. Either the symbol is a root entry point or \
                 it hasn't been indexed yet.",
                analysis.root_symbol
            );
        }

        let mut out = format!(
            "Impact analysis for '{}'\n\
             ─────────────────────────────────────────\n",
            analysis.root_symbol
        );

        let all_nodes: Vec<&ImpactNode> = analysis.by_depth.iter().flatten().collect();

        // Build children_map: symbol → nodes that list it as via_symbol.
        let mut children_map: HashMap<&str, Vec<&ImpactNode>> = HashMap::new();
        for node in &all_nodes {
            if let Some(via) = node.via_symbol.as_deref() {
                children_map.entry(via).or_default().push(node);
            }
        }

        // Leaf nodes (no one calls them) are the outermost callers in the chain.
        let leaf_nodes: Vec<&ImpactNode> = all_nodes
            .iter()
            .copied()
            .filter(|n| !children_map.contains_key(n.symbol.as_str()))
            .collect();

        // Lookup by (depth, symbol) for unambiguous path tracing.
        // or_insert keeps only the *first* ImpactNode seen for any (depth, symbol) pair.
        // When the same symbol appears at the same depth via two different call paths, the
        // duplicate is intentionally dropped so that each (depth, symbol) key maps to exactly
        // one parent — giving the path-tracing loop a single, deterministic choice at every
        // step.  The trade-off is that alternate routes to the same node are not rendered;
        // this is acceptable here because the goal is to show one representative call chain
        // from each leaf up to the queried symbol, not to enumerate every possible path.
        let mut node_by_depth_symbol: HashMap<(usize, &str), &ImpactNode> = HashMap::new();
        for node in &all_nodes {
            node_by_depth_symbol
                .entry((node.depth, node.symbol.as_str()))
                .or_insert(node);
        }

        // Trace each leaf back toward the root and collect the full call chain.
        // Group the resulting chains by the repository of their outermost caller so
        // that the repository name can be rendered as a top-level tree node.
        //
        // Insertion order is preserved by building `repos` as a deduplicated Vec so
        // that the output order matches the BFS discovery order rather than being
        // arbitrary (HashMap iteration order).
        let mut paths_by_repo: HashMap<&str, Vec<Vec<&ImpactNode>>> = HashMap::new();
        let mut repo_order: Vec<&str> = Vec::new();

        for &leaf in &leaf_nodes {
            // Trace from leaf back toward the root symbol.
            let mut path: Vec<&ImpactNode> = vec![leaf];
            let mut current = leaf;
            while let Some(via) = current.via_symbol.as_deref() {
                let parent_depth = current.depth.saturating_sub(1);
                if let Some(&parent) = node_by_depth_symbol.get(&(parent_depth, via)) {
                    path.push(parent);
                    current = parent;
                } else {
                    break;
                }
            }

            let repo = leaf.repository_id.as_str();
            if !paths_by_repo.contains_key(repo) {
                repo_order.push(repo);
            }
            paths_by_repo.entry(repo).or_default().push(path);
        }

        // Render each repository as a named root node, with its call chains nested beneath.
        for (repo_idx, repo) in repo_order.iter().enumerate() {
            let display_name = repo_names.get(*repo).map(String::as_str).unwrap_or(repo);
            out.push_str(display_name);
            out.push('\n');

            let repo_paths = &paths_by_repo[repo];
            for (idx, path) in repo_paths.iter().enumerate() {
                // indent_offset=1 shifts every node one level deeper so the chain
                // hangs beneath the repository root node.
                Self::render_reversed_path(path, &analysis.root_symbol, &mut out, 1, repo_names);

                if idx < repo_paths.len() - 1 {
                    out.push('\n');
                }
            }

            if repo_idx < repo_order.len() - 1 {
                out.push('\n');
            }
        }

        out
    }

    fn alias_suffix(alias: &Option<String>) -> String {
        alias
            .as_ref()
            .map(|a| format!(", as {}", a))
            .unwrap_or_default()
    }

    /// Render a single path (leaf → … → root) as an indented tree.
    ///
    /// `path[0]` is the most-upstream caller; the queried symbol is appended as
    /// the terminal leaf.
    ///
    /// `indent_offset` shifts every node that many levels deeper, which is used
    /// to nest chains beneath a repository root node (`indent_offset = 1`).
    /// Pass `0` for the original flat rendering.
    ///
    /// Repository transitions within the chain are annotated with a `[repo]`
    /// badge on the first node that belongs to a different repository than the
    /// previous one.  The first node's repository is always the group header
    /// and therefore never annotated.
    fn render_reversed_path(
        path: &[&ImpactNode],
        root_symbol: &str,
        out: &mut String,
        indent_offset: usize,
        repo_names: &HashMap<String, String>,
    ) {
        if path.is_empty() {
            return;
        }

        // Start tracking from the outermost caller's repository.  That repo is
        // already shown as the group heading, so depth-0 is never annotated.
        let mut current_repo = path[0].repository_id.as_str();

        for (depth, node) in path.iter().enumerate() {
            let alias_suffix = Self::alias_suffix(&node.import_alias);
            let total_depth = depth + indent_offset;

            // Annotate whenever the repository changes relative to the previous node.
            let repo_badge = if depth > 0 && node.repository_id.as_str() != current_repo {
                current_repo = node.repository_id.as_str();
                let display = repo_names
                    .get(&node.repository_id)
                    .map(String::as_str)
                    .unwrap_or(&node.repository_id);
                format!("[{}] ", display)
            } else {
                String::new()
            };

            if total_depth == 0 {
                // Top-level with no offset: no └── prefix (original behaviour).
                out.push_str(&format!(
                    "{}{} [{}{}] {}:{}\n",
                    repo_badge,
                    node.symbol,
                    node.reference_kind,
                    alias_suffix,
                    node.file_path,
                    node.line,
                ));
            } else {
                let indent = "    ".repeat(total_depth - 1);
                out.push_str(&format!(
                    "{}└── {}{} [{}{}] {}:{}\n",
                    indent,
                    repo_badge,
                    node.symbol,
                    node.reference_kind,
                    alias_suffix,
                    node.file_path,
                    node.line,
                ));
            }
        }

        // Queried symbol is always the terminal leaf.
        let indent = "    ".repeat(path.len() + indent_offset - 1);
        out.push_str(&format!("{}└── {}\n", indent, root_symbol));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImpactAnalysis;

    fn node(symbol: &str, depth: usize, repo: &str, file: &str, via: Option<&str>) -> ImpactNode {
        ImpactNode {
            symbol: symbol.to_string(),
            depth,
            file_path: file.to_string(),
            line: 1,
            reference_kind: "method_call".to_string(),
            repository_id: repo.to_string(),
            import_alias: None,
            via_symbol: via.map(str::to_string),
        }
    }

    /// Cross-repo chain should show `[repo]` badge on the first node whose
    /// repository differs from the previous node in the chain.
    #[test]
    fn test_render_cross_repo_badge() {
        // Simulate: advertiseUsersAboutNewUser (root, php-common)
        //   called by Home::getNewUser (depth 1, php-common)
        //     called by AssociateAdminDevice::post (depth 2, apiuser)
        let analysis = ImpactAnalysis {
            root_symbol: "advertiseUsersAboutNewUser".to_string(),
            total_affected: 2,
            max_depth_reached: 2,
            by_depth: vec![
                vec![node(
                    "Home::getNewUser",
                    1,
                    "php-common",
                    "Home.php",
                    Some("advertiseUsersAboutNewUser"),
                )],
                vec![node(
                    "AssociateAdminDevice::post",
                    2,
                    "apiuser",
                    "AssociateAdminDevice.php",
                    Some("Home::getNewUser"),
                )],
            ],
        };

        // Use a dummy controller — format_impact only uses static methods.
        // We can't easily construct Container in tests, so replicate the
        // formatting logic from format_impact here.
        let all_nodes: Vec<&ImpactNode> = analysis.by_depth.iter().flatten().collect();
        let mut children_map: HashMap<&str, Vec<&ImpactNode>> = HashMap::new();
        for n in &all_nodes {
            if let Some(via) = n.via_symbol.as_deref() {
                children_map.entry(via).or_default().push(n);
            }
        }
        let leaf_nodes: Vec<&ImpactNode> = all_nodes
            .iter()
            .copied()
            .filter(|n| !children_map.contains_key(n.symbol.as_str()))
            .collect();

        let mut node_by_depth_symbol: HashMap<(usize, &str), &ImpactNode> = HashMap::new();
        for n in &all_nodes {
            node_by_depth_symbol
                .entry((n.depth, n.symbol.as_str()))
                .or_insert(n);
        }

        let mut paths_by_repo: HashMap<&str, Vec<Vec<&ImpactNode>>> = HashMap::new();
        let mut repo_order: Vec<&str> = Vec::new();
        for &leaf in &leaf_nodes {
            let mut path: Vec<&ImpactNode> = vec![leaf];
            let mut current = leaf;
            while let Some(via) = current.via_symbol.as_deref() {
                let parent_depth = current.depth.saturating_sub(1);
                if let Some(&parent) = node_by_depth_symbol.get(&(parent_depth, via)) {
                    path.push(parent);
                    current = parent;
                } else {
                    break;
                }
            }
            let repo = leaf.repository_id.as_str();
            if !paths_by_repo.contains_key(repo) {
                repo_order.push(repo);
            }
            paths_by_repo.entry(repo).or_default().push(path);
        }

        let mut out = String::new();
        for repo in &repo_order {
            out.push_str(repo);
            out.push('\n');
            for path in &paths_by_repo[repo] {
                let empty_names = HashMap::new();
                ImpactController::render_reversed_path(
                    path,
                    &analysis.root_symbol,
                    &mut out,
                    1,
                    &empty_names,
                );
            }
        }

        // The leaf is AssociateAdminDevice::post (apiuser), so the group header is "apiuser".
        // path[0] = AssociateAdminDevice::post (apiuser) — same as header, no badge.
        // path[1] = Home::getNewUser (php-common) — different repo, gets [php-common] badge.
        let expected = "apiuser\n└── AssociateAdminDevice::post [method_call] AssociateAdminDevice.php:1\n    └── [php-common] Home::getNewUser [method_call] Home.php:1\n        └── advertiseUsersAboutNewUser\n";

        assert_eq!(out, expected, "\nActual output:\n{}", out);
    }

    /// Single-repo chain should have no badges at all.
    #[test]
    fn test_render_single_repo_no_badge() {
        let analysis = ImpactAnalysis {
            root_symbol: "targetFn".to_string(),
            total_affected: 1,
            max_depth_reached: 1,
            by_depth: vec![vec![node(
                "callerFn",
                1,
                "my-repo",
                "caller.php",
                Some("targetFn"),
            )]],
        };

        let nodes: Vec<&ImpactNode> = analysis.by_depth.iter().flatten().collect();
        let path: Vec<&ImpactNode> = vec![nodes[0]];
        let mut out = String::new();
        out.push_str("my-repo\n");
        let empty_names = HashMap::new();
        ImpactController::render_reversed_path(
            &path,
            &analysis.root_symbol,
            &mut out,
            1,
            &empty_names,
        );

        let expected = "my-repo\n└── callerFn [method_call] caller.php:1\n    └── targetFn\n";

        assert_eq!(out, expected, "\nActual output:\n{}", out);
    }
}
