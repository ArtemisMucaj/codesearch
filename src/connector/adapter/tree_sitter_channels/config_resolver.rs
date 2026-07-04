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

/// Resolve `this.<param>.<key>` where `<param>` is a constructor parameter of
/// `class_name` that is wired from the caller at the `new class_name(...)` site.
///
/// Traces one hop: find the parameter's position in `class_name`'s constructor,
/// find a `new class_name(...)` call, read the object literal passed at that
/// position, and look up `<key>` to recover the *inner* expression (typically a
/// config access). The inner expression is then resolved through
/// [`resolve_channel_expression`]. `sources` are candidate module sources
/// (`(object_name, source)`) — the class definition, the instantiation site,
/// and the config module may live in different files, so all are searched.
pub(super) fn resolve_via_constructor_param(
    expression: &str,
    class_name: &str,
    sources: &[(&str, &str)],
) -> Option<ResolvedChannel> {
    // `this.topics.gatewayRegistered` → param `topics`, key `gatewayRegistered`.
    let segments = property_segments(expression);
    if segments.len() != 2 {
        return None;
    }
    let param = segments[0].as_str();
    let key = segments[1].as_str();

    let param_index = sources
        .iter()
        .find_map(|(_, source)| constructor_param_index(source, class_name, param))?;

    // Find the object literal passed at that position in a `new Class(...)` call
    // and read the inner expression bound to `key`.
    let inner = sources.iter().find_map(|(_, source)| {
        constructor_arg_object_entry(source, class_name, param_index, key)
    })?;

    resolve_channel_expression(&inner, sources)
}

/// Infer an MQTT topic *pattern* from a topic expression built as a template
/// literal, mapping each `${…}` interpolation to a single-level wildcard `+`.
///
/// Resolves two shapes within `source` (the file the call site lives in):
/// - the expression is itself a template literal (`` `+/response/${host}` `` →
///   `+/response/+`);
/// - the expression is a local variable assigned from a `this.getX(…)` method
///   whose body returns a template literal
///   (`const t = this.getRequestTopic(id)` → `getRequestTopic` returns
///   `` `${id}/request` `` → `+/request`).
///
/// Returns the pattern string. The caller marks the endpoint as a pattern.
pub(super) fn infer_topic_pattern(expression: &str, source: &str) -> Option<String> {
    // Case 1: a bare template literal passed directly.
    let expr = expression.trim();
    if expr.starts_with('`') {
        return template_to_pattern_str(expr);
    }

    // Case 2: a plain identifier bound to `this.getX(...)`; follow the getter.
    if is_plain_identifier(expr) {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .ok()?;
        let tree = parser.parse(source, None)?;
        let getter = variable_getter_method(tree.root_node(), source, expr)?;
        let template = method_returns_template(tree.root_node(), source, &getter)?;
        return template_to_pattern_str(&template);
    }

    None
}

/// True for a bare identifier (`requestTopic`) — no dots, brackets, or calls.
fn is_plain_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        && !s.chars().next().unwrap().is_numeric()
}

/// If `var_name` is declared as `const <var_name> = this.<method>(…)`, return
/// `<method>`.
fn variable_getter_method(node: Node<'_>, source: &str, var_name: &str) -> Option<String> {
    let mut result = None;
    find_variable_getter(node, source, var_name, &mut result);
    result
}

