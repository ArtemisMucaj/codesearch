//! Resolve a config-driven channel value from JS/TS source.
//!
//! Many services never pass a topic/route as a string literal; they pass a
//! property access into a config object — `this.config.broker.topics.orders` —
//! whose leaf is a `process.env.X || 'default'` expression. The tree-sitter
//! extractor can only record `orders` (unresolved). This resolver takes the
//! property path and the source of the module that defines the config object,
//! walks the object literal down that path, and returns the default string plus
//! the overriding env var.
//!
//! It is deliberately a small, syntactic value-resolver — not a general
//! evaluator. It handles the dominant shape (a nested object literal of
//! `env || 'literal'` / plain-literal entries) and gives up (returns `None`)
//! on anything it does not understand, leaving the endpoint unresolved rather
//! than guessing.

use tree_sitter::{Node, Parser};

/// A channel value recovered from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedChannel {
    /// The concrete default string the channel resolves to (`"topology_event"`).
    pub value: String,
    /// The env var that overrides it at runtime, if the config reads one
    /// (`KAFKA_TOPOLOGY_EVENT_TOPIC`).
    pub env_var: Option<String>,
}

/// Resolve a config-driven channel expression (`this.config.broker.topics.X`,
/// `config.broker.topics.X`) against a set of candidate config module sources.
///
/// Splits the expression into `head` (the config object, e.g. `config`) and the
/// trailing property path, then tries each candidate source until one defines a
/// matching object and the path resolves. `candidates` are `(object_name,
/// source)` pairs — a caller that cannot statically know the object name may
/// pass the conventional `config`.
pub(super) fn resolve_channel_expression(
    expression: &str,
    candidates: &[(&str, &str)],
) -> Option<ResolvedChannel> {
    let segments = property_segments(expression);
    // Need at least `<object>.<one-key>`.
    if segments.len() < 2 {
        return None;
    }
    let path: Vec<&str> = segments[1..].iter().map(|s| s.as_str()).collect();

    for (object_name, source) in candidates {
        if let Some(resolved) = resolve_in_config(source, object_name, &path) {
            return Some(resolved);
        }
    }
    None
}

/// Break a member-access expression into its bare segments, dropping a leading
/// `this`: `this.config.broker.topics.orders` → `[config, broker, topics,
/// orders]`. Subscript access (`a['b']`) is normalised to `a.b`.
fn property_segments(expression: &str) -> Vec<String> {
    let normalized = expression.replace(['[', ']'], ".");
    normalized
        .split('.')
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`'))
        .filter(|s| !s.is_empty() && *s != "this")
        .map(|s| s.to_string())
        .collect()
}

/// Walk `config_source`'s exported object literal down `path` and resolve the
/// leaf value. `path` is the property chain *after* the config-object head,
/// e.g. `["broker", "topics", "topologyEvent"]` for
/// `config.broker.topics.topologyEvent`. Returns `None` when the object, the
/// path, or the leaf initializer cannot be resolved syntactically.
pub(super) fn resolve_in_config(
    config_source: &str,
    config_object: &str,
    path: &[&str],
) -> Option<ResolvedChannel> {
    if path.is_empty() {
        return None;
    }

    let mut parser = Parser::new();
    // TS is a superset of the JS object syntax we read; parsing config as TS
    // handles both `.ts` and `.js` config modules.
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    let tree = parser.parse(config_source, None)?;

    let object = find_named_object(tree.root_node(), config_source, config_object)?;
    let leaf = walk_object_path(object, config_source, path)?;
    resolve_value_expression(leaf, config_source)
}

/// Find the object literal bound to `name` (`const <name> = { … }`,
/// `export const <name> = { … }`, possibly `as const`).
fn find_named_object<'a>(node: Node<'a>, source: &str, name: &str) -> Option<Node<'a>> {
    if node.kind() == "variable_declarator" {
        if let Some(id) = node.child_by_field_name("name") {
            if &source[id.byte_range()] == name {
                if let Some(value) = node.child_by_field_name("value") {
                    if let Some(obj) = unwrap_to_object(value) {
                        return Some(obj);
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_named_object(child, source, name) {
            return Some(found);
        }
    }
    None
}

/// Strip a wrapping `<expr> as const` / parenthesised expression down to the
/// underlying `object` node, if any.
fn unwrap_to_object(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "object" {
        return Some(node);
    }
    // `{ … } as const`, `({ … })`, `{ … } satisfies T`
    if matches!(
        node.kind(),
        "as_expression" | "parenthesized_expression" | "satisfies_expression"
    ) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(obj) = unwrap_to_object(child) {
                return Some(obj);
            }
        }
    }
    None
}

/// Descend an object literal following `path`, returning the value node of the
/// final key.
fn walk_object_path<'a>(object: Node<'a>, source: &str, path: &[&str]) -> Option<Node<'a>> {
    let mut current = object;
    for (i, segment) in path.iter().enumerate() {
        let value = object_property(current, source, segment)?;
        if i == path.len() - 1 {
            return Some(value);
        }
        current = unwrap_to_object(value)?;
    }
    None
}

/// The value node of property `key` within an `object` node.
fn object_property<'a>(object: Node<'a>, source: &str, key: &str) -> Option<Node<'a>> {
    let mut cursor = object.walk();
    for pair in object.children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        let Some(pair_key) = pair.child_by_field_name("key") else {
            continue;
        };
        if property_key_text(pair_key, source) == key {
            return pair.child_by_field_name("value");
        }
    }
    None
}

/// The bare name of a property key, stripping quotes for string keys.
fn property_key_text(key: Node<'_>, source: &str) -> String {
    let raw = &source[key.byte_range()];
    match key.kind() {
        "string" => raw.trim_matches(|c| c == '"' || c == '\'' || c == '`').to_string(),
        _ => raw.to_string(),
    }
}

