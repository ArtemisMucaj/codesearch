use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The transport protocol a communication endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Kafka,
    Http,
    Mqtt,
    Amqp,
    Grpc,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::Kafka => "kafka",
            Protocol::Http => "http",
            Protocol::Mqtt => "mqtt",
            Protocol::Amqp => "amqp",
            Protocol::Grpc => "grpc",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "kafka" => Some(Protocol::Kafka),
            "http" => Some(Protocol::Http),
            "mqtt" => Some(Protocol::Mqtt),
            "amqp" => Some(Protocol::Amqp),
            "grpc" => Some(Protocol::Grpc),
            _ => None,
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Which side of the channel an endpoint sits on.
///
/// HTTP maps onto the same pair: a client call site is a `Producer`, a
/// route/handler registration is a `Consumer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelRole {
    Producer,
    Consumer,
}

impl ChannelRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChannelRole::Producer => "producer",
            ChannelRole::Consumer => "consumer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "producer" => Some(ChannelRole::Producer),
            "consumer" => Some(ChannelRole::Consumer),
            _ => None,
        }
    }

    /// The role this one links against.
    pub fn opposite(&self) -> Self {
        match self {
            ChannelRole::Producer => ChannelRole::Consumer,
            ChannelRole::Consumer => ChannelRole::Producer,
        }
    }
}

impl std::fmt::Display for ChannelRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// How an endpoint was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSource {
    /// Static extraction from the AST (string literal at the call site).
    TreeSitter,
    /// Resolved through a configuration file (phase 3).
    Config,
    /// Inferred by an LLM (phase 3, lowest trust).
    Llm,
}

impl EndpointSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            EndpointSource::TreeSitter => "tree_sitter",
            EndpointSource::Config => "config",
            EndpointSource::Llm => "llm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tree_sitter" => Some(EndpointSource::TreeSitter),
            "config" => Some(EndpointSource::Config),
            "llm" => Some(EndpointSource::Llm),
            _ => None,
        }
    }
}

impl std::fmt::Display for EndpointSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A communication endpoint: a call site that publishes to or consumes from a
/// named channel (Kafka topic, HTTP route, MQTT topic, …).
///
/// Endpoints from different repositories are joined on the channel identifier
/// at query time to derive cross-service edges; they are the rendezvous points
/// that symbol-based linking cannot see.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEndpoint {
    /// Unique identifier for this endpoint.
    id: String,

    /// Repository the call site belongs to.
    repository_id: String,

    /// File containing the call site (relative to the repository root).
    file_path: String,

    /// Function containing the call site, when the AST walk can determine it.
    /// Load-bearing for phase 2: it is what attaches channels to call-graph
    /// nodes (impact analysis, execution features).
    enclosing_symbol: Option<String>,

    /// Line number of the call site (1-indexed).
    line: u32,

    /// Transport protocol.
    protocol: Protocol,

    /// Producer or consumer side.
    role: ChannelRole,

    /// The channel identifier exactly as written: `"orders.created"`,
    /// `"/users/<id>"`, or the identifier name when unresolved.
    channel_raw: String,

    /// Template-normalized channel used for matching (HTTP route parameters
    /// rewritten to `{}`, URLs reduced to their path).
    channel_normalized: String,

    /// Host portion of an absolute HTTP client URL. Unused until phase 3.
    host: Option<String>,

    /// HTTP verb for an HTTP endpoint (`GET`, `POST`, … or `ANY` for a
    /// verb-less route registration). `None` for non-HTTP protocols. Purely
    /// descriptive — it is never part of the producer↔consumer join.
    method: Option<String>,

    /// The client library this endpoint was confirmed against, resolved from
    /// the call's SCIP symbol (e.g. `kafkajs`). `None` when resolution
    /// could not attribute the call to a known library.
    library: Option<String>,

    /// The environment variable that overrides the channel value, when the
    /// channel was resolved through a `process.env.X || 'default'` config
    /// expression (e.g. `KAFKA_SHIPMENT_EVENT_TOPIC`). `None` otherwise.
    env_var: Option<String>,

    /// True when resolution confirmed the call really targets a known client
    /// library (via its SCIP-resolved package). A confirmed endpoint from a
    /// generic method name (`.produce()`, `.subscribe()`) is trustworthy;
    /// an unconfirmed one is a weaker, method-name-only guess.
    confirmed: bool,

    /// True when the channel is a wildcard/pattern subscription
    /// (e.g. MQTT `orders/+`).
    is_pattern: bool,

    /// False when the channel argument was an identifier rather than a
    /// literal. Unresolved endpoints are excluded from matching but recorded
    /// so the unmatched report stays honest and phase 3 can resolve them.
    resolved: bool,

    /// Extraction confidence in `[0, 1]`.
    confidence: f32,

    /// How this endpoint was discovered.
    source: EndpointSource,
}

