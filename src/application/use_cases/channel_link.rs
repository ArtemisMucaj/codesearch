use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::application::ChannelEndpointRepository;
use crate::domain::{ChannelEdge, ChannelEndpoint, ChannelRole, DomainError, Protocol};

/// A channel matched by more edges than this is flagged as noisy in the
/// report (e.g. `/health` hit by every service). The edges are still
/// returned; the flag lets consumers and the CLI warn or exclude.
const FAN_OUT_WARNING_THRESHOLD: usize = 20;

/// Options for a channel-link query.
#[derive(Debug, Clone, Default)]
pub struct ChannelLinkOptions {
    /// Only consider endpoints of this protocol.
    pub protocol: Option<Protocol>,
    /// Drop edges whose confidence is below this threshold.
    pub min_confidence: Option<f32>,
    /// Glob patterns (`*`, `?`) excluding channels from matching and output,
    /// e.g. `/health*`.
    pub exclude_channels: Vec<String>,
}

/// Result of joining producers and consumers on their channels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelLinkReport {
    /// Matched producer→consumer edges, collapsed per (file-pair, channel).
    pub edges: Vec<ChannelEdge>,
    /// Producer endpoints with no consumer on their channel (dangling), plus
    /// unresolved producer endpoints.
    pub unmatched_producers: Vec<ChannelEndpoint>,
    /// Consumer endpoints with no producer on their channel (dangling), plus
    /// unresolved consumer endpoints.
    pub unmatched_consumers: Vec<ChannelEndpoint>,
    /// `protocol:channel` identifiers whose edge count exceeded the fan-out
    /// threshold — probably generic channels like `/health`.
    pub noisy_channels: Vec<String>,
}

/// Joins stored channel endpoints into cross-service edges at query time.
///
/// Edges are never materialized: endpoint counts are small, matching is an
/// in-memory hash join, and query-time computation means re-indexing a
/// repository can never leave stale edges behind.
pub struct ChannelLinkUseCase {
    endpoint_repo: Arc<dyn ChannelEndpointRepository>,
}

impl ChannelLinkUseCase {
    pub fn new(endpoint_repo: Arc<dyn ChannelEndpointRepository>) -> Self {
        Self { endpoint_repo }
    }

    /// Compute the channel-link report for the given repositories (or the
    /// whole namespace when `repository_ids` is `None`).
    pub async fn link(
        &self,
        repository_ids: Option<&[String]>,
        options: &ChannelLinkOptions,
    ) -> Result<ChannelLinkReport, DomainError> {
        let mut endpoints = match repository_ids {
            Some(ids) => {
                let mut all = Vec::new();
                for id in ids {
                    all.extend(self.endpoint_repo.find_by_repository(id).await?);
                }
                all
            }
            None => match options.protocol {
                Some(protocol) => self.endpoint_repo.find_by_protocol(protocol).await?,
                None => self.endpoint_repo.find_all().await?,
            },
        };

        if let Some(protocol) = options.protocol {
            endpoints.retain(|e| e.protocol() == protocol);
        }
        endpoints.retain(|e| {
            !options.exclude_channels.iter().any(|pattern| {
                glob_match(pattern, e.channel_normalized()) || glob_match(pattern, e.channel_raw())
            })
        });

        debug!("Matching {} channel endpoints", endpoints.len());
        Ok(build_report(endpoints, options))
    }
}

