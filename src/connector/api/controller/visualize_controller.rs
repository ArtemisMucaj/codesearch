use anyhow::{Context, Result};

use crate::application::{aggregate, render, VizFormat as RenderFormat, DEFAULT_NODE_LIMIT};
use crate::cli::{VizFormat, VizLevel};
use crate::domain::GraphView;

use super::super::Container;

/// CLI controller for the `visualize` command — renders Leiden communities to an
/// HTML / SVG / GraphML / Obsidian-canvas artifact on disk.
pub struct VisualizeController<'a> {
    container: &'a Container,
}

impl<'a> VisualizeController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn visualize(
        &self,
        repository: Option<String>,
        global: bool,
        level: VizLevel,
        format: VizFormat,
        output: Option<String>,
        force_aggregate: bool,
        node_limit: usize,
    ) -> Result<String> {
        // 1. Build the render-ready graph from the requested scope and level.
        let (scope, view): (String, GraphView) = if global {
            let view = match level {
                VizLevel::File => self
                    .container
                    .cluster_detection_use_case()
                    .namespace_graph_view(None)
                    .await
                    .context("building namespace-wide file graph view")?,
                VizLevel::Symbol => self
                    .container
                    .symbol_cluster_detection_use_case()
                    .namespace_graph_view(None)
                    .await
                    .context("building namespace-wide symbol graph view")?,
            };
            ("namespace (all repositories)".to_string(), view)
        } else {
            let repository_id = self
                .container
                .resolve_repository_id(repository.as_deref())
                .await;
            let view = match level {
                VizLevel::File => self
                    .container
                    .cluster_detection_use_case()
                    .graph_view(&repository_id)
                    .await
                    .context("building file-level graph view")?,
                VizLevel::Symbol => self
                    .container
                    .symbol_cluster_detection_use_case()
                    .graph_view(&repository_id)
                    .await
                    .context("building symbol-level graph view")?,
            };
            (repository_id, view)
        };

        if view.nodes.is_empty() {
            return Ok(format!(
                "No graph to visualize for `{}` \
                 (nothing indexed, or the call graph is empty).",
                scope
            ));
        }

        // 2. Aggregate to a community meta-graph when asked, or when the graph is
        //    too large to render node-for-node.
        let effective_limit = if node_limit == 0 {
            DEFAULT_NODE_LIMIT
        } else {
            node_limit
        };
        let aggregated = force_aggregate || view.node_count() > effective_limit;
        let view = if aggregated { aggregate(&view) } else { view };

        // 3. Render.
        let render_format = match format {
            VizFormat::Html => RenderFormat::Html,
            VizFormat::Svg => RenderFormat::Svg,
            VizFormat::Canvas => RenderFormat::Canvas,
        };
        let contents = render(&view, render_format);

        // 4. Write to disk (file I/O off the async runtime).
        let path =
            output.unwrap_or_else(|| format!("codesearch-graph.{}", render_format.extension()));
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || std::fs::write(&write_path, contents))
            .await
            .context("join file-write task")?
            .with_context(|| format!("writing visualization to {}", path))?;

        let note = if aggregated {
            " (aggregated community meta-graph)"
        } else {
            ""
        };
        Ok(format!(
            "Wrote {} {}-level visualization to {}{} — {} nodes, {} edges, {} communities.",
            render_format.extension(),
            level_noun(level),
            path,
            note,
            view.node_count(),
            view.edge_count(),
            view.communities.len(),
        ))
    }
}

fn level_noun(level: VizLevel) -> &'static str {
    match level {
        VizLevel::File => "file",
        VizLevel::Symbol => "symbol",
    }
}
