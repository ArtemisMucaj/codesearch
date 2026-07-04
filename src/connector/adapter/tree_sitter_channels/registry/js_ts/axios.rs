//! axios HTTP client detectors.

use super::super::{Detector, HTTP_CLIENT_METHODS};
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    // client: axios.get('http://…')
    for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Http,
        role: ChannelRole::Producer,
        query: r#"(call_expression
            function: (member_expression
                object: (identifier) @object
                property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier)] @channel))"#,
        filters: &[("object", &["axios"]), ("method", HTTP_CLIENT_METHODS)],
        confidence: 0.9,
    })
}