impl ChannelEndpoint {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repository_id: String,
        file_path: String,
        line: u32,
        protocol: Protocol,
        role: ChannelRole,
        channel_raw: String,
        channel_normalized: String,
        confidence: f32,
        source: EndpointSource,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            repository_id,
            file_path,
            enclosing_symbol: None,
            line,
            protocol,
            role,
            channel_raw,
            channel_normalized,
            host: None,
            method: None,
            library: None,
            env_var: None,
            confirmed: false,
            is_pattern: false,
            resolved: true,
            confidence: confidence.clamp(0.0, 1.0),
            source,
        }
    }

    /// Reconstitutes from persisted data (used by adapters).
    #[allow(clippy::too_many_arguments)]
    pub fn reconstitute(
        id: String,
        repository_id: String,
        file_path: String,
        enclosing_symbol: Option<String>,
        line: u32,
        protocol: Protocol,
        role: ChannelRole,
        channel_raw: String,
        channel_normalized: String,
        host: Option<String>,
        method: Option<String>,
        library: Option<String>,
        env_var: Option<String>,
        confirmed: bool,
        is_pattern: bool,
        resolved: bool,
        confidence: f32,
        source: EndpointSource,
    ) -> Self {
        Self {
            id,
            repository_id,
            file_path,
            enclosing_symbol,
            line,
            protocol,
            role,
            channel_raw,
            channel_normalized,
            host,
            method,
            library,
            env_var,
            confirmed,
            is_pattern,
            resolved,
            confidence: confidence.clamp(0.0, 1.0),
            source,
        }
    }

    pub fn with_enclosing_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.enclosing_symbol = Some(symbol.into());
        self
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Attach the HTTP verb (uppercased). Non-HTTP endpoints leave this unset.
    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into().to_uppercase());
        self
    }

    pub fn as_pattern(mut self) -> Self {
        self.is_pattern = true;
        self
    }

    /// Mark the endpoint as unresolved (channel argument was an identifier,
    /// not a literal).
    pub fn unresolved(mut self) -> Self {
        self.resolved = false;
        self
    }

    /// Record the client library this endpoint was confirmed against.
    pub fn with_library(mut self, library: impl Into<String>) -> Self {
        self.library = Some(library.into());
        self
    }

    /// Record the env var that overrides the channel value.
    pub fn with_env_var(mut self, env_var: impl Into<String>) -> Self {
        self.env_var = Some(env_var.into());
        self
    }

    /// Mark the endpoint as library-confirmed (SCIP attributed the call to a
    /// known client library).
    pub fn confirmed(mut self) -> Self {
        self.confirmed = true;
        self
    }

    /// Replace the confidence score (resolution raises a weak method-name guess
    /// once the library is confirmed).
    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    /// Rewrite the channel to a value recovered by resolution: `raw` is what is
    /// shown, `normalized` is what the producer↔consumer join keys on. Marks
    /// the endpoint resolved.
    pub fn resolve_channel(
        mut self,
        raw: impl Into<String>,
        normalized: impl Into<String>,
    ) -> Self {
        self.channel_raw = raw.into();
        self.channel_normalized = normalized.into();
        self.resolved = true;
        self
    }

    // Getters
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn enclosing_symbol(&self) -> Option<&str> {
        self.enclosing_symbol.as_deref()
    }

    pub fn line(&self) -> u32 {
        self.line
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    pub fn role(&self) -> ChannelRole {
        self.role
    }

    pub fn channel_raw(&self) -> &str {
        &self.channel_raw
    }

    pub fn channel_normalized(&self) -> &str {
        &self.channel_normalized
    }

    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// HTTP verb (`GET`, `POST`, `ANY`, …), or `None` for non-HTTP endpoints.
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// The client library this endpoint was confirmed against
    /// (e.g. `kafkajs`), or `None`.
    pub fn library(&self) -> Option<&str> {
        self.library.as_deref()
    }

    /// The env var overriding the channel value, or `None`.
    pub fn env_var(&self) -> Option<&str> {
        self.env_var.as_deref()
    }

    /// Whether resolution confirmed the call targets a known client library.
    pub fn is_confirmed(&self) -> bool {
        self.confirmed
    }

    pub fn is_pattern(&self) -> bool {
        self.is_pattern
    }

    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    pub fn confidence(&self) -> f32 {
        self.confidence
    }

    pub fn source(&self) -> EndpointSource {
        self.source
    }

    /// Returns a formatted location string for this endpoint.
    pub fn location(&self) -> String {
        format!("{}:{}", self.file_path, self.line)
    }
}

