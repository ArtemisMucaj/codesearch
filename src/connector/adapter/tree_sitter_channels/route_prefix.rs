//! Trace the URL prefix an Express route is mounted under.
//!
//! A route's *registered* path is not its *served* path: routes are declared on
//! a router in one file and mounted under a prefix in another. The dominant
//! Netatmo shape spans three files and a custom wrapper:
//!
//! ```ts
//! // configuration-router.ts — routes registered on a passed-in router
//! export function configurationRouter(router) { router.get('/:id', …) }
//! // app.ts — the router factory is called and mounted under a prefix
//! httpApp.addRouter('/history', configurationRouter(router))
//! // http-app.ts — the wrapper forwards (path, router) to Express's use()
//! addRouter(path, router) { this.app.use(path, router) }
//! ```
//!
//! The extractor records `/:id`; the served path is `/history/:id`. This module
//! follows the router object across those files to recover the `/history`
//! prefix. It is deliberately a small, syntactic tracer — like [`super::config_resolver`],
//! it handles the shapes that actually occur and returns `None` (leaving the
//! bare path) on anything it does not understand, never guessing a prefix.
//!
//! The trace anchors on the **route-factory function** (the `enclosing_symbol`
//! that registers the routes) and looks for a mount call whose router argument
//! contains a call to it: `mount('/history', configurationRouter(router))`. The
//! mount call is validated as a real mount two ways:
//!  1. it is Express's `.use(prefix, router)` directly, or
//!  2. it is a **wrapper** method whose body forwards its `(path, router)`
//!     parameters to a `.use(path, router)` — verified one hop into the method
//!     body, across sources, so the `addRouter(path, router)` shape counts.
//!
//! Only a single mount level and a string-literal prefix are resolved; nested
//! mounts through intermediate variables and computed prefixes fall through to
//! `None`, leaving the bare registered path.

use tree_sitter::{Node, Parser, Tree};

/// Parse a TypeScript/JavaScript source string. Mirrors the sibling resolver's
/// parser setup so both modules read the same grammar.
fn parse_ts(source: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .ok()?;
    parser.parse(source, None)
}

/// Resolve the mount prefix for a route registered inside `enclosing_symbol`
/// (the router-factory function). `candidates` are `(name, source)` module
/// sources — `name` is ignored here, only the sources are searched, so
/// duplicates across candidate names are harmless. `_route_file` is accepted for
/// symmetry with the trait (and to allow a future same-file scope fallback) but
/// is not needed: the trace anchors on the factory-function name across sources.
///
/// Returns the single-level mount prefix (`/history`), or `None` when the route
/// is mounted at the root, the prefix is computed, or no mount is found.
pub(super) fn resolve_route_prefix(
    _route_file: &str,
    enclosing_symbol: Option<&str>,
    candidates: &[(&str, &str)],
) -> Option<String> {
    // De-dup the source set: config discovery emits one candidate per exported
    // name, so the same file's source recurs. Trace over distinct sources only.
    let mut sources: Vec<&str> = candidates.iter().map(|(_, s)| *s).collect();
    sources.sort_unstable();
    sources.dedup();

    let anchor = enclosing_symbol?;

    for source in &sources {
        let Some(tree) = parse_ts(source) else {
            continue;
        };
        let mut found: Option<MountSite> = None;
        find_mount_referencing(tree.root_node(), source, anchor, &mut found);
        let Some(mount) = found else {
            continue;
        };

        // The call must actually be a mount: `.use`, or a wrapper method that
        // forwards its params to `.use`. A non-mount call that merely happens to
        // pass the router (a middleware, a logger) must not contribute a prefix.
        if !is_mount_call(&mount.method, &sources) {
            continue;
        }

        return Some(mount.prefix);
    }
    None
}

/// A mount call site found in a source: its literal prefix and the method name
/// it invokes (`use`, `addRouter`, …).
struct MountSite {
    prefix: String,
    method: String,
}

