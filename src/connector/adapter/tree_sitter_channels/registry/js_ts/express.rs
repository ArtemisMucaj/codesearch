//! express route detectors.

use super::super::{Detector, HTTP_SERVER_METHODS};
use super::for_both_languages;
use crate::domain::{ChannelRole, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    let mut all = Vec::new();
    // server: app.get('/p', handler), router.post('/p', …) — the route object is
    // a bare identifier. A second argument (the handler) is required: it excludes
    // `app.get('title')`, Express's one-argument *settings getter*, which is not
    // a route registration.
    //
    // The channel also accepts a `member_expression` for the dynamic-registration
    // shape `router.get(route.path, handler)`, where a route table is iterated in
    // a loop. The trailing property (`path`) is recorded unresolved; phase 3's
    // loop-array resolver expands it to the literal `path:` values in the table.
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Http,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (member_expression
                object: (identifier) @object
                property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier) (member_expression)] @channel . (_)))"#,
        filters: &[
            ("object", &["app", "router", "server"]),
            ("method", HTTP_SERVER_METHODS),
        ],
        confidence: 0.8,
    }));
    // server: this.router.get('/p', …), this.app.post('/p', …) — the route
    // object is a field access (a class holds the router). `@object` captures
    // the trailing property (`router`), so the same allowlist applies as for the
    // bare-identifier form. The handler argument is required for the same reason
    // as the bare-identifier form above. `member_expression` channels are
    // accepted for the same loop-registration reason as the bare-identifier form.
    all.extend(for_both_languages(|language| Detector {
        language,
        protocol: Protocol::Http,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (member_expression
                object: (member_expression property: (property_identifier) @object)
                property: (property_identifier) @method)
            arguments: (arguments . [(string) (identifier) (member_expression)] @channel . (_)))"#,
        filters: &[
            ("object", &["app", "router", "server"]),
            ("method", HTTP_SERVER_METHODS),
        ],
        confidence: 0.8,
    }));
    all
}
