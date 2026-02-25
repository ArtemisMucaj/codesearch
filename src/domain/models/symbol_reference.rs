use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Language;

/// Represents a reference from one symbol to another (call graph edge).
///
/// This captures relationships like:
/// - Function calls
/// - Method invocations
/// - Type references
/// - Constant usage
/// - Import statements
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolReference {
    /// Unique identifier for this reference
    id: String,

    /// The symbol making the reference (caller)
    caller_symbol: Option<String>,

    /// The symbol being referenced (callee)
    callee_symbol: String,

    /// File path where the caller is declared
    caller_file_path: String,

    /// File path where the reference occurs (may differ from caller declaration)
    reference_file_path: String,

    /// Line number where the reference occurs
    reference_line: u32,

    /// Column number where the reference occurs
    reference_column: u32,

    /// Kind of reference (call, method_call, type_reference, etc.)
    reference_kind: ReferenceKind,

    /// Programming language
    language: Language,

    /// Repository this reference belongs to
    repository_id: String,

    /// The node type of the caller (if known)
    caller_node_type: Option<String>,

    /// The enclosing scope/context (e.g., class name for methods)
    enclosing_scope: Option<String>,

    /// Local alias used at the import/require site (e.g., `bar` in `import { foo as bar }`).
    /// `None` when the symbol is imported without renaming.
    import_alias: Option<String>,

    /// The raw path argument of a `require()` call (e.g., `"./sample_middleware.js"`).
    ///
    /// Populated at parse time and used during index-time cross-file export resolution;
    /// **not** persisted to the database and excluded from serialisation.
    #[serde(skip)]
    require_source_path: Option<String>,
}

impl SymbolReference {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        caller_symbol: Option<String>,
        callee_symbol: String,
        caller_file_path: String,
        reference_file_path: String,
        reference_line: u32,
        reference_column: u32,
        reference_kind: ReferenceKind,
        language: Language,
        repository_id: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            caller_symbol,
            callee_symbol,
            caller_file_path,
            reference_file_path,
            reference_line,
            reference_column,
            reference_kind,
            language,
            repository_id,
            caller_node_type: None,
            enclosing_scope: None,
            import_alias: None,
            require_source_path: None,
        }
    }

    /// Reconstitutes from persisted data (used by adapters).
    #[allow(clippy::too_many_arguments)]
    pub fn reconstitute(
        id: String,
        caller_symbol: Option<String>,
        callee_symbol: String,
        caller_file_path: String,
        reference_file_path: String,
        reference_line: u32,
        reference_column: u32,
        reference_kind: ReferenceKind,
        language: Language,
        repository_id: String,
        caller_node_type: Option<String>,
        enclosing_scope: Option<String>,
        import_alias: Option<String>,
    ) -> Self {
        Self {
            id,
            caller_symbol,
            callee_symbol,
            caller_file_path,
            reference_file_path,
            reference_line,
            reference_column,
            reference_kind,
            language,
            repository_id,
            caller_node_type,
            enclosing_scope,
            import_alias,
            require_source_path: None,
        }
    }

    pub fn with_caller_node_type(mut self, node_type: impl Into<String>) -> Self {
        self.caller_node_type = Some(node_type.into());
        self
    }

    pub fn with_enclosing_scope(mut self, scope: impl Into<String>) -> Self {
        self.enclosing_scope = Some(scope.into());
        self
    }

    pub fn with_import_alias(mut self, alias: impl Into<String>) -> Self {
        self.import_alias = Some(alias.into());
        self
    }

    /// Records the raw `require()` path argument for later cross-file resolution.
    /// This value is **not** persisted to the database.
    pub fn with_require_source_path(mut self, path: impl Into<String>) -> Self {
        self.require_source_path = Some(path.into());
        self
    }

    /// Replaces the callee symbol with the resolved exported name and promotes the
    /// previous callee (the local binding) to `import_alias`.
    ///
    /// Used by the index-time export resolver when it determines that
    /// `const localName = require('./file')` actually imports `exportedName`.
    pub fn with_resolved_callee(mut self, exported_name: String, local_binding: String) -> Self {
        self.callee_symbol = exported_name;
        self.import_alias = Some(local_binding);
        self
    }

    // Getters
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn caller_symbol(&self) -> Option<&str> {
        self.caller_symbol.as_deref()
    }

    pub fn callee_symbol(&self) -> &str {
        &self.callee_symbol
    }

    pub fn caller_file_path(&self) -> &str {
        &self.caller_file_path
    }

    pub fn reference_file_path(&self) -> &str {
        &self.reference_file_path
    }

    pub fn reference_line(&self) -> u32 {
        self.reference_line
    }

    pub fn reference_column(&self) -> u32 {
        self.reference_column
    }

    pub fn reference_kind(&self) -> ReferenceKind {
        self.reference_kind
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn caller_node_type(&self) -> Option<&str> {
        self.caller_node_type.as_deref()
    }

    pub fn enclosing_scope(&self) -> Option<&str> {
        self.enclosing_scope.as_deref()
    }

    /// Returns the local alias used at this import site, if the symbol was renamed.
    /// For example, `bar` in `import { foo as bar }` or `const { foo: bar } = require(...)`.
    pub fn import_alias(&self) -> Option<&str> {
        self.import_alias.as_deref()
    }

    /// Returns the raw `require()` path argument captured at parse time.
    /// This is a transient field used for cross-file resolution and is not persisted.
    pub fn require_source_path(&self) -> Option<&str> {
        self.require_source_path.as_deref()
    }

    /// Returns a formatted location string for this reference.
    pub fn location(&self) -> String {
        format!(
            "{}:{}:{}",
            self.reference_file_path, self.reference_line, self.reference_column
        )
    }

    /// Returns the qualified caller name (scope::name if scope exists).
    pub fn qualified_caller(&self) -> Option<String> {
        match (&self.enclosing_scope, &self.caller_symbol) {
            (Some(scope), Some(name)) => Some(format!("{}::{}", scope, name)),
            (None, Some(name)) => Some(name.clone()),
            _ => None,
        }
    }
}

