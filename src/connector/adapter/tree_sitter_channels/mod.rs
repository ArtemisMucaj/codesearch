//! Tree-sitter channel extractor: finds communication endpoints (Kafka
//! producers/consumers, HTTP clients/routes, MQTT publish/subscribe) by
//! running the detector registry's queries over a file's AST.
//!
//! Sibling of `treesitter_parser.rs` rather than an extension of it — the
//! parser file sits at the per-file size guidance ceiling, and channel
//! extraction is an independent pass with its own registry.

mod config_resolver;
mod registry;

use std::collections::HashMap;

use async_trait::async_trait;
use streaming_iterator::StreamingIterator;
use tracing::{debug, warn};
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::application::{
    normalize_http_route, split_http_url, ChannelExtractor, ChannelResolver, ResolvedConfigValue,
};
use crate::domain::{ChannelEndpoint, DomainError, EndpointSource, Language, Protocol};

use registry::{detectors, Detector};

pub struct TreeSitterChannelExtractor {
    detectors: Vec<Detector>,
}

impl TreeSitterChannelExtractor {
    pub fn new() -> Self {
        Self {
            detectors: detectors(),
        }
    }

    fn get_ts_language(language: Language) -> Option<tree_sitter::Language> {
        match language {
            Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            _ => None,
        }
    }

    /// Run one detector's query over the tree, appending endpoints.
    #[allow(clippy::too_many_arguments)]
    fn run_detector(
        detector: &Detector,
        ts_language: &tree_sitter::Language,
        root: Node<'_>,
        content: &str,
        file_path: &str,
        repository_id: &str,
        endpoints: &mut Vec<ChannelEndpoint>,
    ) {
        let query = match Query::new(ts_language, detector.query) {
            Ok(q) => q,
            Err(e) => {
                // A registry entry that does not compile is a programming
                // error; skip it rather than failing the whole indexing run.
                warn!(
                    "Invalid channel detector query ({:?} {} {}): {}",
                    detector.language, detector.protocol, detector.role, e
                );
                return;
            }
        };

        let capture_names: Vec<&str> = query.capture_names().to_vec();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, content.as_bytes());

        'matches: while let Some(query_match) = matches.next() {
            let mut channel_node: Option<Node> = None;
            let mut captured: HashMap<&str, &str> = HashMap::new();

            for capture in query_match.captures {
                let name = capture_names
                    .get(capture.index as usize)
                    .copied()
                    .unwrap_or("");
                if name == "channel" {
                    channel_node = Some(capture.node);
                } else {
                    captured.insert(name, &content[capture.node.byte_range()]);
                }
            }

            for (capture, allowed) in detector.filters {
                match captured.get(capture) {
                    Some(text) if allowed.contains(text) => {}
                    _ => continue 'matches,
                }
            }

            let Some(node) = channel_node else {
                continue;
            };
            let raw_text = &content[node.byte_range()];
            let line = node.start_position().row as u32 + 1;

            // HTTP verb for display: the `@method` capture, with the verb-less
            // route registrations (`route`, `all`) reported as `ANY`. Non-HTTP
            // protocols carry no verb.
            let http_method = (detector.protocol == Protocol::Http)
                .then(|| match captured.get("method").copied() {
                    Some("route") | Some("all") | None => "ANY".to_string(),
                    Some(verb) => verb.to_string(),
                });

            // A string literal resolves immediately; an identifier or a
            // property access (`this.topics.orders`) is recorded unresolved so
            // the unmatched report stays honest and phase 3 has something to
            // resolve.
            let literal = if is_unresolved_channel(node.kind()) {
                None
            } else {
                strip_string_literal(raw_text)
            };

