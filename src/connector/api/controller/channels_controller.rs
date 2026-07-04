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
        include_tests: bool,
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
            include_tests,
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

/// A `repo: file:line (symbol) [conf] [unresolved]` description of one endpoint.
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

/// The channel identifier as a human label: `POST /v1/read/{}` for HTTP (verb
/// from the stored method, `ANY` when absent), `Kafka orders.created` for
/// messaging protocols. The `protocol:` prefix of the old format is dropped —
/// the verb / protocol name already carries it.
///
/// A channel recovered from config, or confirmed against a client library,
/// carries a trailing `(env: X, via @backend/kafkajs)` annotation.
fn channel_label(endpoint: &ChannelEndpoint) -> String {
    let channel = channel_display(endpoint);
    let mut base = match endpoint.protocol() {
        Protocol::Http => {
            let verb = endpoint.method().unwrap_or("ANY");
            format!("{verb} {channel}")
        }
        protocol => format!("{} {channel}", protocol_name(protocol)),
    };
    // A wildcard/pattern channel (MQTT `+`/`#`, or an inferred template
    // pattern) is flagged so it reads as a pattern, not a concrete topic.
    if endpoint.is_pattern() {
        base.push_str(" [pattern]");
    }
    match resolution_note(endpoint) {
        Some(note) => format!("{base}  ({note})"),
        None => base,
    }
}

/// The channel string to show. A resolved channel is shown verbatim; an
/// unresolved property-access expression (`this.config.broker.topics.orders`)
/// is shortened to its trailing segment (`orders`) so the report stays
/// readable — the full path is only needed internally, for resolution.
fn channel_display(endpoint: &ChannelEndpoint) -> &str {
    let raw = endpoint.channel_raw();
    if endpoint.is_resolved() {
        return raw;
    }
    match raw.rsplit(['.', ':']).next() {
        Some(tail) if !tail.is_empty() => tail,
        _ => raw,
    }
}

/// The `env: X, via Y` annotation for a resolved/confirmed endpoint, or `None`
/// when neither the overriding env var nor the confirming library is known.
fn resolution_note(endpoint: &ChannelEndpoint) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(env) = endpoint.env_var() {
        parts.push(format!("env: {env}"));
    }
    if let Some(library) = endpoint.library() {
        parts.push(format!("via {library}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Display name for a messaging protocol: `Kafka`, `MQTT`, `AMQP`, `gRPC`.
fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Kafka => "Kafka",
        Protocol::Mqtt => "MQTT",
        Protocol::Amqp => "AMQP",
        Protocol::Grpc => "gRPC",
        Protocol::Http => "HTTP",
    }
}

/// A channel bucketed into one of the four output sections. `matches` holds the
/// counterpart endpoints this one links to (empty for a dangling endpoint).
struct Row<'a> {
    endpoint: &'a ChannelEndpoint,
    matches: Vec<&'a ChannelEndpoint>,
}

/// How a section renders the arrow to a counterpart.
struct Section<'a> {
    title: &'a str,
    /// Preposition shown on the row's arrow to a counterpart: "from" for a
    /// receiving section (the counterpart is upstream), "by" for a sending one
    /// (downstream). Empty sections are skipped.
    link_verb: &'a str,
    rows: Vec<Row<'a>>,
}