/// Pure matching core, factored out of the use case for table-testing.
fn build_report(
    endpoints: Vec<ChannelEndpoint>,
    options: &ChannelLinkOptions,
) -> ChannelLinkReport {
    // Pair up resolved producers and consumers per protocol.
    let mut pairs: Vec<(&ChannelEndpoint, &ChannelEndpoint)> = Vec::new();
    let mut by_protocol: HashMap<Protocol, (Vec<&ChannelEndpoint>, Vec<&ChannelEndpoint>)> =
        HashMap::new();
    for endpoint in endpoints.iter().filter(|e| e.is_resolved()) {
        let entry = by_protocol.entry(endpoint.protocol()).or_default();
        match endpoint.role() {
            ChannelRole::Producer => entry.0.push(endpoint),
            ChannelRole::Consumer => entry.1.push(endpoint),
        }
    }

    for (protocol, (producers, consumers)) in &by_protocol {
        match protocol {
            // HTTP: template-aware segment walk (`{}` matches any one concrete
            // segment on either side), O(producers × consumers) but bounded.
            Protocol::Http => {
                for producer in producers {
                    for consumer in consumers {
                        if http_route_matches(
                            producer.channel_normalized(),
                            consumer.channel_normalized(),
                        ) {
                            pairs.push((producer, consumer));
                        }
                    }
                }
            }
            // Everything else: exact hash join, plus a wildcard pass for
            // pattern subscriptions (MQTT `+`/`#`).
            _ => {
                let mut exact: HashMap<&str, Vec<&ChannelEndpoint>> = HashMap::new();
                for producer in producers {
                    exact
                        .entry(producer.channel_normalized())
                        .or_default()
                        .push(producer);
                }
                for consumer in consumers {
                    if consumer.is_pattern() {
                        for producer in producers {
                            if !producer.is_pattern()
                                && mqtt_topic_matches(
                                    consumer.channel_normalized(),
                                    producer.channel_normalized(),
                                )
                            {
                                pairs.push((producer, consumer));
                            }
                        }
                    } else if let Some(matched) = exact.get(consumer.channel_normalized()) {
                        for producer in matched {
                            pairs.push((producer, consumer));
                        }
                    }
                }
            }
        }
    }

    // Collapse pairs per (producer file, consumer file, channel).
    type EdgeKey = (String, String, String, String, String);
    let mut grouped: HashMap<EdgeKey, (Vec<&ChannelEndpoint>, Vec<&ChannelEndpoint>)> =
        HashMap::new();
    let mut matched_ids: HashSet<String> = HashSet::new();
    for (producer, consumer) in &pairs {
        matched_ids.insert(producer.id().to_string());
        matched_ids.insert(consumer.id().to_string());
        let key = (
            producer.repository_id().to_string(),
            producer.file_path().to_string(),
            consumer.repository_id().to_string(),
            consumer.file_path().to_string(),
            consumer.channel_normalized().to_string(),
        );
        let entry = grouped.entry(key).or_default();
        entry.0.push(producer);
        entry.1.push(consumer);
    }

    let mut edges: Vec<ChannelEdge> = grouped
        .into_values()
        .map(|(mut producer_sites, mut consumer_sites)| {
            // Representative site: highest confidence, then lowest line —
            // deterministic regardless of input order.
            let best = |sites: &mut Vec<&ChannelEndpoint>| {
                sites.sort_by(|a, b| {
                    b.confidence()
                        .total_cmp(&a.confidence())
                        .then(a.line().cmp(&b.line()))
                });
                sites.dedup_by_key(|e| e.id().to_string());
                sites[0].clone()
            };
            let distinct = |sites: &[&ChannelEndpoint]| {
                sites.iter().map(|e| e.id()).collect::<HashSet<_>>().len()
            };
            let weight = distinct(&producer_sites) * distinct(&consumer_sites);
            let producer = best(&mut producer_sites);
            let consumer = best(&mut consumer_sites);
            let confidence = producer.confidence().min(consumer.confidence());
            ChannelEdge {
                producer,
                consumer,
                weight,
                confidence,
            }
        })
        .collect();

    if let Some(min_confidence) = options.min_confidence {
        edges.retain(|e| e.confidence >= min_confidence);
    }

    edges.sort_by(|a, b| {
        a.protocol()
            .as_str()
            .cmp(b.protocol().as_str())
            .then_with(|| a.channel().cmp(b.channel()))
            .then_with(|| a.producer.repository_id().cmp(b.producer.repository_id()))
            .then_with(|| a.producer.file_path().cmp(b.producer.file_path()))
            .then_with(|| a.consumer.repository_id().cmp(b.consumer.repository_id()))
            .then_with(|| a.consumer.file_path().cmp(b.consumer.file_path()))
    });

    // Fan-out guardrail: flag channels matched by suspiciously many edges.
    let mut per_channel: HashMap<String, usize> = HashMap::new();
    for edge in &edges {
        *per_channel
            .entry(format!("{}:{}", edge.protocol(), edge.channel()))
            .or_default() += 1;
    }
    let mut noisy_channels: Vec<String> = per_channel
        .into_iter()
        .filter(|(_, count)| *count > FAN_OUT_WARNING_THRESHOLD)
        .map(|(channel, _)| channel)
        .collect();
    noisy_channels.sort();

    // Dangling endpoints: everything that participated in no edge. Unresolved
    // endpoints land here by construction, keeping the report honest about
    // what extraction could not see.
    let sort_key = |e: &ChannelEndpoint| {
        (
            e.repository_id().to_string(),
            e.file_path().to_string(),
            e.line(),
        )
    };
    let mut unmatched_producers: Vec<ChannelEndpoint> = Vec::new();
    let mut unmatched_consumers: Vec<ChannelEndpoint> = Vec::new();
    for endpoint in endpoints {
        if matched_ids.contains(endpoint.id()) {
            continue;
        }
        match endpoint.role() {
            ChannelRole::Producer => unmatched_producers.push(endpoint),
            ChannelRole::Consumer => unmatched_consumers.push(endpoint),
        }
    }
    unmatched_producers.sort_by_key(sort_key);
    unmatched_consumers.sort_by_key(sort_key);

    ChannelLinkReport {
        edges,
        unmatched_producers,
        unmatched_consumers,
        noisy_channels,
    }
}