            let endpoint = match literal {
                Some(value) if !value.trim().is_empty() => {
                    let (host, normalized, is_pattern) =
                        normalize_channel(detector.protocol, &value);
                    let mut endpoint = ChannelEndpoint::new(
                        repository_id.to_string(),
                        file_path.to_string(),
                        line,
                        detector.protocol,
                        detector.role,
                        value,
                        normalized,
                        detector.confidence,
                        EndpointSource::TreeSitter,
                    );
                    if let Some(host) = host {
                        endpoint = endpoint.with_host(host);
                    }
                    if let Some(method) = &http_method {
                        endpoint = endpoint.with_method(method);
                    }
                    if is_pattern {
                        endpoint = endpoint.as_pattern();
                    }
                    endpoint
                }
                Some(_) => continue, // empty literal — nothing to join on
                None if is_unresolved_channel(node.kind()) => {
                    // For a property access, the trailing name is the most
                    // channel-like token (`this.topics.orders` → `orders`);
                    // for a bare identifier it is the identifier itself.
                    let name = unresolved_channel_name(node, content, raw_text);
                    let mut endpoint = ChannelEndpoint::new(
                        repository_id.to_string(),
                        file_path.to_string(),
                        line,
                        detector.protocol,
                        detector.role,
                        name.clone(),
                        name,
                        detector.confidence,
                        EndpointSource::TreeSitter,
                    )
                    .unresolved();
                    if let Some(method) = &http_method {
                        endpoint = endpoint.with_method(method);
                    }
                    endpoint
                }
                None => continue, // unparseable literal
            };

            let endpoint = match enclosing_symbol(node, content) {
                Some(symbol) => endpoint.with_enclosing_symbol(symbol),
                None => endpoint,
            };
            endpoints.push(endpoint);
        }
    }
}

impl Default for TreeSitterChannelExtractor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelExtractor for TreeSitterChannelExtractor {
    async fn extract(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
    ) -> Result<Vec<ChannelEndpoint>, DomainError> {
        let active: Vec<&Detector> = self
            .detectors
            .iter()
            .filter(|d| d.language == language)
            .collect();
        if active.is_empty() {
            return Ok(Vec::new());
        }

        let ts_language = Self::get_ts_language(language)
            .ok_or_else(|| DomainError::parse(format!("Unsupported language: {:?}", language)))?;

        let mut parser = Parser::new();
        parser
            .set_language(&ts_language)
            .map_err(|e| DomainError::parse(format!("Failed to set language: {}", e)))?;
        let tree = parser
            .parse(content, None)
            .ok_or_else(|| DomainError::parse("Failed to parse file"))?;

        let mut endpoints = Vec::new();
        for detector in active {
            Self::run_detector(
                detector,
                &ts_language,
                tree.root_node(),
                content,
                file_path,
                repository_id,
                &mut endpoints,
            );
        }

        // Detector shapes overlap (e.g. generic method-name detectors); keep
        // the highest-confidence endpoint per distinct call site.
        let mut best: HashMap<(u32, Protocol, String, String), ChannelEndpoint> = HashMap::new();
        for endpoint in endpoints {
            let key = (
                endpoint.line(),
                endpoint.protocol(),
                endpoint.role().as_str().to_string(),
                endpoint.channel_raw().to_string(),
            );
            match best.get(&key) {
                Some(existing) if existing.confidence() >= endpoint.confidence() => {}
                _ => {
                    best.insert(key, endpoint);
                }
            }
        }
        let mut endpoints: Vec<ChannelEndpoint> = best.into_values().collect();
        endpoints.sort_by(|a, b| {
            a.line()
                .cmp(&b.line())
                .then(a.channel_raw().cmp(b.channel_raw()))
        });

        debug!(
            "Extracted {} channel endpoints from {} ({:?})",
            endpoints.len(),
            file_path,
            language
        );
        Ok(endpoints)
    }

    fn supports_language(&self, language: Language) -> bool {
        self.detectors.iter().any(|d| d.language == language)
    }
}