/// The four sections, in output order.
///
/// HTTP splits by role (server = consumer, client = producer); everything else
/// (Kafka, MQTT, AMQP, gRPC) is "messaging" and splits the same way. Matched
/// edges appear in both the producer-side and consumer-side sections, mirrored,
/// so each section is self-contained.
fn build_sections(report: &ChannelLinkReport) -> Vec<Section<'_>> {
    let is_http = |e: &ChannelEndpoint| e.protocol() == Protocol::Http;

    // Counterparts keyed by endpoint id: an HTTP client's servers, a producer's
    // consumers, and vice versa. One edge feeds both directions.
    let mut producers_of: HashMap<&str, Vec<&ChannelEndpoint>> = HashMap::new();
    let mut consumers_of: HashMap<&str, Vec<&ChannelEndpoint>> = HashMap::new();
    for edge in &report.edges {
        consumers_of
            .entry(edge.producer.id())
            .or_default()
            .push(&edge.consumer);
        producers_of
            .entry(edge.consumer.id())
            .or_default()
            .push(&edge.producer);
    }

    // Distinct endpoints per section. An edge's representative endpoint may
    // recur across edges (fan-out), so dedup by id while preserving order.
    let mut http_servers = RowSet::default();
    let mut http_clients = RowSet::default();
    let mut messaging_consumers = RowSet::default();
    let mut messaging_producers = RowSet::default();

    for edge in &report.edges {
        if is_http(&edge.producer) {
            push_row(&mut http_servers, &edge.consumer, &producers_of);
            push_row(&mut http_clients, &edge.producer, &consumers_of);
        } else {
            push_row(&mut messaging_consumers, &edge.consumer, &producers_of);
            push_row(&mut messaging_producers, &edge.producer, &consumers_of);
        }
    }
    for endpoint in &report.unmatched_consumers {
        let set = if is_http(endpoint) {
            &mut http_servers
        } else {
            &mut messaging_consumers
        };
        push_row(set, endpoint, &producers_of);
    }
    for endpoint in &report.unmatched_producers {
        let set = if is_http(endpoint) {
            &mut http_clients
        } else {
            &mut messaging_producers
        };
        push_row(set, endpoint, &consumers_of);
    }

    vec![
        // Receiving sections (server/consumer) link upstream → "from"; sending
        // sections (client/producer) link downstream → "by".
        Section {
            title: "HTTP servers",
            link_verb: "from",
            rows: http_servers.into_rows(),
        },
        Section {
            title: "HTTP clients",
            link_verb: "by",
            rows: http_clients.into_rows(),
        },
        Section {
            title: "Messaging consumers",
            link_verb: "from",
            rows: messaging_consumers.into_rows(),
        },
        Section {
            title: "Messaging producers",
            link_verb: "by",
            rows: messaging_producers.into_rows(),
        },
    ]
}

/// Add `endpoint` to `set`, attaching its counterparts (looked up by id).
fn push_row<'a>(
    set: &mut RowSet<'a>,
    endpoint: &'a ChannelEndpoint,
    counterparts: &HashMap<&'a str, Vec<&'a ChannelEndpoint>>,
) {
    let matches = counterparts.get(endpoint.id()).cloned().unwrap_or_default();
    set.push(endpoint, matches);
}

/// Accumulates rows while deduplicating endpoints by id (fan-out edges reuse a
/// representative endpoint across several edges).
#[derive(Default)]
struct RowSet<'a> {
    rows: Vec<Row<'a>>,
    seen: std::collections::HashSet<&'a str>,
}

impl<'a> RowSet<'a> {
    fn push(&mut self, endpoint: &'a ChannelEndpoint, matches: Vec<&'a ChannelEndpoint>) {
        if !self.seen.insert(endpoint.id()) {
            return;
        }
        self.rows.push(Row { endpoint, matches });
    }

    fn into_rows(mut self) -> Vec<Row<'a>> {
        self.rows.sort_by(|a, b| {
            a.endpoint
                .protocol()
                .as_str()
                .cmp(b.endpoint.protocol().as_str())
                .then_with(|| a.endpoint.channel_raw().cmp(b.endpoint.channel_raw()))
                .then_with(|| a.endpoint.repository_id().cmp(b.endpoint.repository_id()))
                .then_with(|| a.endpoint.location().cmp(&b.endpoint.location()))
        });
        self.rows
    }
}

