//! axum route detectors.

use super::super::Detector;
use crate::domain::{ChannelRole, Language, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    vec![
        // server: .route("/p", get(handler))
        Detector {
            language: Language::Rust,
            protocol: Protocol::Http,
            role: ChannelRole::Consumer,
            query: r#"(call_expression
                function: (field_expression field: (field_identifier) @method)
                arguments: (arguments . [(string_literal) (identifier)] @channel))"#,
            filters: &[("method", &["route"])],
            confidence: 0.9,
        },
    ]
}