/// Walk `node`, looking for a call `M(<string literal>, <arg>)` whose `<arg>`
/// references `anchor` — a call to `anchor(...)` (the route-factory function).
/// Records the first such call.
fn find_mount_referencing(node: Node<'_>, source: &str, anchor: &str, out: &mut Option<MountSite>) {
    if out.is_some() {
        return;
    }
    if node.kind() == "call_expression" {
        if let Some(site) = mount_site_if_referencing(node, source, anchor) {
            *out = Some(site);
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_mount_referencing(child, source, anchor, out);
        if out.is_some() {
            return;
        }
    }
}

/// If `call` is `M('prefix', <arg>)` with a string-literal first argument and a
/// second argument that references `anchor`, return its [`MountSite`].
fn mount_site_if_referencing(call: Node<'_>, source: &str, anchor: &str) -> Option<MountSite> {
    let args = call.child_by_field_name("arguments")?;
    let prefix_arg = nth_named_arg(args, 0)?;
    let router_arg = nth_named_arg(args, 1)?;

    // First argument must be a string literal — a computed prefix is not one we
    // can join into a concrete served path.
    let prefix = string_literal_text(prefix_arg, source)?;
    // A `/`-only mount adds nothing; skip it so the route keeps its bare path
    // rather than gaining a spurious empty segment.
    if prefix == "/" {
        return None;
    }

    // Second argument must reach `anchor`: a call to the route-factory function
    // (`configurationRouter(router)`), possibly nested in another expression.
    if !argument_references(router_arg, source, anchor) {
        return None;
    }

    let method = call_method_name(call, source)?;
    Some(MountSite { prefix, method })
}

/// Whether `arg` contains a call to `anchor(...)` — the route-factory function
/// whose result (a router carrying the registered routes) is being mounted.
/// Descends into nested expressions so `wrap(configurationRouter(r))` still hits.
fn argument_references(arg: Node<'_>, source: &str, anchor: &str) -> bool {
    if arg.kind() == "call_expression" {
        if let Some(func) = arg.child_by_field_name("function") {
            if func.kind() == "identifier" && &source[func.byte_range()] == anchor {
                return true;
            }
        }
    }
    let mut cursor = arg.walk();
    for child in arg.children(&mut cursor) {
        if argument_references(child, source, anchor) {
            return true;
        }
    }
    false
}

/// Whether a call to `method` is an Express mount: `use` itself, or a wrapper
/// method whose body forwards its `(path, router)` parameters to a `.use(path,
/// router)`. The wrapper body is inspected across all sources (the wrapper class
/// may live in its own file).
fn is_mount_call(method: &str, sources: &[&str]) -> bool {
    if method == "use" {
        return true;
    }
    sources
        .iter()
        .any(|source| method_forwards_to_use(source, method))
}

/// Whether `source` defines a method/function named `method` whose body contains
/// a `.use(a, b)` call applying two of the method's own parameters in order — the
/// `addRouter(path, router) { this.app.use(path, router) }` wrapper shape.
fn method_forwards_to_use(source: &str, method: &str) -> bool {
    let Some(tree) = parse_ts(source) else {
        return false;
    };
    let mut result = false;
    find_forwarding_method(tree.root_node(), source, method, &mut result);
    result
}

fn find_forwarding_method(node: Node<'_>, source: &str, method: &str, out: &mut bool) {
    if *out {
        return;
    }
    let is_named_def = matches!(
        node.kind(),
        "method_definition" | "function_declaration" | "function_signature"
    ) && node
        .child_by_field_name("name")
        .map(|n| &source[n.byte_range()] == method)
        .unwrap_or(false);

    if is_named_def {
        // Two ordered parameter names: (path, router) or similar.
        let params = def_parameter_names(node, source);
        if let Some(body) = node.child_by_field_name("body") {
            if body_forwards_params_to_use(body, source, &params) {
                *out = true;
                return;
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_forwarding_method(child, source, method, out);
        if *out {
            return;
        }
    }
}

/// The ordered parameter identifier names of a function/method definition node.
fn def_parameter_names(def: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let Some(params) = def.child_by_field_name("parameters") else {
        return names;
    };
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        // `required_parameter` / `optional_parameter` wrap a `pattern` that is
        // the identifier; a bare `identifier` occurs in plain JS.
        let ident = if child.kind() == "identifier" {
            Some(child)
        } else {
            child
                .child_by_field_name("pattern")
                .filter(|p| p.kind() == "identifier")
        };
        if let Some(ident) = ident {
            names.push(source[ident.byte_range()].to_string());
        }
    }
    names
}

/// Whether `body` contains a `<something>.use(x, y)` call whose two arguments are
/// the first two of `params` in order — the forwarding-wrapper signature.
fn body_forwards_params_to_use(body: Node<'_>, source: &str, params: &[String]) -> bool {
    if params.len() < 2 {
        return false;
    }
    let mut found = false;
    find_use_forwarding(body, source, &params[0], &params[1], &mut found);
    found
}

fn find_use_forwarding(node: Node<'_>, source: &str, p0: &str, p1: &str, out: &mut bool) {
    if *out {
        return;
    }
    if node.kind() == "call_expression" && call_method_name(node, source).as_deref() == Some("use")
    {
        if let Some(args) = node.child_by_field_name("arguments") {
            let a0 = nth_named_arg(args, 0).map(|n| bare_identifier(n, source));
            let a1 = nth_named_arg(args, 1).map(|n| bare_identifier(n, source));
            if a0 == Some(Some(p0.to_string())) && a1 == Some(Some(p1.to_string())) {
                *out = true;
                return;
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_use_forwarding(child, source, p0, p1, out);
        if *out {
            return;
        }
    }
}

/// The method/function name a `call_expression` invokes: the `property` of a
/// `member_expression` callee (`httpApp.addRouter` → `addRouter`, `this.app.use`
/// → `use`), or a bare callee identifier (`configurationRouter` → itself).
fn call_method_name(call: Node<'_>, source: &str) -> Option<String> {
    let func = call.child_by_field_name("function")?;
    match func.kind() {
        "member_expression" => func
            .child_by_field_name("property")
            .map(|p| source[p.byte_range()].to_string()),
        "identifier" => Some(source[func.byte_range()].to_string()),
        _ => None,
    }
}

/// The `index`-th *named* argument of an `arguments` node (skips punctuation).
fn nth_named_arg(args: Node<'_>, index: usize) -> Option<Node<'_>> {
    let mut cursor = args.walk();
    let found = args
        .named_children(&mut cursor)
        .enumerate()
        .find_map(|(i, child)| (i == index).then_some(child));
    found
}

/// The inner text of a string-literal node (`'/history'` → `/history`), or
/// `None` when the node is not a plain string literal (a template literal, a
/// variable). Template literals are excluded: an interpolated prefix is computed
/// and not joinable.
fn string_literal_text(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let raw = &source[node.byte_range()];
    let trimmed = raw.trim();
    let mut chars = trimmed.chars();
    let quote = chars.next().filter(|c| matches!(c, '"' | '\'' | '`'))?;
    if trimmed.len() >= 2 && trimmed.ends_with(quote) {
        Some(trimmed[1..trimmed.len() - 1].to_string())
    } else {
        None
    }
}

/// The identifier name if `node` is a bare identifier, else `None`.
fn bare_identifier(node: Node<'_>, source: &str) -> Option<String> {
    (node.kind() == "identifier").then(|| source[node.byte_range()].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_wrapper_and_factory_mount() {
        // The full Netatmo shape across three "files" (passed as candidates).
        let router_file = r#"
export function configurationRouter(router) {
    router.get('/:homeId/:deviceId/:configFile', handler)
    return router
}
"#;
        let app_file = r#"
import { configurationRouter } from './router/configuration-router'
export function createHttpServer() {
    const httpApp = new HttpApp()
    const router = express.Router()
    httpApp.addRouter('/history', configurationRouter(router))
    return httpApp
}
"#;
        let http_app_file = r#"
export class HttpApp {
    public addRouter(path, router) {
        this.app.use(path, router)
    }
}
"#;
        let candidates = vec![
            ("configuration-router", router_file),
            ("app", app_file),
            ("http-app", http_app_file),
        ];
        let prefix = resolve_route_prefix(
            "src/connector/api/router/configuration-router.ts",
            Some("configurationRouter"),
            &candidates,
        );
        assert_eq!(prefix.as_deref(), Some("/history"));
    }

    #[test]
    fn traces_direct_app_use_factory() {
        // `app.use('/api', makeRouter())` — direct Express mount, no wrapper.
        let router_file = r#"
export function makeRouter(router) {
    router.get('/users', handler)
    return router
}
"#;
        let app_file = r#"
app.use('/api', makeRouter(express.Router()))
"#;
        let candidates = vec![("router", router_file), ("app", app_file)];
        let prefix = resolve_route_prefix("router.ts", Some("makeRouter"), &candidates);
        assert_eq!(prefix.as_deref(), Some("/api"));
    }

    #[test]
    fn no_prefix_when_mounted_at_root() {
        // `app.use('/', router)` contributes nothing — bare path is kept.
        let router_file = r#"
export function makeRouter(router) {
    router.get('/status', handler)
    return router
}
"#;
        let app_file = r#"app.use('/', makeRouter(express.Router()))"#;
        let candidates = vec![("router", router_file), ("app", app_file)];
        let prefix = resolve_route_prefix("router.ts", Some("makeRouter"), &candidates);
        assert_eq!(prefix, None);
    }

    #[test]
    fn no_prefix_when_wrapper_does_not_forward_to_use() {
        // A method that takes (path, router) but does NOT forward them to `.use`
        // is not a mount — e.g. it logs them. No prefix must be inferred.
        let router_file = r#"
export function makeRouter(router) {
    router.get('/x', handler)
    return router
}
"#;
        let app_file = r#"registry.track('/history', makeRouter(express.Router()))"#;
        let http_app_file = r#"
class Registry {
    track(path, router) {
        this.logger.info('registered', path)
    }
}
"#;
        let candidates = vec![
            ("router", router_file),
            ("app", app_file),
            ("registry", http_app_file),
        ];
        let prefix = resolve_route_prefix("router.ts", Some("makeRouter"), &candidates);
        assert_eq!(prefix, None);
    }

    #[test]
    fn no_prefix_when_symbol_absent() {
        // No enclosing symbol → nothing to anchor the trace on.
        let candidates = vec![("app", "app.use('/history', r)")];
        assert_eq!(resolve_route_prefix("router.ts", None, &candidates), None);
    }

    #[test]
    fn computed_prefix_is_not_resolved() {
        // A template-literal / variable prefix cannot be joined — keep bare path.
        let router_file = r#"
export function makeRouter(router) {
    router.get('/x', handler)
    return router
}
"#;
        let app_file = r#"app.use(`${base}/history`, makeRouter(express.Router()))"#;
        let candidates = vec![("router", router_file), ("app", app_file)];
        assert_eq!(
            resolve_route_prefix("router.ts", Some("makeRouter"), &candidates),
            None
        );
    }
}