fn render_text(
    report: &ChannelLinkReport,
    repo_names: &HashMap<String, String>,
    unmatched_only: bool,
) -> String {
    let sections = build_sections(report);
    let mut out = String::new();
    let mut first = true;

    for section in &sections {
        // With `--unmatched`, keep only dangling endpoints (no counterpart).
        let rows: Vec<&Row> = section
            .rows
            .iter()
            .filter(|r| !unmatched_only || r.matches.is_empty())
            .collect();
        if rows.is_empty() {
            continue;
        }

        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(&format!("{}:\n", section.title));

        // Rows are pre-sorted by channel, so same-channel sites are adjacent.
        // Print the channel label once as a header, then each call site (and
        // its matched counterparts) indented beneath it.
        let mut current_channel: Option<String> = None;
        for row in rows {
            let label = channel_label(row.endpoint);
            if current_channel.as_deref() != Some(label.as_str()) {
                out.push_str(&format!("  {label}\n"));
                current_channel = Some(label);
            }
            out.push_str(&format!(
                "    ← {}\n",
                endpoint_line(row.endpoint, repo_names),
            ));
            if !unmatched_only {
                for counterpart in &row.matches {
                    out.push_str(&format!(
                        "        └── {} {}\n",
                        section.link_verb,
                        endpoint_line(counterpart, repo_names),
                    ));
                }
            }
        }
    }

    if out.is_empty() {
        return "No channel endpoints found.".to_string();
    }

    if !report.noisy_channels.is_empty() {
        out.push_str(&format!(
            "\n⚠ High fan-out channels (consider --exclude-channel): {}\n",
            report.noisy_channels.join(", ")
        ));
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ChannelEdge, ChannelRole, EndpointSource};

    fn endpoint(
        repo: &str,
        file: &str,
        line: u32,
        protocol: Protocol,
        role: ChannelRole,
        channel: &str,
    ) -> ChannelEndpoint {
        ChannelEndpoint::new(
            repo.to_string(),
            file.to_string(),
            line,
            protocol,
            role,
            channel.to_string(),
            channel.to_string(),
            0.8,
            EndpointSource::TreeSitter,
        )
    }

    fn edge(producer: ChannelEndpoint, consumer: ChannelEndpoint) -> ChannelEdge {
        let confidence = producer.confidence().min(consumer.confidence());
        ChannelEdge {
            producer,
            consumer,
            weight: 1,
            confidence,
        }
    }

    /// A config-resolved, library-confirmed endpoint shows its env var and
    /// library in the channel label.
    #[test]
    fn resolved_endpoint_shows_env_and_library() {
        let resolved = endpoint(
            "engine",
            "app.ts",
            40,
            Protocol::Kafka,
            ChannelRole::Consumer,
            "topology_event",
        )
        .with_env_var("KAFKA_TOPOLOGY_EVENT_TOPIC")
        .with_library("@backend/kafkajs");

        let report = ChannelLinkReport {
            edges: vec![],
            unmatched_producers: vec![],
            unmatched_consumers: vec![resolved],
            noisy_channels: vec![],
        };
        let out = render_text(&report, &HashMap::new(), false);
        assert!(out.contains(
            "Kafka topology_event  (env: KAFKA_TOPOLOGY_EVENT_TOPIC, via @backend/kafkajs)"
        ));
    }

    /// An unresolved property-access channel is shown by its trailing segment,
    /// not the full expression stored for resolution.
    #[test]
    fn unresolved_property_path_is_shortened_for_display() {
        let unresolved = endpoint(
            "engine",
            "app.ts",
            93,
            Protocol::Kafka,
            ChannelRole::Consumer,
            "this.config.broker.topics.topologyEvent",
        )
        .unresolved();

        let report = ChannelLinkReport {
            edges: vec![],
            unmatched_producers: vec![],
            unmatched_consumers: vec![unresolved],
            noisy_channels: vec![],
        };
        let out = render_text(&report, &HashMap::new(), false);
        assert!(out.contains("Kafka topologyEvent"));
        assert!(!out.contains("this.config.broker"));
    }

    /// A matched HTTP pair and a matched Kafka pair render into the four
    /// role-split sections, each showing the counterpart via an arrow line.
    #[test]
    fn matched_pairs_render_in_four_sections_with_arrows() {
        let http_client = endpoint(
            "gateway",
            "client.ts",
            10,
            Protocol::Http,
            ChannelRole::Producer,
            "/v1/read/{}",
        )
        .with_method("get")
        .with_enclosing_symbol("readNode");
        let http_server = endpoint(
            "engine",
            "router.ts",
            23,
            Protocol::Http,
            ChannelRole::Consumer,
            "/v1/read/{}",
        )
        .with_method("post")
        .with_enclosing_symbol("controllerRouter");
        let kafka_producer = endpoint(
            "svc-a",
            "checkout.ts",
            12,
            Protocol::Kafka,
            ChannelRole::Producer,
            "orders.created",
        )
        .with_enclosing_symbol("checkout");
        let kafka_consumer = endpoint(
            "svc-b",
            "worker.ts",
            30,
            Protocol::Kafka,
            ChannelRole::Consumer,
            "orders.created",
        )
        .with_enclosing_symbol("onOrder");

        let report = ChannelLinkReport {
            edges: vec![
                edge(http_client, http_server),
                edge(kafka_producer, kafka_consumer),
            ],
            unmatched_producers: vec![],
            unmatched_consumers: vec![],
            noisy_channels: vec![],
        };

        let out = render_text(&report, &HashMap::new(), false);

        // All four sections present, in order.
        let servers = out.find("HTTP servers:").unwrap();
        let clients = out.find("HTTP clients:").unwrap();
        let consumers = out.find("Messaging consumers:").unwrap();
        let producers = out.find("Messaging producers:").unwrap();
        assert!(servers < clients && clients < consumers && consumers < producers);

        // Channel is a header line; each call site is indented beneath it.
        // Server links upstream ("from"), client links downstream ("by").
        assert!(out.contains("POST /v1/read/{}\n"));
        assert!(out.contains("← engine: router.ts:23 (controllerRouter)"));
        assert!(out.contains("└── from gateway: client.ts:10 (readNode)"));
        assert!(out.contains("← gateway: client.ts:10 (readNode)"));
        assert!(out.contains("└── by engine: router.ts:23 (controllerRouter)"));

        // Messaging uses the protocol name as the prefix.
        assert!(out.contains("Kafka orders.created\n"));
        assert!(out.contains("← svc-a: checkout.ts:12 (checkout)"));
        assert!(out.contains("← svc-b: worker.ts:30 (onOrder)"));
        assert!(out.contains("└── by svc-b: worker.ts:30 (onOrder)"));
        assert!(out.contains("└── from svc-a: checkout.ts:12 (checkout)"));
    }

    /// A dangling unresolved producer renders as a plain line with no arrow,
    /// and `--unmatched` keeps only such danglers.
    #[test]
    fn dangling_endpoints_render_plainly_and_survive_unmatched_filter() {
        let dangling = endpoint(
            "engine",
            "broker.ts",
            102,
            Protocol::Mqtt,
            ChannelRole::Producer,
            "requestTopic",
        )
        .with_enclosing_symbol("publish")
        .unresolved();

        let report = ChannelLinkReport {
            edges: vec![],
            unmatched_producers: vec![dangling],
            unmatched_consumers: vec![],
            noisy_channels: vec![],
        };

        let full = render_text(&report, &HashMap::new(), false);
        assert!(full.contains("Messaging producers:"));
        // Channel header, then the indented call site.
        assert!(full.contains("MQTT requestTopic\n"));
        assert!(
            full.contains("← engine: broker.ts:102 (publish) [conf 0.80] [unresolved]")
        );
        assert!(!full.contains("└──"), "dangling endpoint must have no arrow");

        // `--unmatched` keeps it (no counterpart) and still draws no arrow.
        let unmatched = render_text(&report, &HashMap::new(), true);
        assert!(unmatched.contains("MQTT requestTopic"));
        assert!(!unmatched.contains("└──"));
    }

    #[test]
    fn empty_report_reports_nothing_found() {
        let report = ChannelLinkReport {
            edges: vec![],
            unmatched_producers: vec![],
            unmatched_consumers: vec![],
            noisy_channels: vec![],
        };
        assert_eq!(
            render_text(&report, &HashMap::new(), false),
            "No channel endpoints found."
        );
    }

    /// Two call sites on the same channel share one header, listed beneath it.
    #[test]
    fn same_channel_sites_are_grouped_under_one_header() {
        let site_a = endpoint(
            "engine",
            "broker.ts",
            102,
            Protocol::Mqtt,
            ChannelRole::Producer,
            "+/request",
        )
        .with_enclosing_symbol("publish")
        .as_pattern();
        let site_b = endpoint(
            "engine",
            "interaction-model.ts",
            27,
            Protocol::Mqtt,
            ChannelRole::Producer,
            "+/request",
        )
        .with_enclosing_symbol("request")
        .as_pattern();

        let report = ChannelLinkReport {
            edges: vec![],
            unmatched_producers: vec![site_a, site_b],
            unmatched_consumers: vec![],
            noisy_channels: vec![],
        };
        let out = render_text(&report, &HashMap::new(), false);

        // The channel label appears exactly once as a header…
        assert_eq!(out.matches("MQTT +/request [pattern]").count(), 1);
        // …with both call sites listed beneath it.
        assert!(out.contains("← engine: broker.ts:102 (publish)"));
        assert!(out.contains("← engine: interaction-model.ts:27 (request)"));
    }
}