/// Resolve a leaf value expression into a channel value:
/// - `process.env.X || 'default'` → `{ value: "default", env_var: Some("X") }`
/// - `'literal'` / `"literal"`     → `{ value: "literal", env_var: None }`
/// - `process.env.X`               → `{ value: "X", env_var: Some("X") }` (best effort)
fn resolve_value_expression(node: Node<'_>, source: &str) -> Option<ResolvedChannel> {
    match node.kind() {
        "string" => Some(ResolvedChannel {
            value: strip_quotes(&source[node.byte_range()]),
            env_var: None,
        }),
        // `A || B` / `A ?? B`: prefer the string default on the right, keep the
        // env var from the left.
        "binary_expression" => {
            let left = node.child_by_field_name("left")?;
            let right = node.child_by_field_name("right")?;
            let env_var = env_var_name(left, source);
            // The default is whichever side is a string literal (usually right).
            let default = [right, left].into_iter().find_map(|side| {
                if side.kind() == "string" {
                    Some(strip_quotes(&source[side.byte_range()]))
                } else {
                    None
                }
            });
            match (default, env_var) {
                (Some(value), env) => Some(ResolvedChannel { value, env_var: env }),
                // `process.env.X || otherVar` with no literal: fall back to the
                // env var name as the value so the endpoint is at least named.
                (None, Some(env)) => Some(ResolvedChannel {
                    value: env.clone(),
                    env_var: Some(env),
                }),
                (None, None) => None,
            }
        }
        // A bare `process.env.X`.
        "member_expression" | "subscript_expression" => {
            let env = env_var_name(node, source)?;
            Some(ResolvedChannel {
                value: env.clone(),
                env_var: Some(env),
            })
        }
        _ => None,
    }
}

/// The env var name if `node` is `process.env.NAME` or `process.env['NAME']`.
fn env_var_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        // process.env.NAME
        "member_expression" => {
            let object = node.child_by_field_name("object")?;
            let property = node.child_by_field_name("property")?;
            if is_process_env(object, source) {
                Some(source[property.byte_range()].to_string())
            } else {
                None
            }
        }
        // process.env['NAME']
        "subscript_expression" => {
            let object = node.child_by_field_name("object")?;
            let index = node.child_by_field_name("index")?;
            if is_process_env(object, source) && index.kind() == "string" {
                Some(strip_quotes(&source[index.byte_range()]))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// True when `node` is the `process.env` member access.
fn is_process_env(node: Node<'_>, source: &str) -> bool {
    node.kind() == "member_expression" && {
        let text = source[node.byte_range()].replace(char::is_whitespace, "");
        text == "process.env"
    }
}

fn strip_quotes(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
const APP_NAME = process.env.APP_NAME || 'svc'
export const config = {
    app: { port: Number(process.env.PORT) || 3002 },
    broker: {
        uri: process.env.KAFKA_BROKER || '127.0.0.1:9092',
        topics: {
            topologyEvent: process.env.KAFKA_TOPOLOGY_EVENT_TOPIC || 'topology_event',
            plain: 'static_topic',
            envOnly: process.env.KAFKA_ENV_ONLY,
        },
    },
} as const
export type Config = typeof config
"#;

    #[test]
    fn resolves_env_with_default() {
        let r = resolve_in_config(CONFIG, "config", &["broker", "topics", "topologyEvent"]).unwrap();
        assert_eq!(r.value, "topology_event");
        assert_eq!(r.env_var.as_deref(), Some("KAFKA_TOPOLOGY_EVENT_TOPIC"));
    }

    #[test]
    fn resolves_plain_literal() {
        let r = resolve_in_config(CONFIG, "config", &["broker", "topics", "plain"]).unwrap();
        assert_eq!(r.value, "static_topic");
        assert_eq!(r.env_var, None);
    }

    #[test]
    fn resolves_env_only_to_env_name() {
        let r = resolve_in_config(CONFIG, "config", &["broker", "topics", "envOnly"]).unwrap();
        assert_eq!(r.value, "KAFKA_ENV_ONLY");
        assert_eq!(r.env_var.as_deref(), Some("KAFKA_ENV_ONLY"));
    }

    #[test]
    fn unknown_path_is_none() {
        assert!(resolve_in_config(CONFIG, "config", &["broker", "topics", "nope"]).is_none());
        assert!(resolve_in_config(CONFIG, "config", &["nope"]).is_none());
        assert!(resolve_in_config(CONFIG, "missing", &["broker"]).is_none());
    }

    #[test]
    fn property_segments_drops_this_and_normalises_subscript() {
        assert_eq!(
            property_segments("this.config.broker.topics.topologyEvent"),
            vec!["config", "broker", "topics", "topologyEvent"]
        );
        assert_eq!(
            property_segments("config.broker.topics['topologyEvent']"),
            vec!["config", "broker", "topics", "topologyEvent"]
        );
    }

    #[test]
    fn resolve_channel_expression_end_to_end() {
        let candidates = [("config", CONFIG)];
        let r = resolve_channel_expression(
            "this.config.broker.topics.topologyEvent",
            &candidates,
        )
        .unwrap();
        assert_eq!(r.value, "topology_event");
        assert_eq!(r.env_var.as_deref(), Some("KAFKA_TOPOLOGY_EVENT_TOPIC"));

        // No matching candidate → None.
        assert!(
            resolve_channel_expression("this.config.broker.topics.topologyEvent", &[]).is_none()
        );
    }
}
