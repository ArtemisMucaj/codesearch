//! Enrich extracted channel endpoints with cross-file resolution.
//!
//! The tree-sitter extractor sees one call site at a time, so a channel wired
//! through config (`this.config.broker.topics.orders`) or a generic client
//! method (`.produce()`, `.subscribe()`) comes out unresolved and low
//! confidence. This pass runs once per repository, after extraction and SCIP
//! import, and uses two cross-file signals:
//!
//! 1. **Library confirmation** — the SCIP reference at the same call site
//!    carries the package the callee is defined in (`kafkajs`). If it
//!    maps to a known client library, the endpoint is confirmed and its
//!    confidence raised: a bare `.produce()` is no longer a guess.
//! 2. **Config value resolution** — an unresolved property-access channel is
//!    run through the [`ChannelResolver`] against the repo's config modules,
//!    recovering the concrete topic string and the env var behind it.

use std::collections::HashMap;
use std::sync::Arc;

use crate::application::{normalize_channel, ChannelResolver};
use crate::domain::{ChannelEndpoint, ChannelRole, EndpointSource, Protocol, SymbolReference};

/// Confidence assigned to an endpoint once its library is confirmed via SCIP —
/// high, because the receiver's type (not just a method name) now backs it.
const CONFIRMED_CONFIDENCE: f32 = 0.9;

/// Confidence for an endpoint *originated* from a SCIP package reference alone
/// (see [`ResolveChannelsUseCase::synthesize_from_packages`]). The package
/// backs the protocol and the method backs the role, but no channel value is
/// recoverable from SCIP, so it is unresolved and scored below a confirmed
/// literal — enough to surface the call site, not enough to fabricate an edge.
const PACKAGE_DRIVEN_CONFIDENCE: f32 = 0.6;

/// A `(config_object_name, module_source)` pair the resolver can search.
pub type ConfigCandidate = (String, String);

/// Maps a messaging client library's package name to the protocol it speaks.
/// This is the one place messaging-library knowledge lives; adding a client is
/// one entry. HTTP server frameworks are handled separately by
/// [`http_server_package`] because they carry a verb and a fixed role.
fn protocol_for_package(package: &str) -> Option<Protocol> {
    // Match on a substring so scoped/wrapped variants resolve too
    // (`kafkajs`, `@confluentinc/kafka-javascript`, `node-rdkafka`).
    let p = package.to_ascii_lowercase();
    if p.contains("kafka") || p.contains("rdkafka") {
        Some(Protocol::Kafka)
    } else if p.contains("mqtt") {
        Some(Protocol::Mqtt)
    } else if p.contains("amqp") || p.contains("rabbit") {
        Some(Protocol::Amqp)
    } else if p.contains("grpc") {
        Some(Protocol::Grpc)
    } else {
        None
    }
}

/// Whether a package is a known HTTP **server** framework — a route method
/// resolving into it (`IRouter#get`, `FastifyInstance#post`) marks an HTTP
/// server endpoint (a consumer). Client libraries (axios, got) are deliberately
/// excluded: their calls are producers and are already covered by syntactic
/// detectors, and mislabelling one as a server route would invert the edge.
fn http_server_package(package: &str) -> bool {
    let p = package.to_ascii_lowercase();
    // express resolves route calls into `@types/express-serve-static-core`
    // (`IRouter#get`), so match that and the umbrella `express` package.
    p.contains("express") || p.contains("fastify")
}

/// The uppercased HTTP verb a route-registration method names
/// (`IRouter#get` → `GET`, `FastifyInstance#route` → `ANY`), or `None` when the
/// call is not a route registration. `all`/`route` register a handler for any
/// verb and report as `ANY`, mirroring the syntactic detectors.
///
/// Two guards keep this precise:
/// - The **receiver type** must be a router/app/server, not a request/response.
///   `express.Request` also has a `.get('Header')` method resolving into the
///   same package; gating on the receiver stops a header read masquerading as a
///   `GET /Header` route.
/// - `use` is excluded: it registers *middleware*, not a route — its argument is
///   almost always a handler (`app.use(express.json())`), not a path, so it
///   would flood the report. The rare path-mount (`app.use('/api', r)`) is not
///   worth that noise.
fn http_verb_for_symbol(callee_symbol: &str) -> Option<&'static str> {
    // Receiver type gate: only route-holding types register routes.
    if let Some((receiver, _)) = callee_symbol.rsplit_once('#') {
        let r = receiver.to_ascii_lowercase();
        let is_route_holder = r.contains("router")
            || r.contains("application")
            || r.contains("fastify")
            || r.ends_with("app")
            || r.contains("server");
        // A request/response/message receiver is never a route holder even if it
        // superficially matches (e.g. a `…ServerResponse`); exclude explicitly.
        let is_message = r.contains("request")
            || r.contains("response")
            || r.contains("incomingmessage")
            || r.contains("reply");
        if !is_route_holder || is_message {
            return None;
        }
    }

    let method = callee_symbol
        .rsplit(['#', '.', ':'])
        .next()
        .unwrap_or(callee_symbol);
    match method.to_ascii_lowercase().as_str() {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        "head" => Some("HEAD"),
        "options" => Some("OPTIONS"),
        "all" | "route" => Some("ANY"),
        _ => None,
    }
}

/// Whether a package name maps to a known messaging client library or an HTTP
/// server framework — the signal that a call resolving into it can originate a
/// channel endpoint. Exposed so the indexing pipeline can decide whether
/// config-candidate discovery / call-site re-reading is worth running for a repo
/// whose messaging or routing is done through wrappers or field-held routers.
pub fn is_messaging_package(package: &str) -> bool {
    protocol_for_package(package).is_some() || http_server_package(package)
}

