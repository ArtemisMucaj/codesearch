use async_trait::async_trait;

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
#[async_trait]
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
}
