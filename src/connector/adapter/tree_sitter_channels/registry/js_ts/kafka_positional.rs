//! Positional Kafka client shapes: `producer.produce(topic, …)` and
//! `router.subscribe(topic, handler, …)`.
//!
//! Unlike the object form (`send({ topic })` / `subscribe({ topics })`) that
//! [`super::kafkajs`] covers, these pass the topic as the **first positional
//! argument**. The topic's type is `string`, but it is usually wired from
//! config as a property access (`this.topics.orders`), so the channel capture
//! accepts an identifier or a member expression in addition to a string;
//! property-access channels land unresolved (their trailing property is
//! recorded). A bare method name is a weak signal, hence the modest
//! confidence — the channel join filters most of the noise.
//!
//! `subscribe` deliberately does **not** accept a string argument: that shape
//! is MQTT's `client.subscribe('a/+')`, and matching it here would double-count
//! the same call as both an MQTT and a Kafka consumer. `produce` is unambiguous
//! (nothing else uses that method name), so it accepts strings too.

use super::super::Detector;
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    // producer: producer.produce("orders", payload) / produce(this.topics.orders, …)
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Kafka,
        role: ChannelRole::Producer,
        query: r#"(call_expression
            function: (member_expression property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier) (member_expression)] @channel))"#,
        filters: &[("method", &["produce"])],
        confidence: 0.5,
    }));
    // consumer: router.subscribe(topic, handler, schema) — identifier/property
    // topic only (a string arg is MQTT, matched elsewhere).
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Kafka,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (member_expression property: (property_identifier) @method)
            arguments: (arguments . [(identifier) (member_expression)] @channel))"#,
        filters: &[("method", &["subscribe"])],
        confidence: 0.5,
    }));
    all
}