/// Classify a messaging-client call into the channel role it implies, from the
/// normalized SCIP callee symbol (`Consumer#connect`, `Producer#send`, `send`).
///
/// This is what lets a call resolving into a client library get a
/// producer/consumer role without the extractor having matched a specific call
/// shape — so a *wrapper* around the client (a fork like `@backend/kafkajs`, or
/// a project's own `KafkaClientAdapter`) is classified purely by the library
/// symbol it reaches.
///
/// The **method** must name a channel operation for a call to originate an
/// endpoint at all — pure lifecycle methods (`<constructor>`, `close`, `commit`)
/// carry no channel and are ignored, so a consumer's whole method surface does
/// not become a wall of endpoints. The **receiver type**
/// (`Consumer`/`Producer`/`Subscriber`/`Publisher`) then disambiguates the two
/// verbs that both sides share:
///
/// - `connect` — the channel operation on the *consumer* side of the
///   librdkafka-style wrappers (`consumer.connect({ topics })`), but pure setup
///   on the producer side; counted only when the receiver is a consumer.
/// - `run`/`each` — the consumer's message loop; counted only for a consumer.
///
/// Returns `None` when nothing directional can be established, so a call never
/// fabricates an endpoint on a guess.
fn role_for_symbol(callee_symbol: &str) -> Option<ChannelRole> {
    let (receiver, method) = match callee_symbol.rsplit_once('#') {
        Some((recv, m)) => (Some(recv), m),
        None => (None, callee_symbol),
    };

    let receiver_role = receiver.and_then(|r| {
        let r = r.to_ascii_lowercase();
        if r.contains("producer") || r.contains("publisher") {
            Some(ChannelRole::Producer)
        } else if r.contains("consumer") || r.contains("subscriber") {
            Some(ChannelRole::Consumer)
        } else {
            None
        }
    });

    let m = method.to_ascii_lowercase();
    match m.as_str() {
        // Unambiguous channel verbs — role from the verb itself.
        "send" | "produce" | "publish" | "emit" | "batchsend" => Some(ChannelRole::Producer),
        "subscribe" | "consume" | "consumeone" | "eachmessage" | "eachbatch" => {
            Some(ChannelRole::Consumer)
        }
        // Verbs shared by both sides: a channel operation only for a consumer
        // (its subscription/read loop), so require a consumer receiver type.
        "connect" | "run" | "each" if receiver_role == Some(ChannelRole::Consumer) => {
            Some(ChannelRole::Consumer)
        }
        // Everything else (lifecycle: constructor, close, commit, offsets) is
        // not a channel operation.
        _ => None,
    }
}

pub struct ResolveChannelsUseCase {
    resolver: Arc<dyn ChannelResolver>,
}

impl ResolveChannelsUseCase {
    pub fn new(resolver: Arc<dyn ChannelResolver>) -> Self {
        Self { resolver }
    }

    /// Enrich `endpoints` in place-ish (consumes and returns them), then append
    /// any endpoints originated from SCIP package references alone.
    ///
    /// - `repository_id` labels synthesized endpoints (the extracted ones carry
    ///   their own).
    /// - `refs_by_file` is the SCIP call graph keyed by the file the reference
    ///   occurs in (as produced by the importer).
    /// - `config_candidates` are the config/class modules discovered in the repo.
    /// - `sources_by_file` maps a repo file path to its source, so a call site
    ///   can be re-read for template/interface pattern inference.
    pub fn resolve(
        &self,
        repository_id: &str,
        endpoints: Vec<ChannelEndpoint>,
        refs_by_file: &HashMap<String, Vec<SymbolReference>>,
        config_candidates: &[ConfigCandidate],
        sources_by_file: &HashMap<String, String>,
    ) -> Vec<ChannelEndpoint> {
        let mut resolved: Vec<ChannelEndpoint> = endpoints
            .into_iter()
            .map(|endpoint| {
                self.resolve_one(endpoint, refs_by_file, config_candidates, sources_by_file)
            })
            .collect();

        // Originate endpoints for messaging calls the extractor could not shape
        // but SCIP attributes to a client library (wrapper/fork call sites).
        // These carry the real channel argument read from the call site, so they
        // go through the same resolution passes — a wrapper consumer's config
        // topic resolves exactly as an extracted one would.
        let synthesized = synthesize_from_packages(
            &*self.resolver,
            repository_id,
            &resolved,
            refs_by_file,
            sources_by_file,
        );
        for endpoint in synthesized {
            resolved.push(self.resolve_one(
                endpoint,
                refs_by_file,
                config_candidates,
                sources_by_file,
            ));
        }

        // Fan-out expansion: an HTTP route registered inside a loop over a local
        // route table (`for (const r of routes) router.post(r.path, …)`) leaves
        // one unresolved endpoint whose channel is `r.path`. Expand it into one
        // resolved endpoint per array element, replacing the placeholder.
        self.expand_loop_registered_routes(&mut resolved, sources_by_file);

        resolved
    }

    /// Replace each unresolved HTTP endpoint whose channel is a loop-variable
    /// field access with one resolved endpoint per element of the route table it
    /// iterates (see [`ChannelResolver::resolve_loop_array_paths`]).
    fn expand_loop_registered_routes(
        &self,
        endpoints: &mut Vec<ChannelEndpoint>,
        sources_by_file: &HashMap<String, String>,
    ) {
        let mut expanded = Vec::new();
        endpoints.retain(|endpoint| {
            // Only unresolved HTTP endpoints whose raw channel is a `<var>.<field>`
            // access are candidates; anything already resolved stays as-is.
            if endpoint.protocol() != Protocol::Http
                || endpoint.is_resolved()
                || !endpoint.channel_raw().contains('.')
            {
                return true;
            }
            let Some(source) = sources_by_file.get(endpoint.file_path()) else {
                return true;
            };
            let Some(paths) = self.resolver.resolve_loop_array_paths(
                endpoint.channel_raw(),
                source,
                endpoint.line(),
            ) else {
                return true;
            };

            // One resolved endpoint per route-table entry. The id stays keyed on
            // the call site plus the path so a re-index upserts the same rows.
            for path in paths {
                let (host, normalized, is_pattern) = normalize_channel(Protocol::Http, &path);
                let mut route = endpoint
                    .clone()
                    .with_id(format!("{}:{}", endpoint.id(), normalized))
                    .resolve_channel(path, normalized);
                if let Some(host) = host {
                    route = route.with_host(host);
                }
                if is_pattern {
                    route = route.as_pattern();
                }
                expanded.push(route);
            }
            // Drop the placeholder — it has been replaced by the expansion.
            false
        });
        endpoints.append(&mut expanded);
    }

