//! Enrich extracted channel endpoints with cross-file resolution.
//!
//! The tree-sitter extractor sees one call site at a time, so a channel wired
//! through config (`this.config.broker.topics.orders`) or a generic client
//! method (`.produce()`, `.subscribe()`) comes out unresolved and low
//! confidence. This pass runs once per repository, after extraction and SCIP
//! import, and uses two cross-file signals:
//!
//! 1. **Library confirmation** — the SCIP reference at the same call site
//!    carries the package the callee is defined in (`@backend/kafkajs`). If it
//!    maps to a known client library, the endpoint is confirmed and its
//!    confidence raised: a bare `.produce()` is no longer a guess.
//! 2. **Config value resolution** — an unresolved property-access channel is
//!    run through the [`ChannelResolver`] against the repo's config modules,
//!    recovering the concrete topic string and the env var behind it.

use std::collections::HashMap;
use std::sync::Arc;

use crate::application::ChannelResolver;
use crate::domain::{ChannelEndpoint, Protocol, SymbolReference};

/// Confidence assigned to an endpoint once its library is confirmed via SCIP —
/// high, because the receiver's type (not just a method name) now backs it.
const CONFIRMED_CONFIDENCE: f32 = 0.9;

/// A `(config_object_name, module_source)` pair the resolver can search.
pub type ConfigCandidate = (String, String);

/// Maps a client library's package name to the protocol it speaks. This is the
/// one place library knowledge lives; adding a client is one entry.
fn protocol_for_package(package: &str) -> Option<Protocol> {
    // Match on a substring so scoped/wrapped variants resolve too
    // (`@backend/kafkajs`, `kafkajs`, `@confluentinc/kafka-javascript`).
    let p = package.to_ascii_lowercase();
    if p.contains("kafka") || p.contains("rdkafka") {
        Some(Protocol::Kafka)
    } else if p.contains("mqtt") {
        Some(Protocol::Mqtt)
    } else if p.contains("amqp") || p.contains("rabbit") {
        Some(Protocol::Amqp)
    } else if p.contains("grpc") {
        Some(Protocol::Grpc)
    } else {
        None
    }
}

pub struct ResolveChannelsUseCase {
    resolver: Arc<dyn ChannelResolver>,
}

impl ResolveChannelsUseCase {
    pub fn new(resolver: Arc<dyn ChannelResolver>) -> Self {
        Self { resolver }
    }

    /// Enrich `endpoints` in place-ish (consumes and returns them).
    ///
    /// - `refs_by_file` is the SCIP call graph keyed by the file the reference
    ///   occurs in (as produced by the importer).
    /// - `config_candidates` are the config modules discovered in the repo.
    pub fn resolve(
        &self,
        endpoints: Vec<ChannelEndpoint>,
        refs_by_file: &HashMap<String, Vec<SymbolReference>>,
        config_candidates: &[ConfigCandidate],
    ) -> Vec<ChannelEndpoint> {
        endpoints
            .into_iter()
            .map(|endpoint| self.resolve_one(endpoint, refs_by_file, config_candidates))
            .collect()
    }

    fn resolve_one(
        &self,
        mut endpoint: ChannelEndpoint,
        refs_by_file: &HashMap<String, Vec<SymbolReference>>,
        config_candidates: &[ConfigCandidate],
    ) -> ChannelEndpoint {
        // 1. Library confirmation via the SCIP reference at this call site.
        if let Some(package) = library_package_at(endpoint.file_path(), endpoint.line(), refs_by_file)
        {
            if let Some(protocol) = protocol_for_package(&package) {
                if protocol == endpoint.protocol() {
                    endpoint = endpoint
                        .with_library(package)
                        .confirmed()
                        .with_confidence(CONFIRMED_CONFIDENCE);
                }
            }
        }

        // 2. Config value resolution for unresolved property-access channels.
        if !endpoint.is_resolved() {
            if let Some(resolved) = self
                .resolver
                .resolve_config_expression(endpoint.channel_raw(), config_candidates)
            {
                let (_, normalized, _) = normalize(endpoint.protocol(), &resolved.value);
                endpoint = endpoint.resolve_channel(resolved.value, normalized);
                if let Some(env) = resolved.env_var {
                    endpoint = endpoint.with_env_var(env);
                }
            }
        }

        endpoint
    }
}

