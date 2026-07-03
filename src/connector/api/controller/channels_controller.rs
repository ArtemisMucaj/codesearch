use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::application::{ChannelLinkOptions, ChannelLinkReport};
use crate::cli::OutputFormatTextJson;
use crate::connector::api::container::Container;
use crate::domain::{ChannelEndpoint, Protocol};

pub struct ChannelsController<'a> {
    container: &'a Container,
}

impl<'a> ChannelsController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn channels(
        &self,
        repositories: Option<Vec<String>>,
        protocol: Option<String>,
        unmatched_only: bool,
        min_confidence: Option<f32>,
        exclude_channels: Vec<String>,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let protocol = match protocol {
            Some(p) => Some(Protocol::parse(&p).with_context(|| {
                format!("Unknown protocol '{p}' (expected kafka, http, mqtt, amqp, or grpc)")
            })?),
            None => None,
        };

        // Resolve repository names/IDs and build an id → name map for output.
        let all_repos = self
            .container
            .list_use_case()
            .execute()
            .await
            .context("Failed to list repositories")?;
        let repo_names: HashMap<String, String> = all_repos
            .iter()
            .map(|r| (r.id().to_string(), r.name().to_string()))
            .collect();

        let repository_ids: Option<Vec<String>> = match repositories {
            Some(keys) => {
                let mut ids = Vec::new();
                for key in keys {
                    let id = all_repos
                        .iter()
                        .find(|r| r.id() == key)
                        .or_else(|| {
                            all_repos
                                .iter()
                                .find(|r| r.name().eq_ignore_ascii_case(&key))
                        })
                        .map(|r| r.id().to_string())
                        .with_context(|| format!("Repository not found: '{key}'"))?;
                    ids.push(id);
                }
                Some(ids)
            }
            None => None,
        };

        let options = ChannelLinkOptions {
            protocol,
            min_confidence,
            exclude_channels,
        };
        let report = self
            .container
            .channel_link_use_case()
            .link(repository_ids.as_deref(), &options)
            .await
            .context("Failed to compute channel links")?;

        match format {
            OutputFormatTextJson::Json => {
                let mut report = report;
                // Mirror the text path: `--unmatched` drops matched edges and
                // fan-out noise so JSON output stays consistent with the flag.
                if unmatched_only {
                    report.edges.clear();
                    report.noisy_channels.clear();
                }
                serde_json::to_string_pretty(&report).context("Failed to serialize report")
            }
            OutputFormatTextJson::Text => Ok(render_text(&report, &repo_names, unmatched_only)),
        }
    }
}

fn repo_label<'a>(repo_names: &'a HashMap<String, String>, id: &'a str) -> &'a str {
    repo_names.get(id).map(String::as_str).unwrap_or(id)
}

fn endpoint_line(endpoint: &ChannelEndpoint, repo_names: &HashMap<String, String>) -> String {
    let symbol = endpoint
        .enclosing_symbol()
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    let marker = if endpoint.is_resolved() {
        String::new()
    } else {
        " [unresolved]".to_string()
    };
    format!(
        "{}: {}{} [conf {:.2}]{}",
        repo_label(repo_names, endpoint.repository_id()),
        endpoint.location(),
        symbol,
        endpoint.confidence(),
        marker,
    )
}

fn render_text(
    report: &ChannelLinkReport,
    repo_names: &HashMap<String, String>,
    unmatched_only: bool,
) -> String {
    let mut out = String::new();

    if !unmatched_only {
        if report.edges.is_empty() {
            out.push_str("No matched channels.\n");
        } else {
            out.push_str("Matched channels:\n");
            let mut current_channel = String::new();
            for edge in &report.edges {
                let header = format!("{}:{}", edge.protocol(), edge.channel());
                if header != current_channel {
                    out.push_str(&format!("\n  {header}\n"));
                    current_channel = header;
                }
                out.push_str(&format!(
                    "    {} ──▶ {}  (weight {}, conf {:.2})\n",
                    endpoint_line(&edge.producer, repo_names),
                    endpoint_line(&edge.consumer, repo_names),
                    edge.weight,
                    edge.confidence,
                ));
            }
        }

        if !report.noisy_channels.is_empty() {
            out.push_str(&format!(
                "\n⚠ High fan-out channels (consider --exclude-channel): {}\n",
                report.noisy_channels.join(", ")
            ));
        }
        out.push('\n');
    }

    if report.unmatched_producers.is_empty() && report.unmatched_consumers.is_empty() {
        out.push_str("No unmatched endpoints.");
        return out;
    }

    if !report.unmatched_producers.is_empty() {
        out.push_str("Unmatched producers:\n");
        for endpoint in &report.unmatched_producers {
            out.push_str(&format!(
                "  {}:{} ← {}\n",
                endpoint.protocol(),
                endpoint.channel_raw(),
                endpoint_line(endpoint, repo_names),
            ));
        }
    }
    if !report.unmatched_consumers.is_empty() {
        if !report.unmatched_producers.is_empty() {
            out.push('\n');
        }
        out.push_str("Unmatched consumers:\n");
        for endpoint in &report.unmatched_consumers {
            out.push_str(&format!(
                "  {}:{} ← {}\n",
                endpoint.protocol(),
                endpoint.channel_raw(),
                endpoint_line(endpoint, repo_names),
            ));
        }
    }

    out.trim_end().to_string()
}