    fn resolve_one(
        &self,
        mut endpoint: ChannelEndpoint,
        refs_by_file: &HashMap<String, Vec<SymbolReference>>,
        config_candidates: &[ConfigCandidate],
        sources_by_file: &HashMap<String, String>,
    ) -> ChannelEndpoint {
        // 1. Library confirmation via the SCIP reference at this call site.
        if let Some(package) =
            library_package_at(endpoint.file_path(), endpoint.line(), refs_by_file)
        {
            if let Some(protocol) = protocol_for_package(&package) {
                if protocol == endpoint.protocol() {
                    endpoint = endpoint
                        .with_library(package)
                        .confirmed()
                        .with_confidence(CONFIRMED_CONFIDENCE);
                }
            }
        }

        // 2. Config value resolution for unresolved property-access channels.
        // The enclosing class lets the resolver trace a `this.<param>.<key>`
        // access through the class constructor to the config it is wired from.
        if !endpoint.is_resolved() {
            let class = enclosing_class_at(endpoint.file_path(), endpoint.line(), refs_by_file);
            if let Some(resolved) = self.resolver.resolve_config_expression(
                endpoint.channel_raw(),
                class.as_deref(),
                config_candidates,
            ) {
                let (host, normalized, is_pattern) =
                    normalize_channel(endpoint.protocol(), &resolved.value);
                endpoint = endpoint.resolve_channel(resolved.value, normalized);
                // Carry the metadata normalization recovered: an HTTP host from a
                // config-driven URL, and MQTT wildcard state so the join treats
                // it as a pattern (both are dropped otherwise).
                if let Some(host) = host {
                    endpoint = endpoint.with_host(host);
                }
                if is_pattern {
                    endpoint = endpoint.as_pattern();
                }
                if let Some(env) = resolved.env_var {
                    endpoint = endpoint.with_env_var(env);
                }
            }
        }

        // 3. Topic-pattern inference for a channel computed at runtime — a
        // template literal (`${id}/request`), a getter-backed variable, or an
        // interface-dispatched client call. Needs the call site's own source.
        if !endpoint.is_resolved() {
            if let Some(source) = sources_by_file.get(endpoint.file_path()) {
                if let Some(pattern) = self.resolver.resolve_topic_pattern(
                    endpoint.channel_raw(),
                    source,
                    endpoint.line(),
                    config_candidates,
                ) {
                    let (_, normalized, _) = normalize_channel(endpoint.protocol(), &pattern);
                    endpoint = endpoint.resolve_channel(pattern, normalized).as_pattern();
                }
            }
        }

        endpoint
    }
}

/// The enclosing class/scope of the call at `file:line`, from the SCIP
/// references there. Used to trace constructor-parameter channels.
fn enclosing_class_at(
    file_path: &str,
    line: u32,
    refs_by_file: &HashMap<String, Vec<SymbolReference>>,
) -> Option<String> {
    let refs = refs_by_file.get(file_path)?;
    refs.iter()
        .filter(|r| r.reference_line().abs_diff(line) <= CALL_SITE_LINE_WINDOW)
        .find_map(|r| r.enclosing_scope().map(str::to_string))
}

/// How far above/below the endpoint's line a SCIP reference may sit and still
/// be considered the same call site. A method call and its channel argument
/// often land on adjacent lines when the call is wrapped across lines
/// (`produce(\n  topic, …)`), so an exact-line match misses them.
const CALL_SITE_LINE_WINDOW: u32 = 2;

/// The client-library package backing the call at `file:line`, if any SCIP
/// reference within [`CALL_SITE_LINE_WINDOW`] lines resolves into one.
///
/// A call site produces several references (the method, each argument); only
/// some carry a third-party package. We take the nearest reference whose
/// package maps to a known client library, so the app's own wrapper classes
/// (whose package is the project itself) do not mask the underlying client.
fn library_package_at(
    file_path: &str,
    line: u32,
    refs_by_file: &HashMap<String, Vec<SymbolReference>>,
) -> Option<String> {
    let refs = refs_by_file.get(file_path)?;
    refs.iter()
        .filter(|r| r.reference_line().abs_diff(line) <= CALL_SITE_LINE_WINDOW)
        .filter_map(|r| {
            let package = r.callee_package()?;
            // Only packages that map to a client library count — this skips the
            // project's own package on wrapper calls.
            protocol_for_package(package).map(|_| (r.reference_line(), package.to_string()))
        })
        // Nearest line wins when several qualify.
        .min_by_key(|(ref_line, _)| ref_line.abs_diff(line))
        .map(|(_, package)| package)
}