impl ChannelResolver for TreeSitterChannelExtractor {
    fn resolve_config_expression(
        &self,
        expression: &str,
        enclosing_class: Option<&str>,
        candidates: &[(String, String)],
    ) -> Option<ResolvedConfigValue> {
        let borrowed: Vec<(&str, &str)> = candidates
            .iter()
            .map(|(name, source)| (name.as_str(), source.as_str()))
            .collect();

        // Direct config access (`this.config.broker.topics.X`) resolves in one
        // pass; a constructor-param access (`this.topics.X` inside a class)
        // needs the extra hop through the `new Class(…)` site.
        let resolved = config_resolver::resolve_channel_expression(expression, &borrowed)
            .or_else(|| {
                enclosing_class.and_then(|class| {
                    config_resolver::resolve_via_constructor_param(expression, class, &borrowed)
                })
            })?;

        Some(ResolvedConfigValue {
            value: resolved.value,
            env_var: resolved.env_var,
        })
    }

    fn resolve_topic_pattern(
        &self,
        expression: &str,
        call_site_source: &str,
        call_line: u32,
        candidates: &[(String, String)],
    ) -> Option<String> {
        let borrowed: Vec<(&str, &str)> = candidates
            .iter()
            .map(|(name, source)| (name.as_str(), source.as_str()))
            .collect();

        // A template literal or getter-backed variable in the call site itself,
        // else an interface-dispatched client call resolved across sources.
        config_resolver::infer_topic_pattern(expression, call_site_source).or_else(|| {
            config_resolver::resolve_via_interface(call_site_source, call_line, &borrowed)
        })
    }
}

/// Per-protocol channel normalization at extraction time.
///
/// Returns `(host, normalized, is_pattern)`.
fn normalize_channel(protocol: Protocol, raw: &str) -> (Option<String>, String, bool) {
    match protocol {
        Protocol::Http => {
            let (host, path) = split_http_url(raw);
            (host, normalize_http_route(&path), false)
        }
        Protocol::Mqtt => {
            let trimmed = raw.trim().to_string();
            let is_pattern = trimmed.contains('+') || trimmed.contains('#');
            (None, trimmed, is_pattern)
        }
        Protocol::Kafka | Protocol::Amqp | Protocol::Grpc => (None, raw.trim().to_string(), false),
    }
}

/// Node kinds whose channel argument is not a literal and is therefore
/// recorded as an unresolved endpoint: a bare identifier (`ORDERS_TOPIC`), or a
/// property access carrying the topic through config
/// (`this.topics.gatewayRegistered`, kafkajs wrappers). Covers the JS/TS
/// `member_expression`, Python `attribute`, and Rust `field_expression` /
/// `scoped_identifier` shapes.
fn is_unresolved_channel(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "member_expression"
            | "attribute"
            | "field_expression"
            | "scoped_identifier"
    )
}

/// The channel identifier recorded for an unresolved endpoint. A bare
/// identifier is used verbatim; a **property access is kept whole**
/// (`this.config.broker.topics.orders`) so the config resolver can follow the
/// path — the display layer shortens it to the trailing property. Interior
/// whitespace is collapsed so the stored form is stable.
fn unresolved_channel_name(node: Node<'_>, content: &str, raw_text: &str) -> String {
    let is_property_access = matches!(
        node.kind(),
        "member_expression" | "attribute" | "field_expression" | "scoped_identifier"
    );
    if is_property_access {
        let full: String = content[node.byte_range()].split_whitespace().collect();
        if !full.is_empty() {
            return full;
        }
    }
    raw_text.to_string()
}

/// Extract the contents of a string literal node's text, stripping quotes and
/// any literal prefix (`f"…"`, `r"…"`, `b'…'`).
fn strip_string_literal(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let start = raw.find(['"', '\'', '`'])?;
    let quote = raw[start..].chars().next()?;
    let inner = &raw[start + 1..];
    let end = inner.rfind(quote)?;
    let mut value = inner[..end].to_string();
    // Collapse triple-quoted literals ("""…""" / '''…''').
    while value.starts_with(quote) && value.ends_with(quote) && value.len() >= 2 {
        value = value[1..value.len() - 1].to_string();
    }
    Some(value)
}

