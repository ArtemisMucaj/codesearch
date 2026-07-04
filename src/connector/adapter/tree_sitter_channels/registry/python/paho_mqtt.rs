//! paho-mqtt publish/subscribe detectors.

use super::super::Detector;
use crate::domain::{ChannelRole, Language, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    vec![
        // producer: client.publish("a/b", payload)
        Detector {
            language: Language::Python,
            protocol: Protocol::Mqtt,
            role: ChannelRole::Producer,
            query: r#"(call
                function: (attribute attribute: (identifier) @method)
                arguments: (argument_list . [(string) (identifier)] @channel))"#,
            filters: &[("method", &["publish"])],
            confidence: 0.6,
        },
        // consumer: client.subscribe("a/+")  (string arg, not a list)
        Detector {
            language: Language::Python,
            protocol: Protocol::Mqtt,
            role: ChannelRole::Consumer,
            query: r#"(call
                function: (attribute attribute: (identifier) @method)
                arguments: (argument_list . [(string) (identifier)] @channel))"#,
            filters: &[("method", &["subscribe"])],
            confidence: 0.5,
        },
    ]
}
