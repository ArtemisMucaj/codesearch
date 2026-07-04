//! Flask / FastAPI route detectors (both expose the same decorator shape).

use super::super::Detector;
use crate::domain::{ChannelRole, Language, Protocol};

pub(super) fn detectors() -> Vec<Detector> {
    vec![
        // server: @app.route("/p"), @app.get("/p"), …
        Detector {
            language: Language::Python,
            protocol: Protocol::Http,
            role: ChannelRole::Consumer,
            query: r#"(decorator (call
                function: (attribute attribute: (identifier) @method)
                arguments: (argument_list . [(string) (identifier)] @channel)))"#,
            filters: &[(
                "method",
                &["route", "get", "post", "put", "delete", "patch"],
            )],
            confidence: 0.9,
        },
    ]
}
