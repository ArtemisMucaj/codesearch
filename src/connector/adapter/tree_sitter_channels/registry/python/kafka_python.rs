//! kafka-python producer/consumer detectors.

use super::super::Detector;
use crate::domain::{ChannelRole, Language, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    vec![
        // producer: producer.send("topic", payload)
        Detector {
            language: Language::Python,
            protocol: Protocol::Kafka,
            role: ChannelRole::Producer,
            query: r#"(call
                function: (attribute attribute: (identifier) @method)
                arguments: (argument_list . [(string) (identifier)] @channel))"#,
            filters: &[("method", &["send"])],
            confidence: 0.6,
        },
        // consumer: KafkaConsumer("topic-a", "topic-b", …)
        Detector {
            language: Language::Python,
            protocol: Protocol::Kafka,
            role: ChannelRole::Consumer,
            query: r#"(call
                function: (identifier) @func
                arguments: (argument_list [(string) (identifier)] @channel))"#,
            filters: &[("func", &["KafkaConsumer"])],
            confidence: 0.9,
        },
        // consumer: consumer.subscribe(["topic"])
        Detector {
            language: Language::Python,
            protocol: Protocol::Kafka,
            role: ChannelRole::Consumer,
            query: r#"(call
                function: (attribute attribute: (identifier) @method)
                arguments: (argument_list (list [(string) (identifier)] @channel)))"#,
            filters: &[("method", &["subscribe"])],
            confidence: 0.7,
        },
    ]
}