// ── Pure channel normalization / matching helpers ────────────────────────────

/// Split an HTTP client URL into its host and path parts.
///
/// Absolute URLs (`http://orders-svc/api/orders?x=1`) yield
/// `(Some("orders-svc"), "/api/orders?x=1")`; anything else is treated as a
/// bare path with no host.
pub fn split_http_url(raw: &str) -> (Option<String>, String) {
    for scheme in ["http://", "https://"] {
        if let Some(rest) = raw.strip_prefix(scheme) {
            return match rest.find('/') {
                Some(slash) => (Some(rest[..slash].to_string()), rest[slash..].to_string()),
                None => (Some(rest.to_string()), "/".to_string()),
            };
        }
    }
    (None, raw.to_string())
}

/// Normalize an HTTP route or client path to a canonical template.
///
/// Path parameters written as `{id}`, `:id`, `<id>`, or `<int:id>` all become
/// the canonical `{}` segment; query strings, fragments, and trailing slashes
/// are dropped.
pub fn normalize_http_route(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or(path);

    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|segment| {
            let is_param = segment.starts_with(':')
                || (segment.starts_with('{') && segment.ends_with('}'))
                || (segment.starts_with('<') && segment.ends_with('>'));
            if is_param {
                "{}".to_string()
            } else {
                segment.to_string()
            }
        })
        .collect();

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Segment-wise HTTP template match: `{}` on either side matches any single
/// concrete segment (`/users/123` client ↔ `/users/{}` server).
pub fn http_route_matches(client_path: &str, server_template: &str) -> bool {
    let client: Vec<&str> = client_path.split('/').filter(|s| !s.is_empty()).collect();
    let server: Vec<&str> = server_template
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    client.len() == server.len()
        && client
            .iter()
            .zip(&server)
            .all(|(c, s)| c == s || *c == "{}" || *s == "{}")
}

