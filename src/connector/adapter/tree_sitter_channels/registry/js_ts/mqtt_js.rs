//! mqtt.js publish/subscribe detectors.

use super::super::Detector;
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    // producer: client.publish('a/b', payload)
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Mqtt,
        role: ChannelRole::Producer,
        query: r#"(call_expression
            function: (member_expression property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier)] @channel))"#,
        filters: &[("method", &["publish"])],
        confidence: 0.6,
    }));
    // consumer: client.subscribe('a/+')
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Mqtt,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (member_expression property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier)] @channel))"#,
        filters: &[("method", &["subscribe"])],
        confidence: 0.5,
    }));
    all
}
