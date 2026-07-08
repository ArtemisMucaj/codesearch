//! End-to-end channel resolution: SCIP library confirmation + AST config-value
//! resolution, using the real tree-sitter resolver (not a stub).
//!
//! Mirrors a common `kafkajs` + config-module shape: a consumer subscribes with
//! a config-driven topic whose value lives behind `process.env.X || 'default'`,
//! and the call resolves (via SCIP) into a known Kafka client package.

use std::collections::HashMap;
use std::sync::Arc;

use codesearch::{
    ChannelEndpoint, ChannelRole, EndpointSource, Language, Protocol, ReferenceKind,
    SymbolReference,
};
use codesearch::{
    ChannelResolver, ResolveChannelsUseCase, ResolvedConfigValue, TreeSitterChannelExtractor,
};

/// A config module matching a typical service `config.ts` shape.
const CONFIG_SOURCE: &str = r#"
const APP_NAME = process.env.APP_NAME || 'orders-service'
export const config = {
    broker: {
        uri: process.env.KAFKA_BROKER || '127.0.0.1:9092',
        topics: {
            shipmentEvent: process.env.KAFKA_SHIPMENT_EVENT_TOPIC || 'shipment_event',
            orderPlaced: process.env.KAFKA_ORDER_PLACED_EVENT_TOPIC || 'order_placed_event',
        },
    },
} as const
export type Config = typeof config
"#;

fn unresolved_endpoint(role: ChannelRole, expr: &str, line: u32) -> ChannelEndpoint {
    ChannelEndpoint::new(
        "orders".to_string(),
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
        "orders".to_string(),
    )
    .with_callee_package("kafkajs")
}