fn find_variable_getter(
    node: Node<'_>,
    source: &str,
    var_name: &str,
    out: &mut Option<String>,
) {
    if out.is_some() {
        return;
    }
    if node.kind() == "variable_declarator" {
        let name_matches = node
            .child_by_field_name("name")
            .map(|n| &source[n.byte_range()] == var_name)
            .unwrap_or(false);
        if name_matches {
            if let Some(value) = node.child_by_field_name("value") {
                if value.kind() == "call_expression" {
                    if let Some(func) = value.child_by_field_name("function") {
                        // `this.<method>` → the method name.
                        if func.kind() == "member_expression" {
                            if let Some(prop) = func.child_by_field_name("property") {
                                *out = Some(source[prop.byte_range()].to_string());
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_variable_getter(child, source, var_name, out);
    }
}

/// The template-literal text returned by method `method_name`, if its body is a
/// single `return <template_string>`.
fn method_returns_template(node: Node<'_>, source: &str, method_name: &str) -> Option<String> {
    let mut result = None;
    find_method_return_template(node, source, method_name, &mut result);
    result
}

fn find_method_return_template(
    node: Node<'_>,
    source: &str,
    method_name: &str,
    out: &mut Option<String>,
) {
    if out.is_some() {
        return;
    }
    if node.kind() == "method_definition" {
        let name_matches = node
            .child_by_field_name("name")
            .map(|n| &source[n.byte_range()] == method_name)
            .unwrap_or(false);
        if name_matches {
            *out = find_return_template(node, source);
            if out.is_some() {
                return;
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_method_return_template(child, source, method_name, out);
    }
}

/// The text of the first `return <template_string>` within `node`.
fn find_return_template(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "return_statement" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "template_string" {
                return Some(source[child.byte_range()].to_string());
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_return_template(child, source) {
            return Some(found);
        }
    }
    None
}

/// Convert a template-literal *string* into an MQTT pattern by re-parsing it and
/// mapping `${…}` → `+`. Returns `None` if there are no interpolations (a plain
/// literal is not a pattern — it should resolve as an exact channel elsewhere).
fn template_to_pattern_str(template: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    // Parse as an expression statement.
    let tree = parser.parse(template, None)?;
    let ts = find_first(tree.root_node(), "template_string")?;
    template_node_to_pattern(ts, template)
}

/// Build the MQTT pattern from a `template_string` node: each
/// `template_substitution` becomes `+`, each `string_fragment` is kept verbatim.
fn template_node_to_pattern(ts: Node<'_>, source: &str) -> Option<String> {
    let mut out = String::new();
    let mut had_substitution = false;
    let mut cursor = ts.walk();
    for child in ts.children(&mut cursor) {
        match child.kind() {
            "string_fragment" => out.push_str(&source[child.byte_range()]),
            "template_substitution" => {
                had_substitution = true;
                out.push('+');
            }
            // Escaped chars inside the template.
            "escape_sequence" => out.push_str(&source[child.byte_range()]),
            _ => {}
        }
    }
    // A pattern only makes sense when at least one `${…}` was replaced; a
    // literal-only template resolves as an exact channel, not a pattern.
    if had_substitution && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

/// First descendant of `node` with the given kind.
fn find_first<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if node.kind() == kind {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_first(child, kind) {
            return Some(found);
        }
    }
    None
}

/// Resolve a channel for an interface-dispatched messaging call
/// (`this.<field>.<method>(…)` where `<field>` is an interface-typed parameter
/// whose concrete implementation performs the real client call).
///
/// Given the call site's source and line, this:
/// 1. reads the type of `<field>` from the enclosing class's constructor
///    (`private broker: Publisher` → `Publisher`),
/// 2. finds the class implementing that interface (`class Broker implements
///    Publisher`),
/// 3. locates `<method>` on that class and the `this.<client>.<method>(<topic>)`
///    call inside it, and
/// 4. infers the topic pattern from `<topic>` (via [`infer_topic_pattern`]).
///
/// Returns the inferred MQTT pattern. `sources` are candidate module sources.
pub(super) fn resolve_via_interface(
    call_site_source: &str,
    call_line: u32,
    sources: &[(&str, &str)],
) -> Option<String> {
    let (field, method) = call_receiver_and_method(call_site_source, call_line)?;
    let interface = field_type_in_source(call_site_source, &field)?;

    // Find the implementing class and infer the pattern from its method body.
    sources.iter().find_map(|(_, source)| {
        let class = class_implementing(source, &interface)?;
        let topic_arg = method_client_topic_arg(source, &class, &method)?;
        infer_topic_pattern(&topic_arg, source)
    })
}

/// The `(field, method)` of a `this.<field>.<method>(…)` call at `line`.
fn call_receiver_and_method(source: &str, line: u32) -> Option<(String, String)> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    let tree = parser.parse(source, None)?;

    let mut result = None;
    find_this_field_call_at(tree.root_node(), source, line, &mut result);
    result
}

fn find_this_field_call_at(
    node: Node<'_>,
    source: &str,
    line: u32,
    out: &mut Option<(String, String)>,
) {
    if out.is_some() {
        return;
    }
    if node.kind() == "call_expression" && node.start_position().row as u32 + 1 == line {
        if let Some(func) = node.child_by_field_name("function") {
            // `this.<field>.<method>`
            if func.kind() == "member_expression" {
                if let (Some(object), Some(method)) = (
                    func.child_by_field_name("object"),
                    func.child_by_field_name("property"),
                ) {
                    if object.kind() == "member_expression" {
                        let is_this = object
                            .child_by_field_name("object")
                            .map(|o| o.kind() == "this")
                            .unwrap_or(false);
                        if let (true, Some(field)) =
                            (is_this, object.child_by_field_name("property"))
                        {
                            *out = Some((
                                source[field.byte_range()].to_string(),
                                source[method.byte_range()].to_string(),
                            ));
                            return;
                        }
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_this_field_call_at(child, source, line, out);
    }
}

/// The declared type of constructor parameter `field` anywhere in `source`
/// (`private broker: Publisher` → `Publisher`).
fn field_type_in_source(source: &str, field: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut result = None;
    find_param_type(tree.root_node(), source, field, &mut result);
    result
}

fn find_param_type(node: Node<'_>, source: &str, field: &str, out: &mut Option<String>) {
    if out.is_some() {
        return;
    }
    if is_parameter_node(node.kind()) && parameter_name(node, source).as_deref() == Some(field) {
        if let Some(ann) = node.child_by_field_name("type") {
            // type_annotation node → its type_identifier text (trim leading `:`).
            let ty = source[ann.byte_range()]
                .trim_start_matches(':')
                .trim()
                .to_string();
            if !ty.is_empty() {
                *out = Some(ty);
                return;
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_param_type(child, source, field, out);
    }
}

/// The name of the class in `source` that `implements <interface>`.
fn class_implementing(source: &str, interface: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut result = None;
    find_implementing_class(tree.root_node(), source, interface, &mut result);
    result
}

fn find_implementing_class(
    node: Node<'_>,
    source: &str,
    interface: &str,
    out: &mut Option<String>,
) {
    if out.is_some() {
        return;
    }
    if node.kind() == "class_declaration" {
        let implements = find_first(node, "implements_clause")
            .map(|c| {
                let text = &source[c.byte_range()];
                text.split_whitespace().any(|t| t == interface)
            })
            .unwrap_or(false);
        if implements {
            if let Some(name) = node.child_by_field_name("name") {
                *out = Some(source[name.byte_range()].to_string());
                return;
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_implementing_class(child, source, interface, out);
    }
}

/// Inside `class_name.method_name`, the first argument text of a
/// `this.<client>.<method_name>(<arg>, …)` call — the client-level topic.
fn method_client_topic_arg(source: &str, class_name: &str, method_name: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let class = find_class(tree.root_node(), source, class_name)?;
    let method = class_method(class, source, method_name)?;

    let mut result = None;
    find_inner_client_call_arg(method, source, method_name, &mut result);
    result
}

/// The `method_definition` named `method_name` within a class node.
fn class_method<'a>(class: Node<'a>, source: &str, method_name: &str) -> Option<Node<'a>> {
    let body = class.child_by_field_name("body")?;
    let mut cursor = body.walk();
    // Bound to a local so `cursor` outlives the borrow (a tail-position `find`
    // would drop it too early).
    let found = body.children(&mut cursor).find(|child| {
        child.kind() == "method_definition"
            && child
                .child_by_field_name("name")
                .map(|n| &source[n.byte_range()] == method_name)
                .unwrap_or(false)
    });
    found
}

fn find_inner_client_call_arg(
    node: Node<'_>,
    source: &str,
    method_name: &str,
    out: &mut Option<String>,
) {
    if out.is_some() {
        return;
    }
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            // `this.<client>.<method_name>(...)`
            let calls_method = func.kind() == "member_expression"
                && func
                    .child_by_field_name("property")
                    .map(|p| &source[p.byte_range()] == method_name)
                    .unwrap_or(false);
            let on_this_field = func
                .child_by_field_name("object")
                .map(|o| o.kind() == "member_expression")
                .unwrap_or(false);
            if calls_method && on_this_field {
                if let Some(args) = node.child_by_field_name("arguments") {
                    if let Some(first) = nth_call_argument(args, 0) {
                        *out = Some(source[first.byte_range()].to_string());
                        return;
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_inner_client_call_arg(child, source, method_name, out);
    }
}

/// The zero-based position of parameter `param` in `class_name`'s constructor.
fn constructor_param_index(source: &str, class_name: &str, param: &str) -> Option<usize> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    let tree = parser.parse(source, None)?;

    let class = find_class(tree.root_node(), source, class_name)?;
    let ctor = find_constructor(class, source)?;
    let params = ctor.child_by_field_name("parameters")?;

    let mut cursor = params.walk();
    let mut index = 0usize;
    for child in params.children(&mut cursor) {
        if !is_parameter_node(child.kind()) {
            continue;
        }
        if parameter_name(child, source).as_deref() == Some(param) {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// From a `new class_name(...)` call, take the argument at `arg_index` (expected
/// to be an object literal) and return the source text of the value bound to
/// `key`.
fn constructor_arg_object_entry(
    source: &str,
    class_name: &str,
    arg_index: usize,
    key: &str,
) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    let tree = parser.parse(source, None)?;

    let mut result = None;
    find_new_expression(tree.root_node(), source, class_name, &mut |args| {
        if result.is_some() {
            return;
        }
        let Some(arg) = nth_call_argument(args, arg_index) else {
            return;
        };
        if let Some(object) = unwrap_to_object(arg) {
            if let Some(value) = object_property(object, source, key) {
                result = Some(source[value.byte_range()].to_string());
            }
        }
    });
    result
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

/// Find the `class_declaration` named `name`.
fn find_class<'a>(node: Node<'a>, source: &str, name: &str) -> Option<Node<'a>> {
    if node.kind() == "class_declaration" {
        if let Some(id) = node.child_by_field_name("name") {
            if &source[id.byte_range()] == name {
                return Some(node);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(found) = find_class(child, source, name) {
            return Some(found);
        }
    }
    None
}

/// The constructor `method_definition` within a class node.
fn find_constructor<'a>(class: Node<'a>, source: &str) -> Option<Node<'a>> {
    let body = class.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        let is_ctor = child.kind() == "method_definition"
            && child
                .child_by_field_name("name")
                .map(|n| &source[n.byte_range()] == "constructor")
                .unwrap_or(false);
        if is_ctor {
            return Some(child);
        }
    }
    None
}

/// True for a constructor parameter node kind (with or without modifiers,
/// defaults, or optionality).
fn is_parameter_node(kind: &str) -> bool {
    matches!(kind, "required_parameter" | "optional_parameter")
}

/// The declared name of a constructor parameter, e.g. `topics` in
/// `private topics: {…}`. Reads the `pattern` field (an identifier).
fn parameter_name(param: Node<'_>, source: &str) -> Option<String> {
    let pattern = param.child_by_field_name("pattern")?;
    Some(source[pattern.byte_range()].to_string())
}

/// Visit every `new class_name(...)` expression, calling `f` with its
/// `arguments` node.
fn find_new_expression(
    node: Node<'_>,
    source: &str,
    class_name: &str,
    f: &mut impl FnMut(Node<'_>),
) {
    if node.kind() == "new_expression" {
        if let Some(ctor) = node.child_by_field_name("constructor") {
            if &source[ctor.byte_range()] == class_name {
                if let Some(args) = node.child_by_field_name("arguments") {
                    f(args);
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_new_expression(child, source, class_name, f);
    }
}

/// The `index`-th positional argument of an `arguments` node (skipping the
/// parentheses and commas).
fn nth_call_argument<'a>(args: Node<'a>, index: usize) -> Option<Node<'a>> {
    let mut cursor = args.walk();
    // Bound to a local so `cursor` outlives the borrow (a tail-position
    // iterator would drop it too early). `Node` is `Copy`, so the result does
    // not depend on the cursor.
    let found = args
        .named_children(&mut cursor)
        .enumerate()
        .find_map(|(i, child)| (i == index).then_some(child));
    found
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

    // A class receives its topics through a constructor param, wired from the
    // config at the `new Class(...)` site — the producer indirection.
    const CLASS_SOURCE: &str = r#"
import { AsyncProducer } from '@backend/kafkajs'
export class DomainEvent {
    constructor(
        private producer: AsyncProducer,
        private topics: { topologyEvent: string },
    ) { }
    async fire(event) {
        await this.producer.produce(this.topics.topologyEvent, JSON.stringify(event))
    }
}
"#;
    const INSTANTIATION_SOURCE: &str = r#"
function build() {
    const d = new DomainEvent(this.producer, {
        topologyEvent: this.config.broker.topics.topologyEvent,
    })
}
"#;

    #[test]
    fn resolves_topic_through_constructor_param() {
        let sources = [
            ("DomainEvent", CLASS_SOURCE),
            ("application", INSTANTIATION_SOURCE),
            ("config", CONFIG),
        ];
        let r =
            resolve_via_constructor_param("this.topics.topologyEvent", "DomainEvent", &sources)
                .unwrap();
        assert_eq!(r.value, "topology_event");
        assert_eq!(r.env_var.as_deref(), Some("KAFKA_TOPOLOGY_EVENT_TOPIC"));
    }

    #[test]
    fn constructor_param_index_is_positional() {
        // `topics` is the second constructor parameter.
        assert_eq!(
            constructor_param_index(CLASS_SOURCE, "DomainEvent", "topics"),
            Some(1)
        );
        assert_eq!(
            constructor_param_index(CLASS_SOURCE, "DomainEvent", "producer"),
            Some(0)
        );
        assert_eq!(
            constructor_param_index(CLASS_SOURCE, "DomainEvent", "nope"),
            None
        );
    }

    #[test]
    fn constructor_trace_gives_up_cleanly() {
        // Unknown class, or a param not wired at the call site.
        let sources = [("DomainEvent", CLASS_SOURCE), ("config", CONFIG)];
        // No `new DomainEvent(...)` in these sources → None.
        assert!(
            resolve_via_constructor_param("this.topics.topologyEvent", "DomainEvent", &sources)
                .is_none()
        );
        assert!(resolve_via_constructor_param(
            "this.topics.topologyEvent",
            "UnknownClass",
            &sources
        )
        .is_none());
    }

    const BROKER_SOURCE: &str = r#"
class Broker {
    getRequestTopic(gatewayId) {
        return `${gatewayId}/request`
    }
    getResponseTopic(gatewayId) {
        return `${gatewayId}/response/${this.hostname}`
    }
    getSubscribeTopic() {
        return `+/response/${this.hostname}`
    }
    async publish(gatewayId) {
        const requestTopic = this.getRequestTopic(gatewayId)
        await this.mqttClient.publish(requestTopic, payload)
    }
}
"#;

    #[test]
    fn infers_pattern_from_direct_template_literal() {
        // `${...}` → `+`, static fragments kept.
        assert_eq!(
            infer_topic_pattern("`+/response/${this.hostname}`", BROKER_SOURCE),
            Some("+/response/+".to_string())
        );
    }

    #[test]
    fn infers_pattern_via_getter_variable() {
        // requestTopic = this.getRequestTopic(id) → `${id}/request` → +/request
        assert_eq!(
            infer_topic_pattern("requestTopic", {
                // The variable declaration + getter live in the same source.
                &format!(
                    "{BROKER_SOURCE}\nfunction f(){{ const requestTopic = this.getRequestTopic(x) }}"
                )
            }),
            Some("+/request".to_string())
        );
    }

    #[test]
    fn getter_with_two_interpolations() {
        let src = format!(
            "{BROKER_SOURCE}\nfunction f(){{ const responseTopic = this.getResponseTopic(x) }}"
        );
        assert_eq!(
            infer_topic_pattern("responseTopic", &src),
            Some("+/response/+".to_string())
        );
    }

    #[test]
    fn no_interpolation_is_not_a_pattern() {
        // A template with no `${…}` is a plain literal, not a pattern.
        assert_eq!(infer_topic_pattern("`static/topic`", BROKER_SOURCE), None);
        // An unknown identifier resolves to nothing.
        assert_eq!(infer_topic_pattern("unknownVar", BROKER_SOURCE), None);
    }

    const IFACE_CALLER: &str = r#"
export class InteractionModel {
    constructor(
        private broker: Publisher,
        private logger: Logger,
    ) {}
    async request(node) {
        await this.broker.publish(gatewayId, node, sessionId, requestMessage)
    }
}
"#;
    const IFACE_IMPL: &str = r#"
export class Broker implements Publisher {
    getRequestTopic(gatewayId) {
        return `${gatewayId}/request`
    }
    async publish(gatewayId, node) {
        const requestTopic = this.getRequestTopic(gatewayId)
        await this.mqttClient.publish(requestTopic, payload)
    }
}
"#;

    #[test]
    fn resolves_pattern_through_interface_dispatch() {
        let sources = [("InteractionModel", IFACE_CALLER), ("Broker", IFACE_IMPL)];
        // The publish call is on line 8 of IFACE_CALLER (this.broker.publish).
        let pattern = resolve_via_interface(IFACE_CALLER, 8, &sources);
        assert_eq!(pattern.as_deref(), Some("+/request"));
    }

    #[test]
    fn interface_trace_reads_field_type_and_impl() {
        assert_eq!(
            field_type_in_source(IFACE_CALLER, "broker").as_deref(),
            Some("Publisher")
        );
        assert_eq!(
            class_implementing(IFACE_IMPL, "Publisher").as_deref(),
            Some("Broker")
        );
    }

    #[test]
    fn interface_trace_gives_up_without_impl() {
        // Only the caller — no implementing class in scope.
        let sources = [("InteractionModel", IFACE_CALLER)];
        assert!(resolve_via_interface(IFACE_CALLER, 8, &sources).is_none());
    }
}
