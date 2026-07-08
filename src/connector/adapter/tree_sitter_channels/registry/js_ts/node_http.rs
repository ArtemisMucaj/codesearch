//! Node.js core `http` / `https` client detectors.
//!
//! Covers the low-level outbound request shapes from Node's built-in modules:
//! `https.request(url, options, cb)` and `http.get(url, cb)`. The first
//! positional argument is the target URL (a string literal, or a variable /
//! property access recorded unresolved). This is the client counterpart to the
//! higher-level `axios` / `fetch` detectors, for code that uses the standard
//! library directly.
//!
//! Only `request` and `get` are matched — the two methods that issue an
//! outbound request. `http.createServer(app)` is deliberately excluded: it
//! stands up a server rather than addressing a channel, and the routes it serves
//! are already covered by the Express detectors.

use super::super::Detector;
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

/// The `object.method` receivers that name Node's core HTTP modules.
const NODE_HTTP_OBJECTS: &[&str] = &["http", "https"];
/// The methods on those modules that issue an outbound request.
const NODE_HTTP_METHODS: &[&str] = &["request", "get"];

pub(super) fn detectors() -> Vec<Detector> {
    // client: https.request('http://…', options, cb) / http.get(url, cb)
    for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Http,
        role: ChannelRole::Producer,
        query: r#"(call_expression
            function: (member_expression
                object: (identifier) @object
                property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier) (member_expression)] @channel))"#,
        filters: &[("object", NODE_HTTP_OBJECTS), ("method", NODE_HTTP_METHODS)],
        confidence: 0.7,
    })
}