/// Originate endpoints for messaging call sites the extractor never shaped, by
/// reading them straight off the SCIP call graph.
///
/// A wrapper or fork (e.g. `@backend/kafkajs`) exposes its own method names
/// (`consumer.connect({ topics })`) that no framework detector matches. But the
/// call still resolves, via SCIP, into a package whose name reveals the
/// protocol, and into a symbol whose type/method reveals the role. That is
/// enough to record *that* a file produces to / consumes from a channel — the
/// endpoint is unresolved (SCIP carries no argument value, so the concrete
/// topic is unknown) and library-confirmed, so it surfaces the call site
/// without fabricating a matchable channel string.
///
/// De-dup: a reference at a `(file, line)` already covered by an extracted
/// endpoint of the same role is skipped, so a literal `produce("t", …)` the
/// extractor resolved is never shadowed by a weaker synthesized twin. Multiple
/// qualifying references on the same line collapse to one endpoint per role.
///
/// The channel argument (`this.topics`, `cfg.topic`, …) is read from the call
/// site's own source when available, so the endpoint carries a real expression
/// the resolution passes can trace to a config value. When the source or the
/// argument cannot be read, the SCIP receiver type is a descriptive fallback and
/// the endpoint stays unresolved.
fn synthesize_from_packages(
    resolver: &dyn ChannelResolver,
    repository_id: &str,
    existing: &[ChannelEndpoint],
    refs_by_file: &HashMap<String, Vec<SymbolReference>>,
    sources_by_file: &HashMap<String, String>,
) -> Vec<ChannelEndpoint> {
    use std::collections::HashSet;

    // Call sites the *extractor* already owns, as (file, line, role). A ref
    // within [`CALL_SITE_LINE_WINDOW`] lines of one — same file and role — is
    // treated as the same call site (the method call and its channel argument
    // straddle lines when wrapped), so an extracted literal is never shadowed
    // by a weaker synthesized twin.
    //
    // Only tree-sitter endpoints count as covering. Prior *synthesized*
    // endpoints (`Config` source) reloaded from storage must not — otherwise a
    // re-index would skip re-synthesis and leave a stale, unresolved channel in
    // place instead of re-deriving a better one. Re-synthesis is idempotent: the
    // deterministic id below upserts the same row.
    let covered: Vec<(&str, u32, ChannelRole)> = existing
        .iter()
        .filter(|e| e.source() == EndpointSource::TreeSitter)
        .map(|e| (e.file_path(), e.line(), e.role()))
        .collect();
    let is_covered = |file: &str, line: u32, role: ChannelRole| {
        covered
            .iter()
            .any(|&(f, l, r)| f == file && r == role && l.abs_diff(line) <= CALL_SITE_LINE_WINDOW)
    };

    let mut synthesized = Vec::new();
    let mut seen: HashSet<(String, u32, ChannelRole)> = HashSet::new();

    for (file_path, refs) in refs_by_file {
        for r in refs {
            let Some(package) = r.callee_package() else {
                continue;
            };
            let Some(kind) = classify_ref(package, r.callee_symbol()) else {
                continue;
            };
            let role = kind.role();

            let line = r.reference_line();
            if is_covered(file_path, line, role) {
                continue;
            }
            // One endpoint per (file, line, role): a wrapper call resolves into
            // several references (method + args) on the same line.
            if !seen.insert((file_path.clone(), line, role)) {
                continue;
            }

            // Prefer the real channel argument read from the call site (so the
            // resolution passes can trace it to a config value); fall back to
            // the SCIP receiver type as a descriptive label otherwise. For an
            // HTTP route the argument is the path literal, which resolves at once.
            let call_arg = sources_by_file
                .get(file_path)
                .and_then(|source| resolver.channel_argument_at(source, line));

            // A read-loop verb (`consumeOne`, `run`, `each`…) carries no channel
            // argument — the topic was set at the `subscribe`/`connect` call the
            // loop reads from. With no argument to read, synthesizing here would
            // only label the endpoint with the receiver type (`Consumer`), a
            // never-resolvable duplicate of the real subscription. Skip it.
            if call_arg.is_none() && read_loop_verb(r.callee_symbol()) {
                continue;
            }

            // Express's `app.get(name)` settings getter resolves into the same
            // route type as a real `app.get('/p', handler)`, so SCIP alone can't
            // tell them apart. A route always carries a handler as its second
            // argument; when the call site is available and has none, this is a
            // settings read, not a route — skip it.
            if matches!(kind, RefKind::Http { .. }) {
                if let Some(source) = sources_by_file.get(file_path) {
                    if !resolver.is_http_route_call_at(source, line) {
                        continue;
                    }
                }
            }

            let raw = call_arg.unwrap_or_else(|| channel_hint(r.callee_symbol()));

            // Deterministic id keyed on the call site so a re-index upserts the
            // same row (channel value may improve as resolution does) rather than
            // leaving a stale duplicate behind.
            let id = format!("synth:{repository_id}:{file_path}:{line}:{}", role.as_str());

            // An HTTP route path read as a string literal is a resolved channel;
            // a messaging topic read as a property access stays unresolved for
            // the config-resolution pass. Normalize per protocol either way.
            let (host, normalized, is_pattern, resolved) = match &kind {
                RefKind::Http { .. } => {
                    let literal = strip_str_literal(&raw);
                    match literal {
                        Some(path) => {
                            let (host, normalized, is_pattern) =
                                normalize_channel(Protocol::Http, &path);
                            (host, normalized, is_pattern, Some(path))
                        }
                        // Path built at runtime (a variable) — keep unresolved.
                        None => (None, raw.clone(), false, None),
                    }
                }
                RefKind::Messaging { .. } => (None, raw.clone(), false, None),
            };

            let display = resolved.clone().unwrap_or_else(|| raw.clone());
            let mut endpoint = ChannelEndpoint::new(
                repository_id.to_string(),
                file_path.clone(),
                line,
                kind.protocol(),
                role,
                display,
                normalized,
                PACKAGE_DRIVEN_CONFIDENCE,
                EndpointSource::Config,
            )
            .with_id(id)
            .with_library(package)
            .confirmed();
            if resolved.is_none() {
                endpoint = endpoint.unresolved();
            }
            if let Some(host) = host {
                endpoint = endpoint.with_host(host);
            }
            if is_pattern {
                endpoint = endpoint.as_pattern();
            }
            if let RefKind::Http { verb } = kind {
                endpoint = endpoint.with_method(verb);
            }
            if let Some(scope) = r.enclosing_scope() {
                endpoint = endpoint.with_enclosing_symbol(scope);
            }
            synthesized.push(endpoint);
        }
    }

    synthesized
}

