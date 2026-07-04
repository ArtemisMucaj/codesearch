//! express route detectors.

use super::super::{Detector, HTTP_SERVER_METHODS};
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    // server: app.get('/p', handler), router.post('/p', …)
    for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Http,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (member_expression
                object: (identifier) @object
                property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier)] @channel))"#,
        filters: &[
            ("object", &["app", "router", "server"]),
            ("method", HTTP_SERVER_METHODS),
        ],
        confidence: 0.8,
    })
}
