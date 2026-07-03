//! Detector registry: every framework integration is data, not code.
//!
//! A detector is a tree-sitter query with a `@channel` capture (the argument
//! holding the channel name — a string literal, or an identifier recorded as
//! an unresolved endpoint) plus capture filters that pin the query to a
//! framework's call shape. Adding a framework is one entry here and one
//! fixture test.

use crate::domain::{ChannelRole, Language, Protocol};

/// A single framework detector.
pub(super) struct Detector {
    pub language: Language,
    pub protocol: Protocol,
    pub role: ChannelRole,
    /// Tree-sitter S-expression containing a `@channel` capture.
    pub query: &'static str,
    /// Each named capture must equal one of the allowed values for the match
    /// to count (evaluated in Rust; keeps the queries predicate-free).
    pub filters: &'static [(&'static str, &'static [&'static str])],
    /// Extraction confidence for endpoints produced by this detector.
    pub confidence: f32,
}

const HTTP_CLIENT_METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "head"];
const HTTP_SERVER_METHODS: &[&str] = &["get", "post", "put", "delete", "patch", "all"];

/// Precision note: method-name matching (e.g. any `.send(...)`) occasionally
/// fires on unrelated objects. That is acceptable here — a false endpoint only
/// becomes a false edge if an opposite-role endpoint exists on the same
/// channel string, so the join itself filters most noise; confidence scoring
/// covers the rest. Detectors bound to an unambiguous shape (constructor
/// name, decorator, `reqwest::` path) get higher confidence than bare method
/// names.
pub(super) fn detectors() -> Vec<Detector> {
    // ── Python ────────────────────────────────────────────────────────────
    // kafka-python producer: producer.send("topic", payload)
    let mut all = vec![Detector {
        language: Language::Python,
        protocol: Protocol::Kafka,
        role: ChannelRole::Producer,
        query: r#"(call
            function: (attribute attribute: (identifier) @method)
            arguments: (argument_list . [(string) (identifier)] @channel))"#,
        filters: &[("method", &["send"])],
        confidence: 0.6,
    }];
    // kafka-python consumer: KafkaConsumer("topic-a", "topic-b", …)
    all.push(Detector {
        language: Language::Python,
        protocol: Protocol::Kafka,
        role: ChannelRole::Consumer,
        query: r#"(call
            function: (identifier) @func
            arguments: (argument_list [(string) (identifier)] @channel))"#,
        filters: &[("func", &["KafkaConsumer"])],
        confidence: 0.9,
    });
    // kafka-python consumer: consumer.subscribe(["topic"])
    all.push(Detector {
        language: Language::Python,
        protocol: Protocol::Kafka,
        role: ChannelRole::Consumer,
        query: r#"(call
            function: (attribute attribute: (identifier) @method)
            arguments: (argument_list (list [(string) (identifier)] @channel)))"#,
        filters: &[("method", &["subscribe"])],
        confidence: 0.7,
    });
    // paho-mqtt producer: client.publish("a/b", payload)
    all.push(Detector {
        language: Language::Python,
        protocol: Protocol::Mqtt,
        role: ChannelRole::Producer,
        query: r#"(call
            function: (attribute attribute: (identifier) @method)
            arguments: (argument_list . [(string) (identifier)] @channel))"#,
        filters: &[("method", &["publish"])],
        confidence: 0.6,
    });
    // paho-mqtt consumer: client.subscribe("a/+")  (string arg, not a list)
    all.push(Detector {
        language: Language::Python,
        protocol: Protocol::Mqtt,
        role: ChannelRole::Consumer,
        query: r#"(call
            function: (attribute attribute: (identifier) @method)
            arguments: (argument_list . [(string) (identifier)] @channel))"#,
        filters: &[("method", &["subscribe"])],
        confidence: 0.5,
    });
    // Flask / FastAPI server: @app.route("/p"), @app.get("/p"), …
    all.push(Detector {
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
    });
    // requests client: requests.get("http://…")
    all.push(Detector {
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
    });

    // ── JavaScript / TypeScript (same grammar shapes) ─────────────────────
    for language in [Language::JavaScript, Language::TypeScript] {
        // express server: app.get('/p', handler), router.post('/p', …)
        all.push(Detector {
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
        });
        // axios client: axios.get('http://…')
        all.push(Detector {
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
        });
        // fetch('http://…')
        all.push(Detector {
            language,
            protocol: Protocol::Http,
            role: ChannelRole::Producer,
            query: r#"(call_expression
                function: (identifier) @func
                arguments: (arguments . [(string) (identifier)] @channel))"#,
            filters: &[("func", &["fetch"])],
            confidence: 0.9,
        });
        // kafkajs producer: producer.send({ topic: 'orders.created', … })
        all.push(Detector {
            language,
            protocol: Protocol::Kafka,
            role: ChannelRole::Producer,
            query: r#"(call_expression
                function: (member_expression property: (property_identifier) @method)
                arguments: (arguments (object (pair
                    key: (property_identifier) @key
                    value: [(string) (identifier)] @channel))))"#,
            filters: &[("method", &["send"]), ("key", &["topic"])],
            confidence: 0.8,
        });
        // kafkajs consumer: consumer.subscribe({ topic: 't' })
        all.push(Detector {
            language,
            protocol: Protocol::Kafka,
            role: ChannelRole::Consumer,
            query: r#"(call_expression
                function: (member_expression property: (property_identifier) @method)
                arguments: (arguments (object (pair
                    key: (property_identifier) @key
                    value: [(string) (identifier)] @channel))))"#,
            filters: &[("method", &["subscribe"]), ("key", &["topic"])],
            confidence: 0.8,
        });
        // kafkajs consumer: consumer.subscribe({ topics: ['t1', 't2'] })
        all.push(Detector {
            language,
            protocol: Protocol::Kafka,
            role: ChannelRole::Consumer,
            query: r#"(call_expression
                function: (member_expression property: (property_identifier) @method)
                arguments: (arguments (object (pair
                    key: (property_identifier) @key
                    value: (array [(string) (identifier)] @channel)))))"#,
            filters: &[("method", &["subscribe"]), ("key", &["topics"])],
            confidence: 0.8,
        });
        // mqtt.js producer: client.publish('a/b', payload)
        all.push(Detector {
            language,
            protocol: Protocol::Mqtt,
            role: ChannelRole::Producer,
            query: r#"(call_expression
                function: (member_expression property: (property_identifier) @method)
                arguments: (arguments . [(string) (identifier)] @channel))"#,
            filters: &[("method", &["publish"])],
            confidence: 0.6,
        });
        // mqtt.js consumer: client.subscribe('a/+')
        all.push(Detector {
            language,
            protocol: Protocol::Mqtt,
            role: ChannelRole::Consumer,
            query: r#"(call_expression
                function: (member_expression property: (property_identifier) @method)
                arguments: (arguments . [(string) (identifier)] @channel))"#,
            filters: &[("method", &["subscribe"])],
            confidence: 0.5,
        });
    }

    // ── Rust ──────────────────────────────────────────────────────────────
    // axum server: .route("/p", get(handler))
    all.push(Detector {
        language: Language::Rust,
        protocol: Protocol::Http,
        role: ChannelRole::Consumer,
        query: r#"(call_expression
            function: (field_expression field: (field_identifier) @method)
            arguments: (arguments . [(string_literal) (identifier)] @channel))"#,
        filters: &[("method", &["route"])],
        confidence: 0.9,
    });
    // reqwest free functions: reqwest::get("http://…")
    all.push(Detector {
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
    });
    // reqwest client methods: client.get("http://…") — method names alone are
    // generic, hence the low confidence; the channel join filters the noise.
    all.push(Detector {
        language: Language::Rust,
        protocol: Protocol::Http,
        role: ChannelRole::Producer,
        query: r#"(call_expression
            function: (field_expression field: (field_identifier) @method)
            arguments: (arguments . [(string_literal) (identifier)] @channel))"#,
        filters: &[("method", HTTP_CLIENT_METHODS)],
        confidence: 0.5,
    });

    all
}
