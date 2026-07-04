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
    /// the given `(config_object_name, module_source)` candidates. Returns
    /// `None` when nothing resolves it syntactically.
    fn resolve_config_expression(
        &self,
        expression: &str,
        candidates: &[(String, String)],
    ) -> Option<ResolvedConfigValue>;
}