#[test]
fn resolves_config_topic_and_confirms_library_end_to_end() {
    // The resolver is the real tree-sitter extractor.
    let resolver: Arc<dyn ChannelResolver> = Arc::new(TreeSitterChannelExtractor::new());
    let use_case = ResolveChannelsUseCase::new(resolver);

    // A producer and a consumer, both config-driven and unresolved as extracted.
    let producer = unresolved_endpoint(
        ChannelRole::Producer,
        "this.config.broker.topics.orderPlaced",
        64,
    );
    let consumer = unresolved_endpoint(
        ChannelRole::Consumer,
        "this.config.broker.topics.orderPlaced",
        90,
    );

    let mut refs = HashMap::new();
    refs.insert(
        "src/connector/api/application.ts".to_string(),
        vec![kafka_ref(64, "produce"), kafka_ref(90, "subscribe")],
    );

    let candidates = vec![("config".to_string(), CONFIG_SOURCE.to_string())];

    let out = use_case.resolve(
        "orders",
        vec![producer, consumer],
        &refs,
        &candidates,
        &HashMap::new(),
    );

    for endpoint in &out {
        // Config value resolved to the concrete default topic.
        assert_eq!(endpoint.channel_raw(), "order_placed_event");
        assert!(endpoint.is_resolved());
        assert_eq!(endpoint.env_var(), Some("KAFKA_ORDER_PLACED_EVENT_TOPIC"));
        // Library confirmed via SCIP.
        assert!(endpoint.is_confirmed());
        assert_eq!(endpoint.library(), Some("kafkajs"));
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

    let out = use_case.resolve(
        "orders",
        vec![endpoint],
        &HashMap::new(),
        &candidates,
        &HashMap::new(),
    );
    assert!(!out[0].is_resolved());
    assert_eq!(
        out[0].channel_raw(),
        "this.config.broker.topics.unknownTopic"
    );
    assert_eq!(out[0].env_var(), None);
}

/// Confirm the resolver never invents a library on a protocol mismatch even
/// when the config value resolves (uses a real resolver via a hand-set value).
struct FixedResolver(ResolvedConfigValue);
impl ChannelResolver for FixedResolver {
    fn resolve_config_expression(
        &self,
        _expression: &str,
        _enclosing_class: Option<&str>,
        _candidates: &[(String, String)],
    ) -> Option<ResolvedConfigValue> {
        Some(self.0.clone())
    }

    fn resolve_topic_pattern(
        &self,
        _expression: &str,
        _call_site_source: &str,
        _call_line: u32,
        _candidates: &[(String, String)],
    ) -> Option<String> {
        None
    }

    fn channel_argument_at(&self, _call_site_source: &str, _call_line: u32) -> Option<String> {
        None
    }

    fn resolve_loop_array_paths(
        &self,
        _expression: &str,
        _call_site_source: &str,
        _call_line: u32,
    ) -> Option<Vec<String>> {
        None
    }

    fn is_http_route_call_at(&self, _call_site_source: &str, _call_line: u32) -> bool {
        true
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
        "orders".to_string(),
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

    let out = use_case.resolve("orders", vec![mqtt], &refs, &[], &HashMap::new());
    // The MQTT endpoint is first; the kafka package must not confirm it.
    let mqtt_out = out
        .iter()
        .find(|e| e.protocol() == Protocol::Mqtt)
        .expect("mqtt endpoint");
    assert_eq!(mqtt_out.channel_raw(), "sensors/room");
    assert!(!mqtt_out.is_confirmed());
    assert_eq!(mqtt_out.library(), None);
}

/// The class that carries its topics through a constructor param (the producer
/// indirection in the `OrderEvents` wrapper).
const CLASS_SOURCE: &str = r#"
import { EventProducer } from 'kafkajs'
export class OrderEvents {
    constructor(
        private producer: EventProducer,
        private topics: { orderPlaced: string },
    ) { }
    async orderPlaced(event) {
        await this.producer.produce(this.topics.orderPlaced, JSON.stringify(event))
    }
}
"#;

const INSTANTIATION_SOURCE: &str = r#"
class Application {
    start() {
        const orderEvents = new OrderEvents(this.producer, {
            orderPlaced: this.config.broker.topics.orderPlaced,
        })
    }
}
"#;

#[test]
fn resolves_producer_topic_through_constructor_param_end_to_end() {
    let resolver: Arc<dyn ChannelResolver> = Arc::new(TreeSitterChannelExtractor::new());
    let use_case = ResolveChannelsUseCase::new(resolver);

    // The produce call is inside OrderEvents, at order-events.ts:15. Its topic
    // is `this.topics.orderPlaced` — a constructor param.
    let producer = ChannelEndpoint::new(
        "orders".to_string(),
        "src/connector/adapter/order-events.ts".to_string(),
        15,
        Protocol::Kafka,
        ChannelRole::Producer,
        "this.topics.orderPlaced".to_string(),
        "this.topics.orderPlaced".to_string(),
        0.5,
        EndpointSource::TreeSitter,
    )
    .unresolved();

    // SCIP records the enclosing class (OrderEvents) and the kafka package near
    // the call site.
    let mut refs = HashMap::new();
    let scip_ref = SymbolReference::new(
        Some("orderPlaced".to_string()),
        "EventProducer#produce".to_string(),
        "src/connector/adapter/order-events.ts".to_string(),
        "src/connector/adapter/order-events.ts".to_string(),
        14, // method call one line above the topic arg
        1,
        ReferenceKind::MethodCall,
        Language::TypeScript,
        "orders".to_string(),
    )
    .with_callee_package("kafkajs")
    .with_enclosing_scope("OrderEvents");
    refs.insert(
        "src/connector/adapter/order-events.ts".to_string(),
        vec![scip_ref],
    );

    let candidates = vec![
        ("OrderEvents".to_string(), CLASS_SOURCE.to_string()),
        (String::new(), INSTANTIATION_SOURCE.to_string()),
        ("config".to_string(), CONFIG_SOURCE.to_string()),
    ];

    let out = use_case.resolve(
        "orders",
        vec![producer],
        &refs,
        &candidates,
        &HashMap::new(),
    );

    // The two-hop chain resolved: this.topics.orderPlaced →
    // new OrderEvents(…, { orderPlaced: this.config.broker.topics.… }) →
    // config → the concrete topic + env var.
    assert_eq!(out[0].channel_raw(), "order_placed_event");
    assert!(out[0].is_resolved());
    assert_eq!(out[0].env_var(), Some("KAFKA_ORDER_PLACED_EVENT_TOPIC"));
    // And the library was confirmed via SCIP.
    assert!(out[0].is_confirmed());
    assert_eq!(out[0].library(), Some("kafkajs"));
}