/// Segment-wise MQTT wildcard match: `+` matches exactly one topic segment,
/// `#` matches any suffix (including none).
pub fn mqtt_topic_matches(pattern: &str, topic: &str) -> bool {
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    let topic_segments: Vec<&str> = topic.split('/').collect();

    for (i, pattern_segment) in pattern_segments.iter().enumerate() {
        match *pattern_segment {
            "#" => return true,
            "+" => {
                if i >= topic_segments.len() {
                    return false;
                }
            }
            segment => {
                if topic_segments.get(i) != Some(&segment) {
                    return false;
                }
            }
        }
    }
    pattern_segments.len() == topic_segments.len()
}

/// Minimal glob match supporting `*` (any run of characters, including empty)
/// and `?` (exactly one character). Used by `--exclude-channel`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();

    // Two-pointer match with single-level backtracking to the last `*`.
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut star_t) = (None::<usize>, 0usize);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EndpointSource;

    fn endpoint(
        repo: &str,
        file: &str,
        line: u32,
        protocol: Protocol,
        role: ChannelRole,
        channel: &str,
        confidence: f32,
    ) -> ChannelEndpoint {
        ChannelEndpoint::new(
            repo.to_string(),
            file.to_string(),
            line,
            protocol,
            role,
            channel.to_string(),
            channel.to_string(),
            confidence,
            EndpointSource::TreeSitter,
        )
    }

    #[test]
    fn test_normalize_http_route() {
        assert_eq!(normalize_http_route("/users/{id}"), "/users/{}");
        assert_eq!(normalize_http_route("/users/:id/posts"), "/users/{}/posts");
        assert_eq!(
            normalize_http_route("/api/orders/<order_id>"),
            "/api/orders/{}"
        );
        assert_eq!(
            normalize_http_route("/api/orders/<int:order_id>"),
            "/api/orders/{}"
        );
        assert_eq!(normalize_http_route("/users/123?page=2"), "/users/123");
        assert_eq!(normalize_http_route("/users/"), "/users");
        assert_eq!(normalize_http_route("/"), "/");
        assert_eq!(normalize_http_route(""), "/");
    }

    #[test]
    fn test_split_http_url() {
        assert_eq!(
            split_http_url("http://orders-svc/api/orders?x=1"),
            (
                Some("orders-svc".to_string()),
                "/api/orders?x=1".to_string()
            )
        );
        assert_eq!(
            split_http_url("https://example.com"),
            (Some("example.com".to_string()), "/".to_string())
        );
        assert_eq!(
            split_http_url("/api/orders"),
            (None, "/api/orders".to_string())
        );
    }

    #[test]
    fn test_http_route_matches() {
        assert!(http_route_matches("/users/123", "/users/{}"));
        assert!(http_route_matches("/users/{}", "/users/{}"));
        assert!(http_route_matches("/api/orders/123", "/api/orders/{}"));
        assert!(!http_route_matches("/users/123/posts", "/users/{}"));
        assert!(!http_route_matches("/orders/123", "/users/{}"));
        assert!(http_route_matches("/health", "/health"));
    }

    #[test]
    fn test_mqtt_topic_matches() {
        assert!(mqtt_topic_matches("orders/+", "orders/created"));
        assert!(!mqtt_topic_matches("orders/+", "orders/created/eu"));
        assert!(mqtt_topic_matches("orders/#", "orders/created/eu"));
        assert!(mqtt_topic_matches("orders/#", "orders"));
        assert!(mqtt_topic_matches("orders/+/eu", "orders/created/eu"));
        assert!(!mqtt_topic_matches("orders/+/eu", "orders/created/us"));
        assert!(mqtt_topic_matches("orders/created", "orders/created"));
        assert!(!mqtt_topic_matches("orders/created", "orders/deleted"));
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("/health*", "/health"));
        assert!(glob_match("/health*", "/healthz"));
        assert!(glob_match("*.internal", "orders.internal"));
        assert!(glob_match("orders.?", "orders.a"));
        assert!(!glob_match("orders.?", "orders.ab"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("/health", "/metrics"));
    }

    #[test]
    fn test_exact_kafka_join_and_unmatched() {
        let endpoints = vec![
            endpoint(
                "a",
                "p.py",
                3,
                Protocol::Kafka,
                ChannelRole::Producer,
                "orders.created",
                0.9,
            ),
            endpoint(
                "b",
                "c.js",
                8,
                Protocol::Kafka,
                ChannelRole::Consumer,
                "orders.created",
                0.8,
            ),
            endpoint(
                "a",
                "p.py",
                9,
                Protocol::Kafka,
                ChannelRole::Producer,
                "orders.bogus",
                0.9,
            ),
        ];
        let report = build_report(endpoints, &ChannelLinkOptions::default());

        assert_eq!(report.edges.len(), 1);
        let edge = &report.edges[0];
        assert_eq!(edge.channel(), "orders.created");
        assert!(edge.is_cross_repo());
        assert_eq!(edge.weight, 1);
        assert!((edge.confidence - 0.8).abs() < f32::EPSILON);
        assert_eq!(report.unmatched_producers.len(), 1);
        assert_eq!(report.unmatched_producers[0].channel_raw(), "orders.bogus");
        assert!(report.unmatched_consumers.is_empty());
    }

    #[test]
    fn test_pattern_consumer_join() {
        let producer = endpoint(
            "a",
            "p.py",
            3,
            Protocol::Mqtt,
            ChannelRole::Producer,
            "orders/created",
            0.6,
        );
        let consumer = endpoint(
            "b",
            "c.py",
            5,
            Protocol::Mqtt,
            ChannelRole::Consumer,
            "orders/+",
            0.5,
        )
        .as_pattern();
        let report = build_report(vec![producer, consumer], &ChannelLinkOptions::default());
        assert_eq!(report.edges.len(), 1);
        assert_eq!(report.edges[0].channel(), "orders/+");
    }

    #[test]
    fn test_unresolved_endpoints_never_match_but_are_reported() {
        let producer = endpoint(
            "a",
            "p.py",
            3,
            Protocol::Kafka,
            ChannelRole::Producer,
            "TOPIC_NAME",
            0.9,
        )
        .unresolved();
        let consumer = endpoint(
            "b",
            "c.js",
            8,
            Protocol::Kafka,
            ChannelRole::Consumer,
            "TOPIC_NAME",
            0.8,
        );
        let report = build_report(vec![producer, consumer], &ChannelLinkOptions::default());
        assert!(report.edges.is_empty());
        assert_eq!(report.unmatched_producers.len(), 1);
        assert_eq!(report.unmatched_consumers.len(), 1);
    }

    #[test]
    fn test_min_confidence_prunes_edges() {
        let endpoints = vec![
            endpoint(
                "a",
                "p.py",
                3,
                Protocol::Kafka,
                ChannelRole::Producer,
                "t",
                0.4,
            ),
            endpoint(
                "b",
                "c.js",
                8,
                Protocol::Kafka,
                ChannelRole::Consumer,
                "t",
                0.9,
            ),
        ];
        let options = ChannelLinkOptions {
            min_confidence: Some(0.5),
            ..Default::default()
        };
        let report = build_report(endpoints, &options);
        assert!(report.edges.is_empty());
    }

    #[test]
    fn test_weight_collapses_multiple_call_sites() {
        let endpoints = vec![
            endpoint(
                "a",
                "p.py",
                3,
                Protocol::Kafka,
                ChannelRole::Producer,
                "t",
                0.9,
            ),
            endpoint(
                "a",
                "p.py",
                7,
                Protocol::Kafka,
                ChannelRole::Producer,
                "t",
                0.9,
            ),
            endpoint(
                "b",
                "c.js",
                8,
                Protocol::Kafka,
                ChannelRole::Consumer,
                "t",
                0.8,
            ),
        ];
        let report = build_report(endpoints, &ChannelLinkOptions::default());
        assert_eq!(report.edges.len(), 1);
        assert_eq!(report.edges[0].weight, 2);
        assert_eq!(report.edges[0].producer.line(), 3);
    }
}