/// Walk up from a call site to the function that contains it — the same walk
/// that fills `parent_symbol` on chunks. This is what lets phase 2 attach
/// channels to call-graph nodes.
fn enclosing_symbol(node: Node<'_>, content: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(ancestor) = current {
        match ancestor.kind() {
            "function_definition"
            | "function_declaration"
            | "function_item"
            | "method_definition"
            | "method_declaration" => {
                if let Some(name) = ancestor.child_by_field_name("name") {
                    return Some(content[name.byte_range()].to_string());
                }
            }
            // A route decorator sits outside its function_definition; take the
            // decorated function's name (`@app.route(…)` → the handler).
            "decorated_definition" => {
                if let Some(name) = ancestor
                    .child_by_field_name("definition")
                    .and_then(|definition| definition.child_by_field_name("name"))
                {
                    return Some(content[name.byte_range()].to_string());
                }
            }
            "arrow_function" | "function_expression" | "function" => {
                // Anonymous functions take their name from the surrounding
                // binding: `const handler = async () => {…}`.
                if let Some(parent) = ancestor.parent() {
                    let name_node = match parent.kind() {
                        "variable_declarator" => parent.child_by_field_name("name"),
                        "pair" => parent.child_by_field_name("key"),
                        "assignment_expression" => parent.child_by_field_name("left"),
                        _ => None,
                    };
                    if let Some(name) = name_node {
                        return Some(content[name.byte_range()].to_string());
                    }
                }
            }
            _ => {}
        }
        current = ancestor.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ChannelRole;

    async fn extract(content: &str, file: &str, language: Language) -> Vec<ChannelEndpoint> {
        TreeSitterChannelExtractor::new()
            .extract(content, file, language, "test-repo")
            .await
            .unwrap()
    }

    #[test]
    fn test_strip_string_literal() {
        assert_eq!(
            strip_string_literal(r#""orders""#),
            Some("orders".to_string())
        );
        assert_eq!(strip_string_literal("'orders'"), Some("orders".to_string()));
        assert_eq!(strip_string_literal("`orders`"), Some("orders".to_string()));
        assert_eq!(
            strip_string_literal(r#"f"/api/{id}""#),
            Some("/api/{id}".to_string())
        );
        assert_eq!(
            strip_string_literal(r#"r"topic""#),
            Some("topic".to_string())
        );
        assert_eq!(
            strip_string_literal(r#""""doc""""#),
            Some("doc".to_string())
        );
        assert_eq!(strip_string_literal("no_quotes"), None);
    }

    #[tokio::test]
    async fn test_python_kafka_producer_and_flask_route() {
        let content = r#"
from kafka import KafkaProducer

producer = KafkaProducer()

def checkout(order):
    producer.send("orders.created", order)

@app.route("/api/orders/<order_id>")
def get_order(order_id):
    return lookup(order_id)
"#;
        let endpoints = extract(content, "app.py", Language::Python).await;

        let kafka: Vec<_> = endpoints
            .iter()
            .filter(|e| e.protocol() == Protocol::Kafka)
            .collect();
        assert_eq!(kafka.len(), 1);
        assert_eq!(kafka[0].role(), ChannelRole::Producer);
        assert_eq!(kafka[0].channel_raw(), "orders.created");
        assert_eq!(kafka[0].enclosing_symbol(), Some("checkout"));

        let http: Vec<_> = endpoints
            .iter()
            .filter(|e| e.protocol() == Protocol::Http)
            .collect();
        assert_eq!(http.len(), 1);
        assert_eq!(http[0].role(), ChannelRole::Consumer);
        assert_eq!(http[0].channel_normalized(), "/api/orders/{}");
        assert_eq!(http[0].enclosing_symbol(), Some("get_order"));
        // `@app.route(...)` has no explicit verb — reported as ANY.
        assert_eq!(http[0].method(), Some("ANY"));
        // Non-HTTP endpoints carry no verb.
        assert_eq!(kafka[0].method(), None);
    }

    #[tokio::test]
    async fn test_python_kafka_consumer_constructor_and_subscribe() {
        let content = r#"
consumer = KafkaConsumer("orders.created", bootstrap_servers="localhost")
consumer.subscribe(["payments.settled", "orders.updated"])
"#;
        let endpoints = extract(content, "consumer.py", Language::Python).await;
        let channels: Vec<&str> = endpoints.iter().map(|e| e.channel_raw()).collect();
        assert!(channels.contains(&"orders.created"));
        assert!(channels.contains(&"payments.settled"));
        assert!(channels.contains(&"orders.updated"));
        assert!(
            !channels.contains(&"localhost"),
            "keyword args must not match"
        );
        assert!(endpoints
            .iter()
            .all(|e| e.role() == ChannelRole::Consumer && e.protocol() == Protocol::Kafka));
    }

    #[tokio::test]
    async fn test_python_unresolved_identifier_recorded() {
        let content = r#"
def notify(payload):
    producer.send(ORDERS_TOPIC, payload)
"#;
        let endpoints = extract(content, "app.py", Language::Python).await;
        assert_eq!(endpoints.len(), 1);
        assert!(!endpoints[0].is_resolved());
        assert_eq!(endpoints[0].channel_raw(), "ORDERS_TOPIC");
    }

    #[tokio::test]
    async fn test_javascript_kafkajs_and_axios() {
        let content = r#"
async function start() {
    await consumer.subscribe({ topics: ['orders.created'] });
}

const fetchOrder = async (id) => {
    return axios.get('http://orders-service/api/orders/123');
};

client.publish('sensors/kitchen/temp', payload);
client.subscribe('sensors/+/temp');
"#;
        let endpoints = extract(content, "index.js", Language::JavaScript).await;

        let kafka: Vec<_> = endpoints
            .iter()
            .filter(|e| e.protocol() == Protocol::Kafka)
            .collect();
        assert_eq!(kafka.len(), 1);
        assert_eq!(kafka[0].channel_raw(), "orders.created");
        assert_eq!(kafka[0].role(), ChannelRole::Consumer);
        assert_eq!(kafka[0].enclosing_symbol(), Some("start"));

        let http: Vec<_> = endpoints
            .iter()
            .filter(|e| e.protocol() == Protocol::Http)
            .collect();
        assert_eq!(http.len(), 1);
        assert_eq!(http[0].role(), ChannelRole::Producer);
        assert_eq!(http[0].channel_normalized(), "/api/orders/123");
        assert_eq!(http[0].host(), Some("orders-service"));
        assert_eq!(http[0].enclosing_symbol(), Some("fetchOrder"));
        // axios.get(...) carries its verb through to the endpoint.
        assert_eq!(http[0].method(), Some("GET"));

        let mqtt: Vec<_> = endpoints
            .iter()
            .filter(|e| e.protocol() == Protocol::Mqtt)
            .collect();
        assert_eq!(mqtt.len(), 2);
        let pattern = mqtt.iter().find(|e| e.is_pattern()).unwrap();
        assert_eq!(pattern.channel_raw(), "sensors/+/temp");
        assert_eq!(pattern.role(), ChannelRole::Consumer);
    }

    #[tokio::test]
    async fn test_kafka_positional_produce_and_subscribe() {
        // Positional Kafka shapes: the topic is the first positional arg,
        // wired from config as a property access rather than a string literal.
        let content = r#"
class DomainEvent {
    async gatewayRegistered(event) {
        await this.producer.produce(this.topics.gatewayRegistered, payload, key);
    }
    async fixed(event) {
        await this.producer.produce("orders.created", payload);
    }
}

async function start() {
    router.subscribe(this.config.broker.topics.topologyEvent, handler, schema);
}
"#;
        let endpoints = extract(content, "application.ts", Language::TypeScript).await;

        let producers: Vec<_> = endpoints
            .iter()
            .filter(|e| e.role() == ChannelRole::Producer && e.protocol() == Protocol::Kafka)
            .collect();
        assert_eq!(producers.len(), 2);
        // Property-access topic → unresolved, trailing property recorded.
        let prop = producers
            .iter()
            .find(|e| !e.is_resolved())
            .expect("property-access producer");
        // The full property path is kept so config resolution can follow it.
        assert_eq!(prop.channel_raw(), "this.topics.gatewayRegistered");
        assert_eq!(prop.enclosing_symbol(), Some("gatewayRegistered"));
        // A string-literal topic resolves normally.
        let literal = producers
            .iter()
            .find(|e| e.is_resolved())
            .expect("string-literal producer");
        assert_eq!(literal.channel_raw(), "orders.created");

        let consumers: Vec<_> = endpoints
            .iter()
            .filter(|e| e.role() == ChannelRole::Consumer && e.protocol() == Protocol::Kafka)
            .collect();
        assert_eq!(consumers.len(), 1);
        assert_eq!(
            consumers[0].channel_raw(),
            "this.config.broker.topics.topologyEvent"
        );
        assert!(!consumers[0].is_resolved());
        assert_eq!(consumers[0].enclosing_symbol(), Some("start"));
    }

    #[tokio::test]
    async fn test_mqtt_string_subscribe_not_stolen_by_kafka() {
        // A string-argument `.subscribe('a/+')` must stay MQTT — the positional
        // Kafka consumer detector only accepts identifier/property channels,
        // never a string.
        let content = r#"client.subscribe('sensors/+/temp');"#;
        let endpoints = extract(content, "index.ts", Language::TypeScript).await;
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].protocol(), Protocol::Mqtt);
        assert_eq!(endpoints[0].role(), ChannelRole::Consumer);
    }

    #[tokio::test]
    async fn test_typescript_express_route() {
        let content = r#"
app.get('/users/:id', async (req, res) => {
    res.send(users[req.params.id]);
});
"#;
        let endpoints = extract(content, "server.ts", Language::TypeScript).await;
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].role(), ChannelRole::Consumer);
        assert_eq!(endpoints[0].channel_normalized(), "/users/{}");
        assert_eq!(endpoints[0].method(), Some("GET"));
    }

    #[tokio::test]
    async fn test_rust_axum_and_reqwest() {
        let content = r#"
async fn build_router() -> Router {
    Router::new().route("/api/orders/{id}", get(get_order))
}

async fn fetch_status() -> Result<String, Error> {
    let body = reqwest::get("http://status-service/api/status").await?;
    Ok(body.text().await?)
}
"#;
        let endpoints = extract(content, "main.rs", Language::Rust).await;

        let server: Vec<_> = endpoints
            .iter()
            .filter(|e| e.role() == ChannelRole::Consumer)
            .collect();
        assert_eq!(server.len(), 1);
        assert_eq!(server[0].channel_normalized(), "/api/orders/{}");
        assert_eq!(server[0].enclosing_symbol(), Some("build_router"));
        // axum's `.route(...)` is verb-less — ANY.
        assert_eq!(server[0].method(), Some("ANY"));

        let client: Vec<_> = endpoints
            .iter()
            .filter(|e| e.role() == ChannelRole::Producer)
            .collect();
        assert_eq!(client.len(), 1);
        assert_eq!(client[0].channel_normalized(), "/api/status");
        assert_eq!(client[0].host(), Some("status-service"));
        assert_eq!(client[0].method(), Some("GET"));
        // The unambiguous reqwest:: path detector must win over the generic
        // method-name detector at the same call site.
        assert!((client[0].confidence() - 0.9).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_unsupported_language_yields_nothing() {
        let extractor = TreeSitterChannelExtractor::new();
        assert!(!extractor.supports_language(Language::HCL));
        let endpoints = extractor
            .extract("resource \"x\" {}", "main.tf", Language::HCL, "repo")
            .await
            .unwrap();
        assert!(endpoints.is_empty());
    }
}