/// How a SCIP reference into a known library is classified for synthesis.
enum RefKind {
    /// A messaging call (Kafka/MQTT/AMQP/gRPC) with a producer/consumer role.
    Messaging {
        protocol: Protocol,
        role: ChannelRole,
    },
    /// An HTTP server route registration, with its verb. Always a consumer.
    Http { verb: &'static str },
}

impl RefKind {
    fn protocol(&self) -> Protocol {
        match self {
            RefKind::Messaging { protocol, .. } => *protocol,
            RefKind::Http { .. } => Protocol::Http,
        }
    }
    fn role(&self) -> ChannelRole {
        match self {
            RefKind::Messaging { role, .. } => *role,
            RefKind::Http { .. } => ChannelRole::Consumer,
        }
    }
}

/// Classify a SCIP reference (its package + callee symbol) into the endpoint it
/// should originate, or `None` when the call is not a channel operation.
///
/// HTTP is checked first: an express route call resolves into
/// `@types/express-serve-static-core` (`IRouter#get`) — a package that also
/// contains no messaging keyword, so the two branches never collide.
fn classify_ref(package: &str, callee_symbol: &str) -> Option<RefKind> {
    if http_server_package(package) {
        if let Some(verb) = http_verb_for_symbol(callee_symbol) {
            return Some(RefKind::Http { verb });
        }
    }
    if let Some(protocol) = protocol_for_package(package) {
        if let Some(role) = role_for_symbol(callee_symbol) {
            return Some(RefKind::Messaging { protocol, role });
        }
    }
    None
}

/// Strip a JS/TS string literal to its inner text (`'/path'` → `/path`),
/// returning `None` when `raw` is not a quoted string (an identifier or property
/// access built at runtime).
fn strip_str_literal(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let mut chars = raw.chars();
    let quote = chars.next().filter(|c| matches!(c, '"' | '\'' | '`'))?;
    if raw.len() >= 2 && raw.ends_with(quote) {
        Some(raw[1..raw.len() - 1].to_string())
    } else {
        None
    }
}

/// Whether the SCIP callee names a consumer's **read-loop** method — one that
/// pulls from an already-subscribed topic and so carries no channel argument of
/// its own (`consumer.consumeOne()`, `consumer.run(handler)`,
/// `consumer.each(...)`). The topic those loops read was named at the earlier
/// `subscribe`/`connect` call, which synthesis handles separately; a read-loop
/// site has nothing to originate, so it must not fabricate a receiver-labelled
/// endpoint when no argument is present.
fn read_loop_verb(callee_symbol: &str) -> bool {
    let method = callee_symbol
        .rsplit(['#', '.', ':'])
        .next()
        .unwrap_or(callee_symbol)
        .to_ascii_lowercase();
    matches!(
        method.as_str(),
        "consume" | "consumeone" | "run" | "each" | "eachmessage" | "eachbatch"
    )
}

/// A display hint for a synthesized endpoint with no recoverable channel: the
/// receiver type of the SCIP symbol (`Consumer#connect` → `Consumer`), else the
/// method name. Purely descriptive — the endpoint stays unresolved.
fn channel_hint(callee_symbol: &str) -> String {
    match callee_symbol.rsplit_once('#') {
        Some((receiver, _)) if !receiver.is_empty() => receiver
            .rsplit(['.', '/', ':'])
            .next()
            .unwrap_or(receiver)
            .to_string(),
        _ => callee_symbol
            .rsplit(['#', '.', ':'])
            .next()
            .unwrap_or(callee_symbol)
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ResolvedConfigValue;
    use crate::domain::{ChannelRole, EndpointSource, Language, ReferenceKind};

    #[derive(Default)]
    struct StubResolver {
        value: Option<ResolvedConfigValue>,
        pattern: Option<String>,
        /// Channel argument the stub reports for any call site (simulates
        /// reading the topic expression out of the source).
        channel_arg: Option<String>,
        /// Loop-array path fan-out the stub reports for any call site.
        loop_paths: Option<Vec<String>>,
        /// Whether a call site looks like a route registration. `None` (the
        /// default) reports `true`, so route synthesis is not gated unless a
        /// test opts into the settings-getter case with `Some(false)`.
        is_route_call: Option<bool>,
    }

    impl ChannelResolver for StubResolver {
        fn resolve_config_expression(
            &self,
            _expression: &str,
            _enclosing_class: Option<&str>,
            _candidates: &[(String, String)],
        ) -> Option<ResolvedConfigValue> {
            self.value.clone()
        }

        fn resolve_topic_pattern(
            &self,
            _expression: &str,
            _call_site_source: &str,
            _call_line: u32,
            _candidates: &[(String, String)],
        ) -> Option<String> {
            self.pattern.clone()
        }

        fn channel_argument_at(&self, _call_site_source: &str, _call_line: u32) -> Option<String> {
            self.channel_arg.clone()
        }

        fn resolve_loop_array_paths(
            &self,
            _expression: &str,
            _call_site_source: &str,
            _call_line: u32,
        ) -> Option<Vec<String>> {
            self.loop_paths.clone()
        }

        fn is_http_route_call_at(&self, _call_site_source: &str, _call_line: u32) -> bool {
            self.is_route_call.unwrap_or(true)
        }
    }

    fn endpoint(protocol: Protocol, channel: &str, resolved: bool) -> ChannelEndpoint {
        let e = ChannelEndpoint::new(
            "repo".to_string(),
            "src/app.ts".to_string(),
            42,
            protocol,
            ChannelRole::Producer,
            channel.to_string(),
            channel.to_string(),
            0.5,
            EndpointSource::TreeSitter,
        );
        if resolved {
            e
        } else {
            e.unresolved()
        }
    }

    fn scip_ref(line: u32, package: Option<&str>) -> SymbolReference {
        let r = SymbolReference::new(
            None,
            "EventProducer#produce".to_string(),
            "src/app.ts".to_string(),
            "src/app.ts".to_string(),
            line,
            1,
            ReferenceKind::MethodCall,
            Language::TypeScript,
            "repo".to_string(),
        );
        match package {
            Some(p) => r.with_callee_package(p),
            None => r,
        }
    }

    fn refs_map(refs: Vec<SymbolReference>) -> HashMap<String, Vec<SymbolReference>> {
        let mut m = HashMap::new();
        m.insert("src/app.ts".to_string(), refs);
        m
    }

    #[test]
    fn confirms_library_and_boosts_confidence() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver::default()));
        let refs = refs_map(vec![scip_ref(42, Some("kafkajs"))]);