/// Per-protocol normalization mirroring the extractor's, so a config-resolved
/// value joins the same way an inline literal would.
fn normalize(protocol: Protocol, raw: &str) -> (Option<String>, String, bool) {
    use crate::application::{normalize_http_route, split_http_url};
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

/// How far above/below the endpoint's line a SCIP reference may sit and still
/// be considered the same call site. A method call and its channel argument
/// often land on adjacent lines when the call is wrapped across lines
/// (`produce(\n  topic, …)`), so an exact-line match misses them.
const CALL_SITE_LINE_WINDOW: u32 = 2;

/// The client-library package backing the call at `file:line`, if any SCIP
/// reference within [`CALL_SITE_LINE_WINDOW`] lines resolves into one.
///
/// A call site produces several references (the method, each argument); only
/// some carry a third-party package. We take the nearest reference whose
/// package maps to a known client library, so the app's own wrapper classes
/// (whose package is the project itself) do not mask the underlying client.
fn library_package_at(
    file_path: &str,
    line: u32,
    refs_by_file: &HashMap<String, Vec<SymbolReference>>,
) -> Option<String> {
    let refs = refs_by_file.get(file_path)?;
    refs.iter()
        .filter(|r| r.reference_line().abs_diff(line) <= CALL_SITE_LINE_WINDOW)
        .filter_map(|r| {
            let package = r.callee_package()?;
            // Only packages that map to a client library count — this skips the
            // project's own package on wrapper calls.
            protocol_for_package(package).map(|_| (r.reference_line(), package.to_string()))
        })
        // Nearest line wins when several qualify.
        .min_by_key(|(ref_line, _)| ref_line.abs_diff(line))
        .map(|(_, package)| package)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ResolvedConfigValue;
    use crate::domain::{ChannelRole, EndpointSource, Language, ReferenceKind};

    struct StubResolver {
        value: Option<ResolvedConfigValue>,
    }

    impl ChannelResolver for StubResolver {
        fn resolve_config_expression(
            &self,
            _expression: &str,
            _candidates: &[(String, String)],
        ) -> Option<ResolvedConfigValue> {
            self.value.clone()
        }
    }

    fn endpoint(protocol: Protocol, channel: &str, resolved: bool) -> ChannelEndpoint {
        let e = ChannelEndpoint::new(
            "repo".to_string(),
            "src/app.ts".to_string(),
            42,
            protocol,
            ChannelRole::Producer,
            channel.to_string(),
            channel.to_string(),
            0.5,
            EndpointSource::TreeSitter,
        );
        if resolved {
            e
        } else {
            e.unresolved()
        }
    }

    fn scip_ref(line: u32, package: Option<&str>) -> SymbolReference {
        let r = SymbolReference::new(
            None,
            "AsyncProducer#produce".to_string(),
            "src/app.ts".to_string(),
            "src/app.ts".to_string(),
            line,
            1,
            ReferenceKind::MethodCall,
            Language::TypeScript,
            "repo".to_string(),
        );
        match package {
            Some(p) => r.with_callee_package(p),
            None => r,
        }
    }

    fn refs_map(refs: Vec<SymbolReference>) -> HashMap<String, Vec<SymbolReference>> {
        let mut m = HashMap::new();
        m.insert("src/app.ts".to_string(), refs);
        m
    }

    #[test]
    fn confirms_library_and_boosts_confidence() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver { value: None }));
        let refs = refs_map(vec![scip_ref(42, Some("@backend/kafkajs"))]);

        let out = uc.resolve(vec![endpoint(Protocol::Kafka, "orders", true)], &refs, &[]);
        assert_eq!(out[0].library(), Some("@backend/kafkajs"));
        assert!(out[0].is_confirmed());
        assert!((out[0].confidence() - CONFIRMED_CONFIDENCE).abs() < f32::EPSILON);
    }

    #[test]
    fn does_not_confirm_on_protocol_mismatch() {
        // An MQTT endpoint must not be confirmed by a kafka package.
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver { value: None }));
        let refs = refs_map(vec![scip_ref(42, Some("@backend/kafkajs"))]);

        let out = uc.resolve(vec![endpoint(Protocol::Mqtt, "sensors/x", true)], &refs, &[]);
        assert!(!out[0].is_confirmed());
        assert_eq!(out[0].library(), None);
    }

    #[test]
    fn resolves_config_value_and_env() {
        let resolved = ResolvedConfigValue {
            value: "topology_event".to_string(),
            env_var: Some("KAFKA_TOPOLOGY_EVENT_TOPIC".to_string()),
        };
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver {
            value: Some(resolved),
        }));
        let refs = refs_map(vec![scip_ref(42, Some("@backend/kafkajs"))]);

        let out = uc.resolve(
            vec![endpoint(Protocol::Kafka, "this.config.broker.topics.topologyEvent", false)],
            &refs,
            &[("config".to_string(), "…".to_string())],
        );
        assert_eq!(out[0].channel_raw(), "topology_event");
        assert!(out[0].is_resolved());
        assert_eq!(out[0].env_var(), Some("KAFKA_TOPOLOGY_EVENT_TOPIC"));
        // Library confirmation still applied.
        assert!(out[0].is_confirmed());
    }

    #[test]
    fn leaves_unconfirmed_when_no_package() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver { value: None }));
        let refs = refs_map(vec![scip_ref(42, None)]);

        let out = uc.resolve(vec![endpoint(Protocol::Kafka, "orders", true)], &refs, &[]);
        assert!(!out[0].is_confirmed());
        assert!((out[0].confidence() - 0.5).abs() < f32::EPSILON);
    }
}