/// The kind of symbol reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    /// Function/method call: `foo()`, `obj.method()`
    Call,
    /// Method call on an object: `obj.method()`
    MethodCall,
    /// Type reference: `let x: Foo`, `impl Foo`
    TypeReference,
    /// Import/use statement: `use foo::bar`, `import foo`
    Import,
    /// Constant/variable reference: `x + y`
    VariableReference,
    /// Attribute/field access: `obj.field`
    FieldAccess,
    /// Macro invocation: `println!()`, `@decorator`
    MacroInvocation,
    /// Instantiation: `new Foo()`, `Foo {}`
    Instantiation,
    /// Trait/interface implementation: `impl Trait for Type`
    Implementation,
    /// Inheritance: `class Foo extends Bar`
    Inheritance,
    /// Generic/template reference: `Vec<Foo>`
    GenericArgument,
    /// Unknown or unclassified reference
    Unknown,
}

impl ReferenceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReferenceKind::Call => "call",
            ReferenceKind::MethodCall => "method_call",
            ReferenceKind::TypeReference => "type_reference",
            ReferenceKind::Import => "import",
            ReferenceKind::VariableReference => "variable_reference",
            ReferenceKind::FieldAccess => "field_access",
            ReferenceKind::MacroInvocation => "macro_invocation",
            ReferenceKind::Instantiation => "instantiation",
            ReferenceKind::Implementation => "implementation",
            ReferenceKind::Inheritance => "inheritance",
            ReferenceKind::GenericArgument => "generic_argument",
            ReferenceKind::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "call" => ReferenceKind::Call,
            "method_call" => ReferenceKind::MethodCall,
            "type_reference" => ReferenceKind::TypeReference,
            "import" => ReferenceKind::Import,
            "variable_reference" => ReferenceKind::VariableReference,
            "field_access" => ReferenceKind::FieldAccess,
            "macro_invocation" => ReferenceKind::MacroInvocation,
            "instantiation" => ReferenceKind::Instantiation,
            "implementation" => ReferenceKind::Implementation,
            "inheritance" => ReferenceKind::Inheritance,
            "generic_argument" => ReferenceKind::GenericArgument,
            _ => ReferenceKind::Unknown,
        }
    }
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_reference_creation() {
        let reference = SymbolReference::new(
            Some("my_function".to_string()),
            "other_function".to_string(),
            "src/lib.rs".to_string(),
            "src/lib.rs".to_string(),
            42,
            10,
            ReferenceKind::Call,
            Language::Rust,
            "repo-123".to_string(),
        );

        assert_eq!(reference.caller_symbol(), Some("my_function"));
        assert_eq!(reference.callee_symbol(), "other_function");
        assert_eq!(reference.reference_kind(), ReferenceKind::Call);
        assert_eq!(reference.reference_line(), 42);
    }

    #[test]
    fn test_qualified_caller() {
        let reference = SymbolReference::new(
            Some("method".to_string()),
            "other".to_string(),
            "src/lib.rs".to_string(),
            "src/lib.rs".to_string(),
            1,
            1,
            ReferenceKind::Call,
            Language::Rust,
            "repo".to_string(),
        )
        .with_enclosing_scope("MyClass");

        assert_eq!(
            reference.qualified_caller(),
            Some("MyClass::method".to_string())
        );
    }

    #[test]
    fn test_reference_kind_roundtrip() {
        let kinds = vec![
            ReferenceKind::Call,
            ReferenceKind::MethodCall,
            ReferenceKind::TypeReference,
            ReferenceKind::Import,
            ReferenceKind::MacroInvocation,
        ];

        for kind in kinds {
            let s = kind.as_str();
            let parsed = ReferenceKind::parse(s);
            assert_eq!(kind, parsed);
        }
    }
}
