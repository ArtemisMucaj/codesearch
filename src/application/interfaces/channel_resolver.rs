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
}
