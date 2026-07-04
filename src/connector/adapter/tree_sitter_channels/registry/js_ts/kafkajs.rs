//! kafkajs producer/consumer detectors.

use super::super::Detector;
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    // producer: producer.send({ topic: 'orders.created', … })
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Kafka,
        role: ChannelRole::Producer,
        query: r#"(call_expression
            function: (member_expression property: (property_identifier) @method)
            arguments: (arguments (object (pair
                key: (property_identifier) @key
                value: [(string) (identifier)] @channel))))"#,
        filters: &[("method", &["send"]), ("key", &["topic"])],
        confidence: 0.8,
    }));
    // consumer: consumer.subscribe({ topic: 't' })
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Kafka,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (member_expression property: (property_identifier) @method)
            arguments: (arguments (object (pair
                key: (property_identifier) @key
                value: [(string) (identifier)] @channel))))"#,
        filters: &[("method", &["subscribe"]), ("key", &["topic"])],
        confidence: 0.8,
    }));
    // consumer: consumer.subscribe({ topics: ['t1', 't2'] })
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Kafka,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (member_expression property: (property_identifier) @method)
            arguments: (arguments (object (pair
                key: (property_identifier) @key
                value: (array [(string) (identifier)] @channel)))))"#,
        filters: &[("method", &["subscribe"]), ("key", &["topics"])],
        confidence: 0.8,
    }));
    all
}