/// A derived producer→consumer link over a shared channel.
///
/// Edges are computed at query time from stored endpoints and never persisted,
/// so re-indexing one repository can never leave stale edges behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEdge {
    /// Representative producer call site (lowest line in its file).
    pub producer: ChannelEndpoint,
    /// Representative consumer call site (lowest line in its file).
    pub consumer: ChannelEndpoint,
    /// Distinct call sites collapsed into this (file-pair, channel) edge.
    pub weight: usize,
    /// `min(producer.confidence, consumer.confidence)`.
    pub confidence: f32,
}

impl ChannelEdge {
    /// Returns true when both endpoints belong to different repositories.
    pub fn is_cross_repo(&self) -> bool {
        self.producer.repository_id() != self.consumer.repository_id()
    }

    /// The channel both sides agree on (the consumer's normalized channel,
    /// which for pattern subscriptions is the subscription pattern).
    pub fn channel(&self) -> &str {
        self.consumer.channel_normalized()
    }

    /// The shared protocol.
    pub fn protocol(&self) -> Protocol {
        self.producer.protocol()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(role: ChannelRole, repo: &str) -> ChannelEndpoint {
        ChannelEndpoint::new(
            repo.to_string(),
            "src/app.py".to_string(),
            12,
            Protocol::Kafka,
            role,
            "orders.created".to_string(),
            "orders.created".to_string(),
            0.9,
            EndpointSource::TreeSitter,
        )
    }

    #[test]
    fn test_endpoint_builders() {
        let ep = endpoint(ChannelRole::Producer, "repo-a")
            .with_enclosing_symbol("checkout")
            .as_pattern()
            .unresolved();

        assert_eq!(ep.enclosing_symbol(), Some("checkout"));
        assert!(ep.is_pattern());
        assert!(!ep.is_resolved());
        assert_eq!(ep.location(), "src/app.py:12");
    }

    #[test]
    fn test_enum_roundtrips() {
        for p in [
            Protocol::Kafka,
            Protocol::Http,
            Protocol::Mqtt,
            Protocol::Amqp,
            Protocol::Grpc,
        ] {
            assert_eq!(Protocol::parse(p.as_str()), Some(p));
        }
        assert_eq!(Protocol::parse("smtp"), None);

        for r in [ChannelRole::Producer, ChannelRole::Consumer] {
            assert_eq!(ChannelRole::parse(r.as_str()), Some(r));
            assert_eq!(r.opposite().opposite(), r);
        }

        for s in [
            EndpointSource::TreeSitter,
            EndpointSource::Config,
            EndpointSource::Llm,
        ] {
            assert_eq!(EndpointSource::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn test_confidence_is_clamped() {
        let over = ChannelEndpoint::new(
            "repo-a".to_string(),
            "src/app.py".to_string(),
            1,
            Protocol::Kafka,
            ChannelRole::Producer,
            "orders.created".to_string(),
            "orders.created".to_string(),
            1.5,
            EndpointSource::TreeSitter,
        );
        assert_eq!(over.confidence(), 1.0);

        let under = ChannelEndpoint::reconstitute(
            "id-1".to_string(),
            "repo-a".to_string(),
            "src/app.py".to_string(),
            None,
            1,
            Protocol::Kafka,
            ChannelRole::Producer,
            "orders.created".to_string(),
            "orders.created".to_string(),
            None,  // host
            None,  // method
            None,  // library
            None,  // env_var
            false, // confirmed
            false, // is_pattern
            true,  // resolved
            -0.3,
            EndpointSource::TreeSitter,
        );
        assert_eq!(under.confidence(), 0.0);
    }

    #[test]
    fn test_edge_cross_repo() {
        let edge = ChannelEdge {
            producer: endpoint(ChannelRole::Producer, "repo-a"),
            consumer: endpoint(ChannelRole::Consumer, "repo-b"),
            weight: 1,
            confidence: 0.9,
        };
        assert!(edge.is_cross_repo());
        assert_eq!(edge.channel(), "orders.created");
        assert_eq!(edge.protocol(), Protocol::Kafka);
    }
}
