//! reqwest HTTP client detectors.

use super::super::{Detector, HTTP_CLIENT_METHODS};
use crate::domain::{ChannelRole, Language, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    vec![
        // free functions: reqwest::get("http://…")
        Detector {
            language: Language::Rust,
            protocol: Protocol::Http,
            role: ChannelRole::Producer,
            query: r#"(call_expression
                function: (scoped_identifier
                    path: (identifier) @object
                    name: (identifier) @method)
                arguments: (arguments . [(string_literal) (identifier)] @channel))"#,
            filters: &[("object", &["reqwest"]), ("method", HTTP_CLIENT_METHODS)],
            confidence: 0.9,
        },
        // client methods: client.get("http://…") — method names alone are
        // generic, hence the low confidence; the channel join filters the
        // noise.
        Detector {
            language: Language::Rust,
            protocol: Protocol::Http,
            role: ChannelRole::Producer,
            query: r#"(call_expression
                function: (field_expression field: (field_identifier) @method)
                arguments: (arguments . [(string_literal) (identifier)] @channel))"#,
            filters: &[("method", HTTP_CLIENT_METHODS)],
            confidence: 0.5,
        },
    ]
}
