//! requests / httpx HTTP client detectors (same free-function-on-module shape).

use super::super::{Detector, HTTP_CLIENT_METHODS};
use crate::domain::{ChannelRole, Language, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    vec![
        // client: requests.get("http://…"), httpx.post("http://…")
        Detector {
            language: Language::Python,
            protocol: Protocol::Http,
            role: ChannelRole::Producer,
            query: r#"(call
                function: (attribute
                    object: (identifier) @object
                    attribute: (identifier) @method)
                arguments: (argument_list . [(string) (identifier)] @channel))"#,
            filters: &[
                ("object", &["requests", "httpx"]),
                ("method", HTTP_CLIENT_METHODS),
            ],
            confidence: 0.9,
        },
    ]
}
