use anyhow::{Context, Result};

use crate::connector::api::container::Container;
use crate::domain::Repository;

pub struct UsesController<'a> {
    container: &'a Container,
}

impl<'a> UsesController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn uses(&self, from: String, to: String) -> Result<String> {
        let uc = self.container.file_graph_use_case();

        // Resolve names/IDs for both repos
        let all_repos: Vec<Repository> = self
            .container
            .list_use_case()
            .execute()
            .await
            .context("Failed to list repositories")?;

        let resolve = |name_or_id: &str| -> Option<(String, String)> {
            all_repos
                .iter()
                .find(|r: &&Repository| r.id() == name_or_id)
                .or_else(|| {
                    all_repos
                        .iter()
                        .find(|r: &&Repository| r.name().to_lowercase() == name_or_id.to_lowercase())
                })
                .map(|r: &Repository| (r.id().to_string(), r.name().to_string()))
        };

        let (from_id, from_name) = resolve(&from)
            .with_context(|| format!("Repository not found: '{from}'"))?;
        let (to_id, to_name) = resolve(&to)
            .with_context(|| format!("Repository not found: '{to}'"))?;

        // Build a cross-repo graph that includes both repos
        let graph = uc
            .build_graph(Some(&[from_id.clone(), to_id.clone()]), 1, true)
            .await
            .context("Failed to build file graph")?;

        // Filter to edges that go from `from` repo → `to` repo
        let mut edges: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.from_repo_id == from_id && e.to_repo_id == to_id)
            .collect();

        if edges.is_empty() {
            return Ok(format!(
                "No dependencies found: '{from_name}' does not use any files from '{to_name}'."
            ));
        }

        // Sort by target file then source file for readable output
        edges.sort_by(|a, b| a.to_file.cmp(&b.to_file).then(a.from_file.cmp(&b.from_file)));

        // Group by target file
        let mut out = format!(
            "Files in '{}' that use files from '{}':\n\n",
            from_name, to_name
        );

        let mut current_target = String::new();
        for e in &edges {
            if e.to_file != current_target {
                current_target = e.to_file.clone();
                out.push_str(&format!("  {}\n", e.to_file));
            }
            out.push_str(&format!("    ← {}\n", e.from_file));
        }

        let unique_sources: std::collections::HashSet<_> =
            edges.iter().map(|e| &e.from_file).collect();
        let unique_targets: std::collections::HashSet<_> =
            edges.iter().map(|e| &e.to_file).collect();

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
