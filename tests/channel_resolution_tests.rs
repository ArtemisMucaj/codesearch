//! End-to-end channel resolution: SCIP library confirmation + AST config-value
//! resolution, using the real tree-sitter resolver (not a stub).
//!
//! Mirrors the `@backend/kafkajs` + config-module shape found in the Netatmo
//! execution-engine repo: a consumer subscribes with a config-driven topic
//! whose value lives behind `process.env.X || 'default'`, and the call resolves
//! (via SCIP) into a known Kafka client package.

use std::collections::HashMap;
use std::sync::Arc;

use codesearch::{
    ChannelResolver, ResolveChannelsUseCase, ResolvedConfigValue, TreeSitterChannelExtractor,
};
use codesearch::{
    ChannelEndpoint, ChannelRole, EndpointSource, Language, Protocol, ReferenceKind,
    SymbolReference,
};

/// A config module matching the real execution-engine `config.ts` shape.
const CONFIG_SOURCE: &str = r#"
const APP_NAME = process.env.APP_NAME || 'execution-engine-domain-event'
export const config = {
    broker: {
        uri: process.env.KAFKA_BROKER || '127.0.0.1:9092',
        topics: {
            topologyEvent: process.env.KAFKA_TOPOLOGY_EVENT_TOPIC || 'topology_event',
            gatewayRegistered: process.env.KAFKA_GATEWAY_REGISTERED_EVENT_TOPIC || 'gateway_registered_event',
        },
    },
} as const
export type Config = typeof config
"#;

fn unresolved_endpoint(role: ChannelRole, expr: &str, line: u32) -> ChannelEndpoint {
    ChannelEndpoint::new(
        "engine".to_string(),
        "src/connector/api/application.ts".to_string(),
        line,
        Protocol::Kafka,
        role,
        expr.to_string(),
        expr.to_string(),
        0.5,
        EndpointSource::TreeSitter,
    )
    .unresolved()
}

fn kafka_ref(line: u32, method: &str) -> SymbolReference {
    SymbolReference::new(
        None,
        format!("Async#{method}"),
        "src/connector/api/application.ts".to_string(),
        "src/connector/api/application.ts".to_string(),
        line,
        1,
        ReferenceKind::MethodCall,
        Language::TypeScript,
        "engine".to_string(),
    )
    .with_callee_package("@backend/kafkajs")
}

#[test]
fn resolves_config_topic_and_confirms_library_end_to_end() {
    // The resolver is the real tree-sitter extractor.
    let resolver: Arc<dyn ChannelResolver> = Arc::new(TreeSitterChannelExtractor::new());
    let use_case = ResolveChannelsUseCase::new(resolver);

    // A producer and a consumer, both config-driven and unresolved as extracted.
    let producer = unresolved_endpoint(
        ChannelRole::Producer,
        "this.config.broker.topics.gatewayRegistered",
        64,
    );
    let consumer = unresolved_endpoint(
        ChannelRole::Consumer,
        "this.config.broker.topics.gatewayRegistered",
        90,
    );

    let mut refs = HashMap::new();
    refs.insert(
        "src/connector/api/application.ts".to_string(),
        vec![kafka_ref(64, "produce"), kafka_ref(90, "subscribe")],
    );

    let candidates = vec![("config".to_string(), CONFIG_SOURCE.to_string())];

    let out = use_case.resolve(vec![producer, consumer], &refs, &candidates);

    for endpoint in &out {
        // Config value resolved to the concrete default topic.
        assert_eq!(endpoint.channel_raw(), "gateway_registered_event");
        assert!(endpoint.is_resolved());
        assert_eq!(
            endpoint.env_var(),
            Some("KAFKA_GATEWAY_REGISTERED_EVENT_TOPIC")
        );
        // Library confirmed via SCIP.
        assert!(endpoint.is_confirmed());
        assert_eq!(endpoint.library(), Some("@backend/kafkajs"));
        assert!((endpoint.confidence() - 0.9).abs() < f32::EPSILON);
    }

    // Both sides now share a channel → they can join.
    assert_eq!(out[0].channel_normalized(), out[1].channel_normalized());
}

#[test]
fn unmatched_config_expression_stays_unresolved() {
    let resolver: Arc<dyn ChannelResolver> = Arc::new(TreeSitterChannelExtractor::new());
    let use_case = ResolveChannelsUseCase::new(resolver);

    // A property path not present in the config object: no resolution.
    let endpoint = unresolved_endpoint(
        ChannelRole::Producer,
        "this.config.broker.topics.unknownTopic",
        10,
    );
    let candidates = vec![("config".to_string(), CONFIG_SOURCE.to_string())];

    let out = use_case.resolve(vec![endpoint], &HashMap::new(), &candidates);
    assert!(!out[0].is_resolved());
    assert_eq!(out[0].channel_raw(), "this.config.broker.topics.unknownTopic");
    assert_eq!(out[0].env_var(), None);
}

/// Confirm the resolver never invents a library on a protocol mismatch even
/// when the config value resolves (uses a real resolver via a hand-set value).
struct FixedResolver(ResolvedConfigValue);
impl ChannelResolver for FixedResolver {
    fn resolve_config_expression(
        &self,
        _expression: &str,
        _candidates: &[(String, String)],
    ) -> Option<ResolvedConfigValue> {
        Some(self.0.clone())
    }
}

#[test]
fn mqtt_endpoint_not_confirmed_by_kafka_package() {
    let resolver: Arc<dyn ChannelResolver> = Arc::new(FixedResolver(ResolvedConfigValue {
        value: "sensors/room".to_string(),
        env_var: None,
    }));
    let use_case = ResolveChannelsUseCase::new(resolver);

    let mqtt = ChannelEndpoint::new(
        "engine".to_string(),
        "src/connector/api/application.ts".to_string(),
        50,
        Protocol::Mqtt,
        ChannelRole::Consumer,
        "this.config.mqtt.topic".to_string(),
        "this.config.mqtt.topic".to_string(),
        0.5,
        EndpointSource::TreeSitter,
    )
    .unresolved();

    let mut refs = HashMap::new();
    refs.insert(
        "src/connector/api/application.ts".to_string(),
        vec![kafka_ref(50, "produce")],
    );

    let out = use_case.resolve(vec![mqtt], &refs, &[]);
    // Value resolves, but the kafka package must not confirm an MQTT endpoint.
    assert_eq!(out[0].channel_raw(), "sensors/room");
    assert!(!out[0].is_confirmed());
    assert_eq!(out[0].library(), None);
}
