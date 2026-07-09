/// A channel value recovered from a config module (`process.env.X ||
/// 'default'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfigValue {
    /// The concrete default string the channel resolves to.
    pub value: String,
    /// The env var that overrides it at runtime, if any.
    pub env_var: Option<String>,
}

/// Syntactic resolution of a config-driven channel expression.
///
/// Producers/consumers frequently pass their topic/route as a property access
/// into a config object (`this.config.broker.topics.orders`) whose leaf is a
/// `process.env.X || 'default'` expression. Given the expression and the source
/// of the candidate config modules, an implementation walks the object literal
/// and returns the resolved value. Implementations live in the connector layer
/// (tree-sitter); the use case supplies the candidate sources it discovered.
pub trait ChannelResolver: Send + Sync {
    /// Resolve `expression` (e.g. `this.config.broker.topics.orders`) against
    /// the given `(object_name, module_source)` candidates. Returns `None` when
    /// nothing resolves it syntactically.
    ///
    /// When `enclosing_class` is set and the expression accesses a constructor
    /// parameter (`this.topics.orders` inside a class), the resolver also traces
    /// through the `new <class>(…)` instantiation to recover the wired config
    /// expression before resolving it.
    fn resolve_config_expression(
        &self,
        expression: &str,
        enclosing_class: Option<&str>,
        candidates: &[(String, String)],
    ) -> Option<ResolvedConfigValue>;

    /// Infer a topic *pattern* for a computed channel: a template literal
    /// (`` `${id}/request` `` → `+/request`), a variable assigned from a local
    /// getter that returns one, or an interface-dispatched client call
    /// (`this.broker.publish(…)` where a `Broker implements Publisher` performs
    /// the real publish).
    ///
    /// `call_site_source` is the source of the file the call occurs in and
    /// `call_line` its 1-based line — both needed to read local getters and the
    /// interface field type. `candidates` supply the other module sources
    /// (implementing classes, config). Returns the MQTT pattern, or `None`.
    fn resolve_topic_pattern(
        &self,
        expression: &str,
        call_site_source: &str,
        call_line: u32,
        candidates: &[(String, String)],
    ) -> Option<String>;

    /// The channel argument expression of the messaging call at `call_line`
    /// (1-based) in `call_site_source`, as written.
    ///
    /// Used to give a *synthesized* endpoint — one originated from the SCIP call
    /// graph, not matched by a framework detector — the real topic expression so
    /// it can flow through the resolution passes. Reads the first positional
    /// argument, or the `topic`/`topics` value of a leading options object
    /// (`connect({ topics: this.topics }, …)` → `this.topics`). Returns `None`
    /// when no call or channel-like argument is found at that line.
    fn channel_argument_at(&self, call_site_source: &str, call_line: u32) -> Option<String>;

    /// Whether the call at `call_line` (1-based) in `call_site_source` looks
    /// like an HTTP **route registration** rather than a settings access.
    ///
    /// Express overloads `app.get(name)`: with a single argument it *reads a
    /// setting* (`app.get('title')`), and only with a handler as its second
    /// argument does it *register a route* (`app.get('/p', handler)`). A route
    /// call always carries at least two arguments, so this reports whether the
    /// call at that line has a second argument — letting synthesis reject the
    /// settings getter that SCIP still resolves into the express route type.
    /// Returns `false` when no call is found at that line.
    fn is_http_route_call_at(&self, call_site_source: &str, call_line: u32) -> bool;

    /// Expand a channel registered inside a loop over a local array of route
    /// objects into one value per array element.
    ///
    /// The dominant Express fan-out shape registers many routes from a table:
    ///
    /// ```ts
    /// const routes = [{ path: '/search', handler: search }, …]
    /// for (const route of routes) router.post(route.path, route.handler)
    /// ```
    ///
    /// The call site reads `route.path`, an access on the loop variable, so no
    /// single value resolves. Given that expression, the call-site source, and
    /// the call line, an implementation finds the enclosing `for (const <var> of
    /// <array>)` (or `<array>.forEach(<var> => …)`), resolves `<array>` to a
    /// local array literal, and returns the `<field>` string of each element.
    /// Returns `None` when the expression is not a loop-variable field access or
    /// the array cannot be resolved to element literals.
    fn resolve_loop_array_paths(
        &self,
        expression: &str,
        call_site_source: &str,
        call_line: u32,
    ) -> Option<Vec<String>>;

    /// The URL path prefix an HTTP route is mounted under, traced across the
    /// codebase, or `None` when no literal prefix can be established.
    ///
    /// Express routes are registered on a router in one place and *mounted* under
    /// a prefix in another (`app.use('/history', router)`), often via a factory
    /// function and a custom wrapper method:
    ///
    /// ```ts
    /// // configuration-router.ts
    /// export function configurationRouter(router) { router.get('/:id', …) }
    /// // app.ts
    /// httpApp.addRouter('/history', configurationRouter(router))
    /// // http-app.ts
    /// addRouter(path, router) { this.app.use(path, router) }
    /// ```
    ///
    /// The registered path (`/:id`) is not the served path (`/history/:id`). Given
    /// the route's registration site — the file it is registered in and the
    /// enclosing function/router symbol — an implementation follows the router
    /// object through factory calls and mount wrappers to the `.use(prefix, …)`
    /// that applies a string-literal prefix, and returns that prefix. `candidates`
    /// supply every module source so the trace can cross files.
    ///
    /// Returns `None` when the route is mounted at the root, the prefix is
    /// computed at runtime, or the chain cannot be followed — in which case the
    /// caller keeps the bare registered path rather than guessing.
    fn resolve_route_prefix(
        &self,
        route_file: &str,
        enclosing_symbol: Option<&str>,
        candidates: &[(String, String)],
    ) -> Option<String>;
}
