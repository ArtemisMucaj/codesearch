//! fetch (WHATWG / Node global) HTTP client detector.

use super::super::Detector;
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    // client: fetch('http://…')
    for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Http,
        role: ChannelRole::Producer,
        query: r#"(call_expression
            function: (identifier) @func
            arguments: (arguments . [(string) (identifier)] @channel))"#,
        filters: &[("func", &["fetch"])],
        confidence: 0.9,
    })
}
