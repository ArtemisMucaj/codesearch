use anyhow::Result;

use crate::cli::OutputFormat;
use crate::domain::ExecutionFeature;

use super::super::Container;

pub struct ExecutionFeaturesController<'a> {
    container: &'a Container,
}

impl<'a> ExecutionFeaturesController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// List entry-point features for `repository_id`, sorted by descending criticality.
    pub async fn list(
        &self,
        repository: String,
        limit: usize,
        format: OutputFormat,
    ) -> Result<String> {
        let use_case = self.container.execution_features_use_case();
        let features = use_case.list_features(&repository, limit).await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&features)?,
            OutputFormat::Vimgrep => Self::format_list_vimgrep(&features),
            OutputFormat::Text => Self::format_list_text(&features),
        })
    }

    /// Retrieve a single feature by entry-point symbol name.
    pub async fn get(
        &self,
        symbol: String,
        repository: Option<String>,
        format: OutputFormat,
    ) -> Result<String> {
        let use_case = self.container.execution_features_use_case();
        let result = use_case
            .get_feature(&symbol, repository.as_deref())
            .await?;

        match result {
            None => Ok(match format {
                OutputFormat::Json => "null".to_string(),
                OutputFormat::Vimgrep => String::new(),
                OutputFormat::Text => format!("No entry-point feature found for '{symbol}'."),
            }),
            Some(feature) => Ok(match format {
                OutputFormat::Json => serde_json::to_string_pretty(&feature)?,
                OutputFormat::Vimgrep => Self::format_feature_vimgrep(&feature),
                OutputFormat::Text => Self::format_feature_text(&feature),
            }),
        }
    }

    /// Show features impacted by a set of changed symbols.
    pub async fn impacted(
        &self,
        symbols: Vec<String>,
        repository: Option<String>,
        format: OutputFormat,
    ) -> Result<String> {
        let use_case = self.container.execution_features_use_case();
        let features = use_case
            .get_impacted_features(&symbols, repository.as_deref())
            .await?;

        Ok(match format {
            OutputFormat::Json => serde_json::to_string_pretty(&features)?,
            OutputFormat::Vimgrep => Self::format_list_vimgrep(&features),
            OutputFormat::Text => {
                if features.is_empty() {
                    format!(
                        "No features impacted by the provided symbol(s): {}",
                        symbols.join(", ")
                    )
                } else {
                    Self::format_list_text(&features)
                }
            }
        })
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Formatters
    // ──────────────────────────────────────────────────────────────────────────

    fn format_list_text(features: &[ExecutionFeature]) -> String {
        if features.is_empty() {
            return "No execution features found.".to_string();
        }

        let mut out = format!(
            "Execution Features ({} total)\n\
             ─────────────────────────────────────────\n",
            features.len()
        );

        for feature in features {
            out.push_str(&format!(
                "{name}  criticality={crit:.2}  depth={depth}  files={files}\n  entry: {ep}\n\n",
                name = feature.name,
                crit = feature.criticality,
                depth = feature.depth,
                files = feature.file_count,
                ep = feature.entry_point,
            ));
        }

        out
    }

    fn format_list_vimgrep(features: &[ExecutionFeature]) -> String {
        features
            .iter()
            .filter_map(|f| f.path.first())
            .map(|node| {
                format!(
                    "{}:{}:1:{}",
                    node.file_path, node.line, node.symbol
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn format_feature_text(feature: &ExecutionFeature) -> String {
        let mut out = format!(
            "Execution Feature: {name}\n\
             ─────────────────────────────────────────\n\
             Entry point : {ep}\n\
             Repository  : {repo}\n\
             Criticality : {crit:.2}\n\
             Depth       : {depth}\n\
             Files       : {files}\n\
             \n\
             Call chain:\n",
            name = feature.name,
            ep = feature.entry_point,
            repo = feature.repository_id,
            crit = feature.criticality,
            depth = feature.depth,
            files = feature.file_count,
        );

        for node in &feature.path {
            let indent = "    ".repeat(node.depth);
            if node.depth == 0 {
                out.push_str(&format!("{}\n", node.symbol));
            } else {
                out.push_str(&format!(
                    "{}└── {} [{}:{}]\n",
                    indent, node.symbol, node.file_path, node.line
                ));
            }
        }

        out
    }

    fn format_feature_vimgrep(feature: &ExecutionFeature) -> String {
        feature
            .path
            .iter()
            .map(|node| {
                format!(
                    "{}:{}:1:{}",
                    node.file_path, node.line, node.symbol
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