        let out = uc.resolve(
            "repo",
            vec![endpoint(Protocol::Kafka, "orders", true)],
            &refs,
            &[],
            &HashMap::new(),
        );
        assert_eq!(out[0].library(), Some("kafkajs"));
        assert!(out[0].is_confirmed());
        assert!((out[0].confidence() - CONFIRMED_CONFIDENCE).abs() < f32::EPSILON);
    }

    #[test]
    fn does_not_confirm_on_protocol_mismatch() {
        // An MQTT endpoint must not be confirmed by a kafka package.
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver::default()));
        let refs = refs_map(vec![scip_ref(42, Some("kafkajs"))]);

        let out = uc.resolve(
            "repo",
            vec![endpoint(Protocol::Mqtt, "sensors/x", true)],
            &refs,
            &[],
            &HashMap::new(),
        );
        assert!(!out[0].is_confirmed());
        assert_eq!(out[0].library(), None);
    }

    #[test]
    fn resolves_config_value_and_env() {
        let resolved = ResolvedConfigValue {
            value: "shipment_event".to_string(),
            env_var: Some("KAFKA_SHIPMENT_EVENT_TOPIC".to_string()),
        };
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver {
            value: Some(resolved),
            ..Default::default()
        }));
        let refs = refs_map(vec![scip_ref(42, Some("kafkajs"))]);

        let out = uc.resolve(
            "repo",
            vec![endpoint(
                Protocol::Kafka,
                "this.config.broker.topics.shipmentEvent",
                false,
            )],
            &refs,
            &[("config".to_string(), "…".to_string())],
            &HashMap::new(),
        );
        assert_eq!(out[0].channel_raw(), "shipment_event");
        assert!(out[0].is_resolved());
        assert_eq!(out[0].env_var(), Some("KAFKA_SHIPMENT_EVENT_TOPIC"));
        // Library confirmation still applied.
        assert!(out[0].is_confirmed());
    }

    #[test]
    fn leaves_unconfirmed_when_no_package() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver::default()));
        let refs = refs_map(vec![scip_ref(42, None)]);

        let out = uc.resolve(
            "repo",
            vec![endpoint(Protocol::Kafka, "orders", true)],
            &refs,
            &[],
            &HashMap::new(),
        );
        assert!(!out[0].is_confirmed());
        assert!((out[0].confidence() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn infers_topic_pattern_and_marks_endpoint() {
        // An unresolved computed topic gets an inferred pattern from its source.
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver {
            pattern: Some("+/request".to_string()),
            ..Default::default()
        }));
        let mut sources = HashMap::new();
        sources.insert("src/app.ts".to_string(), "…".to_string());

        let out = uc.resolve(
            "repo",
            vec![endpoint(Protocol::Mqtt, "requestTopic", false)],
            &HashMap::new(),
            &[],
            &sources,
        );
        assert_eq!(out[0].channel_raw(), "+/request");
        assert!(out[0].is_resolved());
        assert!(out[0].is_pattern());
    }

    #[test]
    fn role_for_symbol_classifies_channel_verbs() {
        // A consumer's `connect`/`run` is its subscription loop — a channel op.
        assert_eq!(
            role_for_symbol("Consumer#connect"),
            Some(ChannelRole::Consumer)
        );
        assert_eq!(role_for_symbol("Consumer#run"), Some(ChannelRole::Consumer));
        // A producer's `connect` is pure setup — not a channel op.
        assert_eq!(role_for_symbol("Producer#connect"), None);
        // Unambiguous verbs need no type.
        assert_eq!(role_for_symbol("send"), Some(ChannelRole::Producer));
        assert_eq!(role_for_symbol("subscribe"), Some(ChannelRole::Consumer));
        assert_eq!(
            role_for_symbol("Client#produce"),
            Some(ChannelRole::Producer)
        );
        assert_eq!(
            role_for_symbol("Consumer#consumeOne"),
            Some(ChannelRole::Consumer)
        );
        // Lifecycle methods and bare shared verbs originate nothing.
        assert_eq!(role_for_symbol("connect"), None);
        assert_eq!(role_for_symbol("Consumer#close"), None);
        assert_eq!(role_for_symbol("Producer#<constructor>"), None);
        assert_eq!(role_for_symbol("Client#commit"), None);
    }

    /// A wrapper/fork consumer call (`consumer.connect({ topics })`) that no
    /// framework detector matched is originated from the SCIP package reference:
    /// the package gives the protocol, the receiver type gives the role.
    #[test]
    fn synthesizes_consumer_from_wrapper_package() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver::default()));

        // SCIP resolves `this.consumer.connect(...)` to the fork's
        // `Consumer#connect`, defined in `@backend/kafkajs`.
        let mut refs = HashMap::new();
        refs.insert(
            "src/kafka-client-adapter.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "Consumer#connect".to_string(),
                "src/kafka-client-adapter.ts".to_string(),
                "src/kafka-client-adapter.ts".to_string(),
                38,
                9,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("@backend/kafkajs")
            .with_enclosing_scope("KafkaClientAdapter")],
        );

        // Extraction found nothing in this file.
        let out = uc.resolve("repo", vec![], &refs, &[], &HashMap::new());

        assert_eq!(out.len(), 1, "one synthesized consumer endpoint");
        let ep = &out[0];
        assert_eq!(ep.protocol(), Protocol::Kafka);
        assert_eq!(ep.role(), ChannelRole::Consumer);
        assert!(!ep.is_resolved(), "no concrete topic from SCIP");
        assert!(ep.is_confirmed());
        assert_eq!(ep.library(), Some("@backend/kafkajs"));
        assert_eq!(ep.enclosing_symbol(), Some("KafkaClientAdapter"));
        assert_eq!(ep.line(), 38);
    }

    /// A consumer's read-loop call (`consumer.consumeOne()`) carries no channel
    /// argument — the topic was named at the earlier `subscribe`/`connect`. With
    /// no argument to read, synthesis must skip it rather than fabricate an
    /// endpoint labelled with the receiver type (`Consumer`).
    #[test]
    fn does_not_synthesize_from_read_loop_call_without_argument() {
        // Default stub reports no channel argument at any call site.
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver::default()));

        let mut refs = HashMap::new();
        refs.insert(
            "src/consumer.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "Consumer#consumeOne".to_string(),
                "src/consumer.ts".to_string(),
                "src/consumer.ts".to_string(),
                94,
                9,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("@backend/kafkajs")
            .with_enclosing_scope("Consumer")],
        );

        let out = uc.resolve("repo", vec![], &refs, &[], &HashMap::new());
        assert!(
            out.is_empty(),
            "read-loop call with no channel argument must not synthesize, got {:?}",
            out.iter().map(|e| e.channel_raw()).collect::<Vec<_>>()
        );
    }

    /// A read-loop call *does* originate an endpoint when the call site actually
    /// carries a topic argument the resolver can read (some wrappers pass the
    /// topic into `run`/`consume`), so the skip is gated on the missing argument.
    #[test]
    fn synthesizes_from_read_loop_call_with_argument() {
        let resolver = StubResolver {
            channel_arg: Some("'orders'".to_string()),
            ..Default::default()
        };
        let uc = ResolveChannelsUseCase::new(Arc::new(resolver));

        let mut refs = HashMap::new();
        refs.insert(
            "src/consumer.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "Consumer#consume".to_string(),
                "src/consumer.ts".to_string(),
                "src/consumer.ts".to_string(),
                50,
                9,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("@backend/kafkajs")],
        );

        // The channel argument is read from the call-site source, so a source
        // entry must exist for the resolver stub to be consulted at all.
        let mut sources = HashMap::new();
        sources.insert("src/consumer.ts".to_string(), "// source".to_string());

        let out = uc.resolve("repo", vec![], &refs, &[], &sources);
        assert_eq!(out.len(), 1, "argument present → synthesize");
        assert_eq!(out[0].role(), ChannelRole::Consumer);
    }

    /// A literal `produce("orders", …)` the extractor already resolved must not
    /// gain a weaker synthesized twin from the SCIP reference at the same site.
    #[test]
    fn does_not_shadow_extracted_endpoint() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver::default()));

        let mut refs = HashMap::new();
        refs.insert(
            "src/app.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "Producer#produce".to_string(),
                "src/app.ts".to_string(),
                "src/app.ts".to_string(),
                42,
                1,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("@confluentinc/kafka-javascript")],
        );

        // The extractor already produced a resolved producer at line 42.
        let extracted = endpoint(Protocol::Kafka, "orders", true);
        let out = uc.resolve("repo", vec![extracted], &refs, &[], &HashMap::new());

        // Only the extracted endpoint survives — no synthesized duplicate.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel_raw(), "orders");
        assert!(out[0].is_resolved());
    }

    /// An unresolved HTTP endpoint whose channel is a loop-variable field access
    /// (`route.path`) is expanded into one resolved endpoint per route-table
    /// entry the resolver reports, and the placeholder is dropped.
    #[test]
    fn expands_loop_registered_routes() {
        let resolver = StubResolver {
            loop_paths: Some(vec![
                "/search".to_string(),
                "/delete".to_string(),
                "/create".to_string(),
            ]),
            ..Default::default()
        };
        let uc = ResolveChannelsUseCase::new(Arc::new(resolver));

        // The placeholder the synthesizer would leave: unresolved HTTP consumer
        // whose raw channel is the loop-variable access.
        let placeholder = ChannelEndpoint::new(
            "repo".to_string(),
            "src/routes/index.ts".to_string(),
            79,
            Protocol::Http,
            ChannelRole::Consumer,
            "route.path".to_string(),
            "route.path".to_string(),
            0.6,
            EndpointSource::Config,
        )
        .with_method("POST")
        .unresolved();

        let mut sources = HashMap::new();
        sources.insert("src/routes/index.ts".to_string(), "// source".to_string());

        let out = uc.resolve("repo", vec![placeholder], &HashMap::new(), &[], &sources);

        // Three resolved routes, no `route.path` placeholder left behind.
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|e| e.is_resolved()));
        assert!(out.iter().all(|e| e.method() == Some("POST")));
        let mut paths: Vec<_> = out.iter().map(|e| e.channel_normalized()).collect();
        paths.sort();
        assert_eq!(paths, vec!["/create", "/delete", "/search"]);
        assert!(out.iter().all(|e| e.channel_raw() != "route.path"));
    }

    /// A non-messaging package reference never originates an endpoint.
    #[test]
    fn ignores_non_messaging_packages() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver::default()));

        let mut refs = HashMap::new();
        refs.insert(
            "src/db.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "Client#connect".to_string(),
                "src/db.ts".to_string(),
                "src/db.ts".to_string(),
                10,
                1,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("pg")],
        );

        let out = uc.resolve("repo", vec![], &refs, &[], &HashMap::new());
        assert!(out.is_empty());
    }

    #[test]
    fn http_verb_and_package_classification() {
        // express route methods resolve into express's own router type.
        assert!(http_server_package("@types/express-serve-static-core"));
        assert!(http_server_package("express"));
        assert!(http_server_package("fastify"));
        assert!(!http_server_package("axios"));
        assert!(!http_server_package("pg"));

        assert_eq!(http_verb_for_symbol("IRouter#get"), Some("GET"));
        assert_eq!(http_verb_for_symbol("FastifyInstance#post"), Some("POST"));
        assert_eq!(http_verb_for_symbol("Application#all"), Some("ANY"));
        assert_eq!(http_verb_for_symbol("Router#route"), Some("ANY"));
        // `use` registers middleware, not a route — excluded to avoid flooding
        // the report with `app.use(express.json())`-style non-endpoints.
        assert_eq!(http_verb_for_symbol("IRouter#use"), None);
        assert_eq!(http_verb_for_symbol("Server#listen"), None);
        // Receiver-type gate: `req.get('Header')` is a header read, not a route.
        assert_eq!(http_verb_for_symbol("Request#get"), None);
        assert_eq!(http_verb_for_symbol("Response#get"), None);
    }

    /// A `this.router.get('/p', …)` route whose object is a field access — missed
    /// by a bare-identifier syntactic detector — is originated from the SCIP
    /// reference into express's router type, with the verb and normalized path.
    #[test]
    fn synthesizes_http_route_from_express_type() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver {
            // The resolver reads the path literal off the call site.
            channel_arg: Some("'/minHeatingTime/:id'".to_string()),
            ..Default::default()
        }));

        let mut refs = HashMap::new();
        refs.insert(
            "src/router.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "IRouter#get".to_string(),
                "src/router.ts".to_string(),
                "src/router.ts".to_string(),
                22,
                9,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("@types/express-serve-static-core")
            .with_enclosing_scope("ThermoregulationRouter")],
        );
        // The call-site source must be present for the resolver to read the path.
        let mut sources = HashMap::new();
        sources.insert("src/router.ts".to_string(), "…".to_string());

        let out = uc.resolve("repo", vec![], &refs, &[], &sources);

        assert_eq!(out.len(), 1);
        let ep = &out[0];
        assert_eq!(ep.protocol(), Protocol::Http);
        assert_eq!(ep.role(), ChannelRole::Consumer);
        assert_eq!(ep.method(), Some("GET"));
        // Path parameters normalized to `{}`, literal is resolved (joinable).
        assert_eq!(ep.channel_normalized(), "/minHeatingTime/{}");
        assert!(ep.is_resolved());
        assert_eq!(ep.library(), Some("@types/express-serve-static-core"));
        assert_eq!(ep.enclosing_symbol(), Some("ThermoregulationRouter"));
    }

    /// Express's `app.get('title')` settings getter resolves into the same route
    /// type as a real route (SCIP `IRouter#get`), but has a single argument.
    /// Synthesis must reject it so it does not become a bogus `GET` route.
    #[test]
    fn does_not_synthesize_route_from_settings_getter() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver {
            // The call site is a one-argument getter, not a route registration.
            is_route_call: Some(false),
            ..Default::default()
        }));

        let mut refs = HashMap::new();
        refs.insert(
            "src/app.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "Application#get".to_string(),
                "src/app.ts".to_string(),
                "src/app.ts".to_string(),
                28,
                9,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("@types/express-serve-static-core")],
        );
        let mut sources = HashMap::new();
        sources.insert("src/app.ts".to_string(), "// source".to_string());

        let out = uc.resolve("repo", vec![], &refs, &[], &sources);
        assert!(
            out.is_empty(),
            "settings getter must not synthesize a route, got {:?}",
            out.iter().map(|e| e.channel_raw()).collect::<Vec<_>>()
        );
    }

    /// An HTTP **client** package (axios) must never originate a server route —
    /// its calls are producers, covered by syntactic detectors.
    #[test]
    fn does_not_synthesize_http_route_from_client_package() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver {
            channel_arg: Some("'http://svc/api'".to_string()),
            ..Default::default()
        }));

        let mut refs = HashMap::new();
        refs.insert(
            "src/client.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "AxiosInstance#get".to_string(),
                "src/client.ts".to_string(),
                "src/client.ts".to_string(),
                5,
                1,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("axios")],
        );

        let out = uc.resolve("repo", vec![], &refs, &[], &HashMap::new());
        assert!(out.is_empty());
    }

    /// An HTTP route already extracted by a syntactic detector must not gain a
    /// synthesized twin from the SCIP reference at the same line.
    #[test]
    fn does_not_shadow_extracted_http_route() {
        let uc = ResolveChannelsUseCase::new(Arc::new(StubResolver {
            channel_arg: Some("'/rhc'".to_string()),
            ..Default::default()
        }));

        let mut refs = HashMap::new();
        refs.insert(
            "src/router.ts".to_string(),
            vec![SymbolReference::new(
                None,
                "IRouter#post".to_string(),
                "src/router.ts".to_string(),
                "src/router.ts".to_string(),
                25,
                1,
                ReferenceKind::MethodCall,
                Language::TypeScript,
                "repo".to_string(),
            )
            .with_callee_package("@types/express-serve-static-core")],
        );

        // The syntactic detector already produced an HTTP consumer at line 25.
        let extracted = ChannelEndpoint::new(
            "repo".to_string(),
            "src/router.ts".to_string(),
            25,
            Protocol::Http,
            ChannelRole::Consumer,
            "/rhc".to_string(),
            "/rhc".to_string(),
            0.8,
            EndpointSource::TreeSitter,
        )
        .with_method("post");
        let out = uc.resolve("repo", vec![extracted], &refs, &[], &HashMap::new());

        assert_eq!(out.len(), 1, "no synthesized duplicate");
        assert_eq!(out[0].source(), EndpointSource::TreeSitter);
    }
}
