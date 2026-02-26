use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use streaming_iterator::StreamingIterator;
use tracing::debug;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::application::ParserService;
use crate::domain::{CodeChunk, DomainError, Language, NodeType, ReferenceKind, SymbolReference};

/// Metadata for a definition scope (function, class, trait, …) used to
/// resolve enclosing-scope names and to identify class definitions.
#[derive(Debug, Clone)]
struct ScopeInfo {
    start_line: u32,
    end_line: u32,
    name: String,
    parent: Option<String>,
    /// The query capture kind, e.g. `"class"`, `"function"`, `"interface"`.
    kind: String,
}

/// Normalize import paths by stripping surrounding delimiters.
/// - Go imports: "fmt" -> fmt
/// - C++ string includes: "header.h" -> header.h
/// - C++ system includes: <iostream> -> iostream
fn normalize_import_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.len() < 2 {
        return trimmed.to_string();
    }

    // Check for surrounding quotes (Go imports, C++ string includes)
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return trimmed[1..trimmed.len() - 1].to_string();
    }

    // Check for surrounding angle brackets (C++ system includes)
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        return trimmed[1..trimmed.len() - 1].to_string();
    }

    trimmed.to_string()
}

pub struct TreeSitterParser {
    supported_languages: Vec<Language>,
}

impl TreeSitterParser {
    pub fn new() -> Self {
        Self {
            supported_languages: vec![
                Language::Rust,
                Language::Python,
                Language::JavaScript,
                Language::TypeScript,
                Language::Go,
                Language::HCL,
                Language::Php,
                Language::Cpp,
                Language::Swift,
                Language::Kotlin,
            ],
        }
    }

    fn get_ts_language(&self, language: Language) -> Option<tree_sitter::Language> {
        match language {
            Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
            Language::HCL => Some(tree_sitter_hcl::LANGUAGE.into()),
            Language::Php => Some(tree_sitter_php::LANGUAGE_PHP.into()),
            Language::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
            Language::Swift => Some(tree_sitter_swift::LANGUAGE.into()),
            Language::Kotlin => Some(tree_sitter_kotlin_ng::LANGUAGE.into()),
            Language::Unknown => None,
        }
    }

    fn get_query_patterns(&self, language: Language) -> &'static str {
        match language {
            Language::Rust => {
                r#"
                (function_item name: (identifier) @name) @function
                (impl_item) @impl
                (struct_item name: (type_identifier) @name) @struct
                (enum_item name: (type_identifier) @name) @enum
                (trait_item name: (type_identifier) @name) @trait
                (mod_item name: (identifier) @name) @module
                (const_item name: (identifier) @name) @constant
                (static_item name: (identifier) @name) @constant
                (type_item name: (type_identifier) @name) @typedef
                "#
            }
            Language::Python => {
                r#"
                (function_definition name: (identifier) @name) @function
                (class_definition name: (identifier) @name) @class
                "#
            }
            Language::JavaScript => {
                r#"
                (function_declaration name: (identifier) @name) @function
                (class_declaration name: (identifier) @name) @class
                (method_definition name: (property_identifier) @name) @function
                (arrow_function) @function
                "#
            }
            Language::TypeScript => {
                r#"
                (function_declaration name: (identifier) @name) @function
                (class_declaration name: (type_identifier) @name) @class
                (method_definition name: (property_identifier) @name) @function
                (arrow_function) @function
                (interface_declaration name: (type_identifier) @name) @interface
                (type_alias_declaration name: (type_identifier) @name) @typedef
                (export_statement (interface_declaration name: (type_identifier) @name)) @interface
                (export_statement (type_alias_declaration name: (type_identifier) @name)) @typedef
                "#
            }
            Language::Go => {
                r#"
                (function_declaration name: (identifier) @name) @function
                (method_declaration name: (field_identifier) @name) @function
                (type_declaration (type_spec name: (type_identifier) @name)) @struct
                "#
            }
            Language::HCL => {
                r#"
                (block (identifier) @name) @block
                (attribute (identifier) @name) @constant
                "#
            }
            Language::Php => {
                r#"
                (function_definition name: (name) @name) @function
                (method_declaration name: (name) @name) @function
                (class_declaration name: (name) @name) @class
                (interface_declaration name: (name) @name) @interface
                (trait_declaration name: (name) @name) @trait
                (namespace_definition name: (namespace_name) @name) @module
                (enum_declaration name: (name) @name) @enum
                "#
            }
            Language::Cpp => {
                r#"
                ; Classes and structs
                (class_specifier name: (type_identifier) @name) @class
                (struct_specifier name: (type_identifier) @name) @struct
                (union_specifier name: (type_identifier) @name) @class

                ; Functions and methods
                (function_definition
                  declarator: (function_declarator declarator: (identifier) @name)) @function
                (function_definition
                  declarator: (function_declarator declarator: (field_identifier) @name)) @function
                (function_definition
                  declarator: (function_declarator
                    declarator: (qualified_identifier
                      scope: (namespace_identifier) @class.name
                      name: (identifier) @name))) @function

                ; Destructors
                (function_definition
                  declarator: (function_declarator
                    (destructor_name
                      (identifier) @name))) @function

                ; Operators (use function_definition with operator_cast)
                (function_definition
                  declarator: (operator_cast) @name) @function
                (function_definition
                  declarator: (qualified_identifier
                    scope: (namespace_identifier) @class.name
                    name: (operator_cast) @name)) @function

                ; Operator declarations
                (declaration
                  declarator: (operator_cast) @name) @function

                ; Template declarations - capture the nested declaration's name
                (template_declaration
                  (alias_declaration
                    name: (type_identifier) @name)) @template
                (template_declaration
                  (function_definition
                    declarator: (function_declarator declarator: (identifier) @name))) @template
                (template_declaration
                  (class_specifier
                    name: (type_identifier) @name)) @template

                ; Template instantiations - these have a declarator field
                (template_instantiation
                  declarator: (_declarator
                    (identifier) @name)) @template

                ; Namespaces
                (namespace_definition
                  name: (namespace_identifier) @name) @module
                (namespace_alias_definition
                  name: (namespace_identifier) @name) @module

                ; Types
                (type_definition declarator: (type_identifier) @name) @typedef
                (enum_specifier name: (type_identifier) @name) @enum

                ; Aliases and using
                (using_declaration
                  (identifier) @name) @using
                (alias_declaration
                  name: (type_identifier) @name) @alias

                ; Concepts (C++20)
                (concept_definition
                  name: (identifier) @name) @concept
                "#
            }
            Language::Swift => {
                r#"
                ; Free functions and methods
                (function_declaration name: (simple_identifier) @name) @function

                ; Classes
                (class_declaration
                  declaration_kind: "class"
                  name: (type_identifier) @name) @class

                ; Structs
                (class_declaration
                  declaration_kind: "struct"
                  name: (type_identifier) @name) @struct

                ; Enums
                (class_declaration
                  declaration_kind: "enum"
                  name: (type_identifier) @name) @enum

                ; Actors (treated as classes)
                (class_declaration
                  declaration_kind: "actor"
                  name: (type_identifier) @name) @class

                ; Protocols (like traits/interfaces)
                (protocol_declaration name: (type_identifier) @name) @trait

                ; Extensions (like impl blocks)
                (class_declaration declaration_kind: "extension") @impl

                ; Type aliases
                (typealias_declaration name: (type_identifier) @name) @typedef
                "#
            }
            Language::Kotlin => {
                r#"
                ; Top-level functions and methods
                (function_declaration (identifier) @name) @function

                ; Classes (includes data classes, sealed classes, abstract classes,
                ; interfaces, enum classes, and annotation classes)
                (class_declaration (identifier) @name) @class

                ; Object declarations (singletons and companion objects)
                (object_declaration (identifier) @name) @struct

                ; Type aliases
                (type_alias (identifier) @name) @typedef
                "#
            }
            Language::Unknown => "",
        }
    }

    fn capture_to_node_type(capture_name: &str) -> NodeType {
        match capture_name {
            "function" => NodeType::Function,
            "class" => NodeType::Class,
            "struct" => NodeType::Struct,
            "enum" => NodeType::Enum,
            "trait" => NodeType::Trait,
            "impl" => NodeType::Impl,
            "module" => NodeType::Module,
            "constant" => NodeType::Constant,
            "typedef" => NodeType::TypeDef,
            "interface" => NodeType::Interface,
            _ => NodeType::Block,
        }
    }

    /// Get tree-sitter query patterns for extracting symbol references.
    fn get_reference_query_patterns(&self, language: Language) -> &'static str {
        match language {
            Language::Rust => {
                r#"
                ; Function calls
                (call_expression
                    function: (identifier) @callee) @call

                ; Method calls
                (call_expression
                    function: (field_expression
                        field: (field_identifier) @callee)) @method_call

                ; Scoped calls (e.g., Module::function())
                (call_expression
                    function: (scoped_identifier
                        name: (identifier) @callee)) @call

                ; Macro invocations
                (macro_invocation
                    macro: (identifier) @callee) @macro

                ; Use statements (imports)
                (use_declaration
                    argument: (scoped_identifier
                        name: (identifier) @callee)) @import
                (use_declaration
                    argument: (identifier) @callee) @import

                ; Struct instantiation
                (struct_expression
                    name: (type_identifier) @callee) @instantiation
                "#
            }
            Language::Python => {
                r#"
                ; Function calls (also covers class instantiation in Python)
                (call
                    function: (identifier) @callee) @call

                ; Method calls
                (call
                    function: (attribute
                        attribute: (identifier) @callee)) @method_call

                ; Import statements
                (import_statement
                    name: (dotted_name
                        (identifier) @callee)) @import
                (import_from_statement
                    name: (dotted_name
                        (identifier) @callee)) @import

                ; Type annotations (Python 3.5+)
                (type
                    (identifier) @callee) @type_ref

                ; Decorator usage
                (decorator
                    (identifier) @callee) @decorator
                (decorator
                    (call
                        function: (identifier) @callee)) @decorator
                "#
            }
            Language::JavaScript => {
                r#"
                ; Function calls
                (call_expression
                    function: (identifier) @callee) @call

                ; Method calls
                (call_expression
                    function: (member_expression
                        property: (property_identifier) @callee)) @method_call

                ; New expressions (instantiation)
                (new_expression
                    constructor: (identifier) @callee) @instantiation

                ; Import statements
                (import_statement
                    (import_clause
                        (identifier) @callee)) @import
                (import_statement
                    (import_clause
                        (named_imports
                            (import_specifier
                                name: (identifier) @callee)))) @import

                ; CommonJS require() — captures the local binding name as the callee.
                ; @fn_name is used to validate the call is actually require().
                ; @require_path captures the string argument so the indexer can
                ; resolve relative paths to actual exported symbol names.
                (variable_declarator
                    name: (identifier) @callee
                    value: (call_expression
                        function: (identifier) @fn_name
                        arguments: (arguments (string) @require_path))) @require_import

                ; CommonJS shorthand destructure: const { foo } = require('...')
                ; The property name is both the original and local name.
                (variable_declarator
                    name: (object_pattern
                        (shorthand_property_identifier_pattern) @callee)
                    value: (call_expression
                        function: (identifier) @fn_name
                        arguments: (arguments (string)))) @require_import

                ; CommonJS renamed destructure: const { foo: bar } = require('...')
                ; @callee captures the original exported name; @import_alias captures
                ; the local binding so callers of `bar` can be traced back to `foo`.
                (variable_declarator
                    name: (object_pattern
                        (pair_pattern
                            key: (property_identifier) @callee
                            value: (identifier) @import_alias))
                    value: (call_expression
                        function: (identifier) @fn_name
                        arguments: (arguments (string)))) @require_import_renamed

                ; JSX elements (React components)
                (jsx_element
                    open_tag: (jsx_opening_element
                        name: (identifier) @callee)) @instantiation
                (jsx_self_closing_element
                    name: (identifier) @callee) @instantiation
                "#
            }
            Language::TypeScript => {
                r#"
                ; Function calls
                (call_expression
                    function: (identifier) @callee) @call

                ; Method calls
                (call_expression
                    function: (member_expression
                        property: (property_identifier) @callee)) @method_call

                ; New expressions (instantiation)
                (new_expression
                    constructor: (identifier) @callee) @instantiation

                ; Import statements
                (import_statement
                    (import_clause
                        (identifier) @callee)) @import
                (import_statement
                    (import_clause
                        (named_imports
                            (import_specifier
                                name: (identifier) @callee)))) @import

                ; CommonJS require() — captures the local binding name as the callee.
                ; @fn_name is used to validate the call is actually require().
                ; @require_path captures the string argument so the indexer can
                ; resolve relative paths to actual exported symbol names.
                (variable_declarator
                    name: (identifier) @callee
                    value: (call_expression
                        function: (identifier) @fn_name
                        arguments: (arguments (string) @require_path))) @require_import

                ; CommonJS shorthand destructure: const { foo } = require('...')
                (variable_declarator
                    name: (object_pattern
                        (shorthand_property_identifier_pattern) @callee)
                    value: (call_expression
                        function: (identifier) @fn_name
                        arguments: (arguments (string)))) @require_import

                ; CommonJS renamed destructure: const { foo: bar } = require('...')
                (variable_declarator
                    name: (object_pattern
                        (pair_pattern
                            key: (property_identifier) @callee
                            value: (identifier) @import_alias))
                    value: (call_expression
                        function: (identifier) @fn_name
                        arguments: (arguments (string)))) @require_import_renamed

                ; Type annotations
                (type_annotation
                    (type_identifier) @callee) @type_ref
                "#
            }
            Language::Go => {
                r#"
                ; Function calls
                (call_expression
                    function: (identifier) @callee) @call

                ; Package-qualified calls (also covers method calls on package variables)
                (call_expression
                    function: (selector_expression
                        operand: (identifier) @_pkg
                        field: (field_identifier) @callee)) @call

                ; Type references
                (type_identifier) @type_ref

                ; Import statements
                (import_spec
                    path: (interpreted_string_literal) @callee) @import

                ; Struct instantiation (composite literal)
                (composite_literal
                    type: (type_identifier) @callee) @instantiation
                "#
            }
            Language::Php => {
                r#"
                ; Function calls
                (function_call_expression
                    function: (name) @callee) @call

                ; Method calls
                (member_call_expression
                    name: (name) @callee) @method_call

                ; Static method calls
                (scoped_call_expression
                    name: (name) @callee) @method_call

                ; New expressions (instantiation)
                (object_creation_expression
                    (name) @callee) @instantiation

                ; Use statements (imports)
                (namespace_use_clause
                    (qualified_name) @callee) @import

                ; Class extends
                (base_clause
                    (name) @callee) @inheritance

                ; Interface implements
                (class_interface_clause
                    (name) @callee) @implementation

                ; Type hints
                (type_list
                    (named_type
                        (name) @callee)) @type_ref
                "#
            }
            Language::Cpp => {
                r#"
                ; Function calls
                (call_expression
                    function: (identifier) @callee) @call

                ; Method calls
                (call_expression
                    function: (field_expression
                        field: (field_identifier) @callee)) @method_call

                ; Scoped calls (namespace::function)
                (call_expression
                    function: (qualified_identifier
                        name: (identifier) @callee)) @call

                ; Constructor calls (new)
                (new_expression
                    type: (type_identifier) @callee) @instantiation

                ; Type references
                (type_identifier) @type_ref

                ; Include statements
                (preproc_include
                    path: (string_literal) @callee) @import
                (preproc_include
                    path: (system_lib_string) @callee) @import

                ; Template arguments
                (template_argument_list
                    (type_descriptor
                        type: (type_identifier) @callee)) @generic

                ; Inheritance
                (base_class_clause
                    (type_identifier) @callee) @inheritance
                "#
            }
            Language::HCL => {
                r#"
                ; Function calls
                (function_call
                    (identifier) @callee) @call

                ; Variable references
                (variable_expr
                    (identifier) @callee) @variable_ref

                ; Block references (resource, data, module)
                (block
                    (identifier) @callee) @call
                "#
            }
            Language::Swift => {
                r#"
                ; Simple function calls: foo()
                (call_expression
                    (simple_identifier) @callee) @call

                ; Method calls: obj.method()
                (call_expression
                    (navigation_expression
                        suffix: (navigation_suffix
                            suffix: (simple_identifier) @callee))) @method_call

                ; Import statements: import Foundation
                (import_declaration
                    (identifier (simple_identifier) @callee)) @import

                ; Type references (user-defined types in annotations, generics, etc.)
                (user_type (type_identifier) @callee) @type_ref

                ; Inheritance / protocol conformance
                (inheritance_specifier (user_type (type_identifier) @callee)) @inheritance
                "#
            }
            Language::Kotlin => {
                r#"
                ; Simple function calls: foo(...)
                (call_expression (identifier) @callee) @call

                ; Method calls: obj.bar(...) — anchor captures only the method name
                (call_expression
                    (navigation_expression (identifier) @callee .)) @method_call

                ; Type references in annotations, generics, supertypes, etc.
                (user_type (identifier) @callee) @type_ref

                ; Import statements — capture the last segment of a dotted path
                (import (qualified_identifier (identifier) @callee .)) @import
                ; Single-segment imports (no dots)
                (import (identifier) @callee) @import

                ; Class/interface inheritance and delegation
                (delegation_specifier
                    (constructor_invocation
                        (user_type (identifier) @callee))) @inheritance
                (delegation_specifier
                    (user_type (identifier) @callee)) @inheritance
                "#
            }
            Language::Unknown => "",
        }
    }

    fn capture_to_reference_kind(capture_name: &str) -> ReferenceKind {
        match capture_name {
            "call" => ReferenceKind::Call,
            "method_call" => ReferenceKind::MethodCall,
            "type_ref" => ReferenceKind::TypeReference,
            "import" => ReferenceKind::Import,
            // CommonJS require() bindings — validated separately by the fn_name filter.
            // `require_import_renamed` also carries an @import_alias capture for renamed
            // destructured properties (e.g. `const { foo: bar } = require(...)`).
            "require_import" | "require_import_renamed" => ReferenceKind::Import,
            "instantiation" => ReferenceKind::Instantiation,
            "macro" => ReferenceKind::MacroInvocation,
            "decorator" => ReferenceKind::MacroInvocation,
            "inheritance" => ReferenceKind::Inheritance,
            "implementation" => ReferenceKind::Implementation,
            "generic" => ReferenceKind::GenericArgument,
            "variable_ref" => ReferenceKind::VariableReference,
            _ => ReferenceKind::Unknown,
        }
    }

    /// Collect all scopes (functions, classes, etc.) from the file in one pass.
    /// Returns a Vec of `ScopeInfo` for each scope.
    /// This avoids repeated Query creation and tree traversal for each reference.
    fn collect_scopes(
        &self,
        content: &str,
        tree: &tree_sitter::Tree,
        language: Language,
    ) -> Vec<ScopeInfo> {
        let ts_language = match self.get_ts_language(language) {
            Some(lang) => lang,
            None => return Vec::new(),
        };

        let query_source = self.get_query_patterns(language);
        if query_source.is_empty() {
            return Vec::new();
        }

        let query = match Query::new(&ts_language, query_source) {
            Ok(q) => q,
            Err(_) => return Vec::new(),
        };

        let mut cursor = QueryCursor::new();
        let text_bytes = content.as_bytes();
        let capture_names: Vec<&str> = query.capture_names().to_vec();

        let mut scopes = Vec::new();
        let mut matches_iter = cursor.matches(&query, tree.root_node(), text_bytes);

        while let Some(query_match) = matches_iter.next() {
            let mut symbol_name: Option<String> = None;
            let mut parent_symbol: Option<String> = None;
            let mut main_node = None;
            let mut kind = String::new();

            for capture in query_match.captures {
                let capture_name = capture_names
                    .get(capture.index as usize)
                    .copied()
                    .unwrap_or("");

                if capture_name == "name" {
                    symbol_name = Some(content[capture.node.byte_range()].to_string());
                } else if capture_name.ends_with(".name") {
                    parent_symbol = Some(content[capture.node.byte_range()].to_string());
                } else {
                    main_node = Some(capture.node);
                    kind = capture_name.to_string();
                }
            }

            if let (Some(node), Some(name)) = (main_node, symbol_name) {
                let start_line = node.start_position().row as u32 + 1;
                let end_line = node.end_position().row as u32 + 1;
                scopes.push(ScopeInfo {
                    start_line,
                    end_line,
                    name,
                    parent: parent_symbol,
                    kind,
                });
            }
        }

        scopes
    }

    /// Returns `true` if `node` is a `member_expression` that represents `module.exports`.
    fn is_module_exports(node: tree_sitter::Node<'_>, content: &str) -> bool {
        node.kind() == "member_expression"
            && node
                .child_by_field_name("object")
                .map(|n| &content[n.byte_range()] == "module")
                .unwrap_or(false)
            && node
                .child_by_field_name("property")
                .map(|n| &content[n.byte_range()] == "exports")
                .unwrap_or(false)
    }

    /// Recursively walks `node` and appends exported symbol names to `exports`.
    ///
    /// Handles:
    /// - `module.exports = identifier`
    /// - `module.exports.key = expression`
    /// - `export default identifier`
    /// - `export function/class/const identifier`
    /// - `export { identifier, identifier as alias }`
    fn collect_exports_from_node(
        node: tree_sitter::Node<'_>,
        content: &str,
        exports: &mut HashSet<String>,
    ) {
        match node.kind() {
            "assignment_expression" => {
                if let Some(left) = node.child_by_field_name("left") {
                    if Self::is_module_exports(left, content) {
                        // module.exports = someIdentifier
                        // module.exports = function namedFn(...) { ... }
                        if let Some(right) = node.child_by_field_name("right") {
                            if right.kind() == "identifier" {
                                exports.insert(content[right.byte_range()].to_string());
                            } else if matches!(right.kind(), "function_expression" | "function") {
                                // Named function expression: `module.exports = function foo() {}`
                                if let Some(name_node) = right.child_by_field_name("name") {
                                    exports.insert(content[name_node.byte_range()].to_string());
                                }
                            }
                        }
                    } else if left.kind() == "member_expression" {
                        // module.exports.key = ...
                        if let Some(obj) = left.child_by_field_name("object") {
                            if Self::is_module_exports(obj, content) {
                                if let Some(prop) = left.child_by_field_name("property") {
                                    exports.insert(content[prop.byte_range()].to_string());
                                }
                            }
                        }
                    }
                }
            }
            "export_statement" => {
                // export default someIdentifier
                if let Some(val) = node.child_by_field_name("value") {
                    if val.kind() == "identifier" {
                        exports.insert(content[val.byte_range()].to_string());
                    }
                }
                // export function foo / export class Foo
                if let Some(decl) = node.child_by_field_name("declaration") {
                    if let Some(name_node) = decl.child_by_field_name("name") {
                        exports.insert(content[name_node.byte_range()].to_string());
                    }
                    // export const/let/var foo = ...
                    if matches!(decl.kind(), "lexical_declaration" | "variable_declaration") {
                        for i in 0..decl.child_count() {
                            if let Some(declarator) = decl.child(i as u32) {
                                if declarator.kind() == "variable_declarator" {
                                    if let Some(n) = declarator.child_by_field_name("name") {
                                        if n.kind() == "identifier" {
                                            exports.insert(content[n.byte_range()].to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // export { foo, bar as baz }
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32) {
                        if child.kind() == "export_clause" {
                            for j in 0..child.child_count() {
                                if let Some(spec) = child.child(j as u32) {
                                    if spec.kind() == "export_specifier" {
                                        // Use the alias if present (e.g. `bar` in `export { foo as bar }`),
                                        // falling back to the original name. The alias is the externally
                                        // visible symbol that callers will reference.
                                        let exported_name = spec
                                            .child_by_field_name("alias")
                                            .or_else(|| spec.child_by_field_name("name"))
                                            .map(|n| content[n.byte_range()].to_string());
                                        if let Some(name) = exported_name {
                                            exports.insert(name);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                Self::collect_exports_from_node(child, content, exports);
            }
        }
    }
}

/// Walk up the AST from `node` to find the innermost enclosing class, trait,
/// or interface and return its name.  Returns `None` when the node is not
/// nested inside a class body (e.g. a top-level function).
///
/// This avoids relying on the `enclosing_scope` value supplied by the scope
/// collection query, which uses flat (non-nested) patterns and therefore
/// never captures the parent class name for methods.
fn find_enclosing_class(
    node: tree_sitter::Node<'_>,
    content: &str,
    language: Language,
) -> Option<String> {
    find_enclosing_class_node(node, language)
        .and_then(|cls| cls.child_by_field_name("name"))
        .map(|n| content[n.byte_range()].to_string())
}

/// Walk up the AST from `node` to find the innermost enclosing class / trait /
/// interface AST node.  Returns the node itself so callers can inspect its
/// children (e.g. `base_clause` for PHP extends).
fn find_enclosing_class_node<'tree>(
    node: tree_sitter::Node<'tree>,
    language: Language,
) -> Option<tree_sitter::Node<'tree>> {
    let class_kinds: &[&str] = match language {
        Language::Php => &[
            "class_declaration",
            "trait_declaration",
            "interface_declaration",
        ],
        Language::Python => &["class_definition"],
        Language::JavaScript | Language::TypeScript => &["class_declaration"],
        _ => return None,
    };

    let mut current = node;
    loop {
        let parent = current.parent()?;
        if class_kinds.contains(&parent.kind()) {
            return Some(parent);
        }
        current = parent;
    }
}

/// For a PHP `parent::method()` call, resolve the actual parent class name by
/// finding the `base_clause` (extends) of the enclosing class declaration.
///
/// ```php
/// class Child extends Base {
///     public function foo() { parent::bar(); }
///     //                      ^^^^^^^^^^^^^ → "Base::bar"
/// }
/// ```
///
/// Returns `None` when there is no enclosing class or when the class has no
/// `extends` clause (traits / interfaces don't have `base_clause`).
fn find_php_parent_class(node: tree_sitter::Node<'_>, content: &str) -> Option<String> {
    let class_node = find_enclosing_class_node(node, Language::Php)?;
    // PHP tree-sitter grammar: class_declaration has an optional base_clause
    // child that wraps the parent class name.
    //   (class_declaration name: (name) (base_clause (name)) …)
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() == "base_clause" {
            // The base_clause contains one or more (name) children; the first
            // is the extended class.
            let mut inner_cursor = child.walk();
            for base_child in child.children(&mut inner_cursor) {
                if base_child.kind() == "name" || base_child.kind() == "qualified_name" {
                    return Some(content[base_child.byte_range()].to_string());
                }
            }
        }
    }
    None
}

/// Extract a class name from a PHP type node, handling `named_type`,
/// `optional_type` (`?Foo`), and ignoring primitives / union types.
fn extract_php_class_from_type(type_node: tree_sitter::Node<'_>, content: &str) -> Option<String> {
    match type_node.kind() {
        "named_type" => {
            // named_type contains a `name` child with the class identifier.
            let mut cursor = type_node.walk();
            for child in type_node.children(&mut cursor) {
                if child.kind() == "name" || child.kind() == "qualified_name" {
                    return Some(content[child.byte_range()].to_string());
                }
            }
            None
        }
        "optional_type" => {
            // ?Foo — unwrap and recurse into the inner type.
            let mut cursor = type_node.walk();
            for child in type_node.children(&mut cursor) {
                if let Some(name) = extract_php_class_from_type(child, content) {
                    return Some(name);
                }
            }
            None
        }
        // primitive_type, union_type, intersection_type — can't resolve to a
        // single class, so return None.
        _ => None,
    }
}

/// Collect a mapping of PHP variable names to their class types by scanning
/// the AST for:
///
/// - **Parameter type hints**: `function foo(Home $home)` → `$home → Home`
/// - **Constructor promotion**: `private Home $home` in parameter lists → `$home → Home`
/// - **Property declarations**: `private Home $home;` → `$home → Home`
/// - **`new` assignments**: `$home = new Home()` → `$home → Home`
///
/// The returned map uses the bare variable name (without `$`) as keys.
/// Only PHP files are scanned; returns an empty map for other languages.
fn collect_php_variable_types(
    tree: &tree_sitter::Tree,
    content: &str,
    language: Language,
) -> HashMap<String, String> {
    let mut var_types: HashMap<String, String> = HashMap::new();

    if language != Language::Php {
        return var_types;
    }

    fn walk_node(
        node: tree_sitter::Node<'_>,
        content: &str,
        var_types: &mut HashMap<String, String>,
    ) {
        match node.kind() {
            // function foo(Home $home) — simple_parameter with a type field
            "simple_parameter" => {
                if let Some(type_node) = node.child_by_field_name("type") {
                    if let Some(class_name) = extract_php_class_from_type(type_node, content) {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let var_name = content[name_node.byte_range()].to_string();
                            // Strip leading $ if present
                            let key = var_name.strip_prefix('$').unwrap_or(&var_name);
                            var_types.insert(key.to_string(), class_name);
                        }
                    }
                }
            }
            // PHP 8.0 constructor promotion: private Home $home
            "property_promotion_parameter" => {
                let mut cursor = node.walk();
                let mut found_type: Option<String> = None;
                for child in node.children(&mut cursor) {
                    if found_type.is_none() {
                        if let Some(class_name) = extract_php_class_from_type(child, content) {
                            found_type = Some(class_name);
                        }
                    }
                    if child.kind() == "variable_name" {
                        if let Some(ref class_name) = found_type {
                            let var_name = content[child.byte_range()].to_string();
                            let key = var_name.strip_prefix('$').unwrap_or(&var_name);
                            var_types.insert(key.to_string(), class_name.clone());
                        }
                    }
                }
            }
            // private Home $home;
            "property_declaration" => {
                if let Some(type_node) = node.child_by_field_name("type") {
                    if let Some(class_name) = extract_php_class_from_type(type_node, content) {
                        let mut cursor = node.walk();
                        for child in node.children(&mut cursor) {
                            if child.kind() == "property_element" {
                                let mut inner = child.walk();
                                for prop_child in child.children(&mut inner) {
                                    if prop_child.kind() == "variable_name" {
                                        let var_name = content[prop_child.byte_range()].to_string();
                                        let key = var_name.strip_prefix('$').unwrap_or(&var_name);
                                        var_types.insert(key.to_string(), class_name.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // $home = new Home()
            "assignment_expression" => {
                if let (Some(left), Some(right)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) {
                    if left.kind() == "variable_name"
                        && right.kind() == "object_creation_expression"
                    {
                        // The class name is a `name` child of object_creation_expression.
                        let mut cursor = right.walk();
                        for child in right.children(&mut cursor) {
                            if child.kind() == "name" || child.kind() == "qualified_name" {
                                let class_name = content[child.byte_range()].to_string();
                                let var_name = content[left.byte_range()].to_string();
                                let key = var_name.strip_prefix('$').unwrap_or(&var_name);
                                var_types.insert(key.to_string(), class_name);
                                break;
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        // Recurse into children.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk_node(child, content, var_types);
        }
    }

    walk_node(tree.root_node(), content, &mut var_types);
    var_types
}

/// Qualify a callee name with its receiver class where that is deterministic
/// from the AST — without requiring type inference.
///
/// Handles:
/// - PHP/other static calls (`Foo::bar()`, `self::bar()`, `static::bar()`) —
///   the class name is taken from the `scope` field of `scoped_call_expression`.
/// - PHP `$this->method()` — receiver is the literal `$this` token; the
///   enclosing class is found by walking up the AST.
/// - Python `self.method()` / `cls.method()` — same idea via the `attribute` node.
/// - JS/TS `this.method()` — via `member_expression`.
///
/// Returns the unmodified `name` for any case where the receiver class cannot be
/// determined statically (e.g. `$obj->method()`, `obj.method()`).
///
/// `file_class_names` contains class names defined in the current file,
/// used by JS/TS to qualify `ClassName.method()` static calls.
///
/// `php_var_types` maps PHP variable names (without `$`) to their class types,
/// used to qualify `$obj->method()` calls when the variable's type is known.
fn qualify_callee_name(
    name: String,
    node: tree_sitter::Node<'_>,
    content: &str,
    enclosing_scope: &Option<String>,
    language: Language,
    file_class_names: &HashSet<String>,
    php_var_types: &HashMap<String, String>,
) -> String {
    let parent = match node.parent() {
        Some(p) => p,
        None => return name,
    };

    match language {
        Language::Php => match parent.kind() {
            "scoped_call_expression" => {
                // Foo::bar(), self::bar(), parent::bar(), static::bar()
                if let Some(scope_node) = parent.child_by_field_name("scope") {
                    let scope_text = &content[scope_node.byte_range()];
                    return match scope_text {
                        "self" | "static" => {
                            // Walk the AST to find the actual enclosing class name;
                            // fall back to the scope-collection value if AST walk fails.
                            find_enclosing_class(node, content, language)
                                .or_else(|| enclosing_scope.clone())
                                .map(|class| format!("{}::{}", class, name))
                                .unwrap_or(name)
                        }
                        "parent" => {
                            // Resolve the actual parent class from the extends clause.
                            // Falls back to bare name if the class doesn't extend anything.
                            find_php_parent_class(node, content)
                                .map(|parent_class| format!("{}::{}", parent_class, name))
                                .unwrap_or(name)
                        }
                        class_name => format!("{}::{}", class_name, name),
                    };
                }
                name
            }
            "member_call_expression" => {
                // $this->method() — qualify with the enclosing class.
                // PHP tree-sitter represents $this as a variable_name node whose
                // full text is "$this"; guard against both forms for safety.
                if let Some(obj_node) = parent.child_by_field_name("object") {
                    let obj_text = &content[obj_node.byte_range()];
                    if obj_text == "$this" || obj_text == "this" {
                        let class = find_enclosing_class(node, content, language)
                            .or_else(|| enclosing_scope.clone());
                        if let Some(class) = class {
                            return format!("{}::{}", class, name);
                        }
                    } else {
                        // $obj->method() — look up the variable's type from
                        // parameter hints, property declarations, or `new` assignments.
                        let var_name = obj_text.strip_prefix('$').unwrap_or(obj_text);
                        if let Some(class) = php_var_types.get(var_name) {
                            return format!("{}::{}", class, name);
                        }
                    }
                }
                name
            }
            _ => name,
        },

        Language::Python => {
            // self.method() / cls.method()
            // The @callee node is the `attribute` field of an `attribute` AST node.
            if parent.kind() == "attribute" {
                if let Some(obj_node) = parent.child_by_field_name("object") {
                    let obj_text = &content[obj_node.byte_range()];
                    if obj_text == "self" || obj_text == "cls" {
                        let class = find_enclosing_class(node, content, language)
                            .or_else(|| enclosing_scope.clone());
                        if let Some(class) = class {
                            return format!("{}::{}", class, name);
                        }
                    }
                }
            }
            name
        }

        Language::JavaScript | Language::TypeScript => {
            // The @callee node is the `property` field of a `member_expression` node.
            // JS/TS uses dot notation for qualified names: `ClassName.method`.
            if parent.kind() == "member_expression" {
                if let Some(obj_node) = parent.child_by_field_name("object") {
                    let obj_text = &content[obj_node.byte_range()];
                    if obj_text == "this" {
                        // this.method() — qualify with the enclosing class.
                        let class = find_enclosing_class(node, content, language)
                            .or_else(|| enclosing_scope.clone());
                        if let Some(class) = class {
                            return format!("{}.{}", class, name);
                        }
                    } else if file_class_names.contains(obj_text) {
                        // ClassName.staticMethod() — the receiver is a class
                        // defined in this file.
                        return format!("{}.{}", obj_text, name);
                    }
                }
            }
            name
        }

        _ => name,
    }
}

/// Look up the tightest enclosing scope for a given line.
/// Returns (name, parent) of the tightest scope containing the line.
fn lookup_enclosing_scope(scopes: &[ScopeInfo], line: u32) -> Option<(String, Option<String>)> {
    let mut best_match: Option<&ScopeInfo> = None;

    for scope in scopes {
        // Check if this scope contains our target line
        if scope.start_line <= line && scope.end_line >= line {
            let is_better = match best_match {
                None => true,
                Some(best) => {
                    // Prefer tighter scope (smaller range that still contains the line)
                    (scope.end_line - scope.start_line) < (best.end_line - best.start_line)
                }
            };

            if is_better {
                best_match = Some(scope);
            }
        }
    }

    best_match.map(|s| (s.name.clone(), s.parent.clone()))
}

impl Default for TreeSitterParser {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ParserService for TreeSitterParser {
    async fn parse_file(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
    ) -> Result<Vec<CodeChunk>, DomainError> {
        let ts_language = self
            .get_ts_language(language)
            .ok_or_else(|| DomainError::parse(format!("Unsupported language: {:?}", language)))?;

        let mut parser = Parser::new();
        parser
            .set_language(&ts_language)
            .map_err(|e| DomainError::parse(format!("Failed to set language: {}", e)))?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| DomainError::parse("Failed to parse file"))?;

        let query_source = self.get_query_patterns(language);
        if query_source.is_empty() {
            return Ok(Vec::new());
        }

        let query = Query::new(&ts_language, query_source)
            .map_err(|e| DomainError::parse(format!("Failed to create query: {}", e)))?;

        let mut cursor = QueryCursor::new();
        let text_bytes = content.as_bytes();

        let mut chunks = Vec::new();
        let capture_names: Vec<&str> = query.capture_names().to_vec();

        let mut matches_iter = cursor.matches(&query, tree.root_node(), text_bytes);

        while let Some(query_match) = matches_iter.next() {
            let mut symbol_name: Option<String> = None;
            let mut parent_symbol: Option<String> = None;
            let mut main_node = None;
            let mut node_type = NodeType::Block;

            for capture in query_match.captures {
                let capture_name = capture_names
                    .get(capture.index as usize)
                    .copied()
                    .unwrap_or("");

                if capture_name == "name" {
                    symbol_name = Some(content[capture.node.byte_range()].to_string());
                } else if capture_name.ends_with(".name") {
                    parent_symbol = Some(content[capture.node.byte_range()].to_string());
                } else {
                    main_node = Some(capture.node);
                    node_type = Self::capture_to_node_type(capture_name);
                }
            }

            if let Some(node) = main_node {
                let start_line = node.start_position().row as u32 + 1;
                let end_line = node.end_position().row as u32 + 1;
                let node_content = content[node.byte_range()].to_string();

                if node_content.trim().len() < 10 {
                    continue;
                }

                let mut chunk = CodeChunk::new(
                    file_path.to_string(),
                    node_content,
                    start_line,
                    end_line,
                    language,
                    node_type,
                    repository_id.to_string(),
                );

                if let Some(name) = symbol_name {
                    chunk = chunk.with_symbol_name(name);
                }

                if let Some(parent) = parent_symbol {
                    chunk = chunk.with_parent_symbol(parent);
                }

                chunks.push(chunk);
            }
        }

        debug!(
            "Parsed {} chunks from {} ({:?})",
            chunks.len(),
            file_path,
            language
        );

        Ok(chunks)
    }

    fn supported_languages(&self) -> Vec<Language> {
        self.supported_languages.clone()
    }

    async fn extract_module_exports(&self, content: &str, language: Language) -> Vec<String> {
        // Only JS/TS files have module exports.
        if !matches!(language, Language::JavaScript | Language::TypeScript) {
            return Vec::new();
        }

        let ts_language = match self.get_ts_language(language) {
            Some(l) => l,
            None => return Vec::new(),
        };

        // A full tree-sitter parse is CPU-bound; offload it to the blocking thread pool
        // so we don't stall the async runtime.
        let content = content.to_string();
        match tokio::task::spawn_blocking(move || {
            let mut parser = Parser::new();
            if parser.set_language(&ts_language).is_err() {
                return Vec::new();
            }
            let tree = match parser.parse(&content, None) {
                Some(t) => t,
                None => return Vec::new(),
            };

            let mut exports: HashSet<String> = HashSet::new();
            Self::collect_exports_from_node(tree.root_node(), &content, &mut exports);
            exports.into_iter().collect()
        })
        .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!("failed to join export extraction task: {}", e);
                Vec::new()
            }
        }
    }

    async fn extract_references(
        &self,
        content: &str,
        file_path: &str,
        language: Language,
        repository_id: &str,
        exports_by_file: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<SymbolReference>, DomainError> {
        let ts_language = self
            .get_ts_language(language)
            .ok_or_else(|| DomainError::parse(format!("Unsupported language: {:?}", language)))?;

        let mut parser = Parser::new();
        parser
            .set_language(&ts_language)
            .map_err(|e| DomainError::parse(format!("Failed to set language: {}", e)))?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| DomainError::parse("Failed to parse file"))?;

        let query_source = self.get_reference_query_patterns(language);
        if query_source.is_empty() {
            return Ok(Vec::new());
        }

        let query = Query::new(&ts_language, query_source)
            .map_err(|e| DomainError::parse(format!("Failed to create reference query: {}", e)))?;

        let mut cursor = QueryCursor::new();
        let text_bytes = content.as_bytes();

        // Collect all scopes once for efficient lookup
        let scopes = self.collect_scopes(content, &tree, language);

        // Build a set of class names defined in this file so that
        // `qualify_callee_name` can recognise `ClassName.method()` calls in
        // JS/TS and qualify them as `ClassName.method`.
        let class_names: HashSet<String> = scopes
            .iter()
            .filter(|s| s.kind == "class")
            .map(|s| s.name.clone())
            .collect();

        // Collect PHP variable-to-class-type mappings from parameter type hints,
        // property declarations, and `new` assignments.  Empty for non-PHP files.
        let php_var_types = collect_php_variable_types(&tree, content, language);

        let mut references = Vec::new();
        // Tracks the raw require() path for each reference at the same index.
        // Used in the resolution pass below; avoids needing a field on SymbolReference.
        let mut require_paths: Vec<Option<String>> = Vec::new();
        let capture_names: Vec<&str> = query.capture_names().to_vec();

        let mut matches_iter = cursor.matches(&query, tree.root_node(), text_bytes);

        while let Some(query_match) = matches_iter.next() {
            let mut callee_name: Option<String> = None;
            let mut reference_kind = ReferenceKind::Unknown;
            let mut ref_node = None;
            // Auxiliary capture used by the require_import pattern to record the
            // called function identifier so we can validate it is actually "require".
            let mut fn_name: Option<String> = None;
            let mut is_require_import = false;
            // Local alias from a renamed import/require (e.g. `bar` in `{ foo: bar }`).
            let mut query_import_alias: Option<String> = None;
            // Raw string argument of a require() call (e.g. `'./module.js'`).
            // Used in the post-loop resolution pass for cross-file export resolution.
            let mut require_path_raw: Option<String> = None;

            for capture in query_match.captures {
                let capture_name = capture_names
                    .get(capture.index as usize)
                    .copied()
                    .unwrap_or("");

                if capture_name == "callee" {
                    // "callee" always takes priority - set name and node unconditionally
                    callee_name = Some(content[capture.node.byte_range()].to_string());
                    ref_node = Some(capture.node);
                } else if capture_name == "require_path" {
                    // The string literal passed to require() — kept verbatim (including quotes)
                    // so normalize_import_path can strip them later.
                    require_path_raw = Some(content[capture.node.byte_range()].to_string());
                } else if capture_name == "import_alias" {
                    // Local alias captured directly by the query pattern (CommonJS
                    // renamed destructure: `const { foo: bar } = require(...)`).
                    query_import_alias = Some(content[capture.node.byte_range()].to_string());
                } else if capture_name == "type_ref" {
                    // For type_ref, set the kind but only set name/node if not already set by "callee"
                    reference_kind = ReferenceKind::TypeReference;
                    if callee_name.is_none() {
                        callee_name = Some(content[capture.node.byte_range()].to_string());
                        ref_node = Some(capture.node);
                    }
                } else if capture_name == "fn_name" {
                    // Auxiliary: records the function identifier for require_import filtering.
                    fn_name = Some(content[capture.node.byte_range()].to_string());
                } else {
                    // This is the outer capture (call, method_call, etc.)
                    if reference_kind == ReferenceKind::Unknown {
                        reference_kind = Self::capture_to_reference_kind(capture_name);
                    }
                    if capture_name == "require_import" || capture_name == "require_import_renamed"
                    {
                        is_require_import = true;
                    }
                }
            }

            // For require_import patterns the called function must be "require".
            // Any other function that happens to match the variable_declarator
            // pattern (e.g. `const x = someFactory("path")`) is discarded.
            if is_require_import && fn_name.as_deref() != Some("require") {
                continue;
            }

            if let (Some(mut name), Some(node)) = (callee_name, ref_node) {
                // Normalize import paths: strip surrounding quotes/brackets
                // Go imports: "fmt" -> fmt
                // C++ includes: "header.h" -> header.h, <iostream> -> iostream
                if reference_kind == ReferenceKind::Import {
                    name = normalize_import_path(&name);
                }

                // Skip very short names (likely noise), common keywords, and primitive types
                if name.len() < 2
                    || matches!(
                        name.as_str(),
                        // Common keywords
                        "if" | "else"
                            | "for"
                            | "while"
                            | "return"
                            | "true"
                            | "false"
                            | "null"
                            | "None"
                            | "self"
                            | "this"
                            | "super"
                            // Common primitive types (to reduce noise from bare type_identifier patterns)
                            | "int"
                            | "i8"
                            | "i16"
                            | "i32"
                            | "i64"
                            | "i128"
                            | "u8"
                            | "u16"
                            | "u32"
                            | "u64"
                            | "u128"
                            | "f32"
                            | "f64"
                            | "bool"
                            | "char"
                            | "str"
                            | "void"
                            | "string"
                            | "float"
                            | "double"
                            | "byte"
                            | "short"
                            | "long"
                            | "usize"
                            | "isize"
                            // Swift capitalized primitive / standard types
                            | "String"
                            | "Bool"
                            | "Double"
                            | "Float"
                            | "Int"
                            | "Int8"
                            | "Int16"
                            | "Int32"
                            | "Int64"
                            | "UInt"
                            | "UInt8"
                            | "UInt16"
                            | "UInt32"
                            | "UInt64"
                            | "Character"
                            // Kotlin core types
                            | "Unit"
                            | "Any"
                            | "Nothing"
                            | "Boolean"
                            | "Long"
                            | "Short"
                            | "Byte"
                    )
                {
                    continue;
                }

                let line = node.start_position().row as u32 + 1;
                let column = node.start_position().column as u32 + 1;

                // Determine the import alias:
                // 1. For CommonJS renamed destructure (`const { foo: bar } = require(...)`),
                //    the alias was captured directly in the query as @import_alias.
                // 2. For ES6 named imports with alias (`import { foo as bar } from '...'`),
                //    the alias is recorded on the import_specifier node's `alias` field.
                //    We inspect the node's parent to detect this without needing a separate
                //    query pattern (which would cause duplicate matches).
                let import_alias = if reference_kind == ReferenceKind::Import {
                    query_import_alias.clone().or_else(|| {
                        node.parent()
                            .filter(|p| p.kind() == "import_specifier")
                            .and_then(|p| p.child_by_field_name("alias"))
                            .map(|alias_node| content[alias_node.byte_range()].to_string())
                    })
                } else {
                    None
                };

                // Find enclosing scope (function/class that contains this reference)
                let (caller_symbol, enclosing_scope) = lookup_enclosing_scope(&scopes, line)
                    .map(|(name, parent)| (Some(name), parent))
                    .unwrap_or((None, None));

                // Qualify method callee names with receiver class where the AST
                // makes it unambiguous: `Foo::bar()` → "Foo::bar",
                // `$this->method()` / `self.method()` → "EnclosingClass::method".
                // Unknown-receiver calls (e.g. `$obj->method()`) stay as bare names.
                name = qualify_callee_name(
                    name,
                    node,
                    content,
                    &enclosing_scope,
                    language,
                    &class_names,
                    &php_var_types,
                );

                let mut reference = SymbolReference::new(
                    caller_symbol,
                    name,
                    file_path.to_string(),
                    file_path.to_string(),
                    line,
                    column,
                    reference_kind,
                    language,
                    repository_id.to_string(),
                );

                if let Some(scope) = enclosing_scope {
                    reference = reference.with_enclosing_scope(scope);
                }

                if let Some(alias) = import_alias {
                    reference = reference.with_import_alias(alias);
                }

                // Track the raw require() path for simple bindings so the resolution
                // pass below can map them to the actual exported symbol name.
                let raw_require = if is_require_import && query_import_alias.is_none() {
                    require_path_raw.clone()
                } else {
                    None
                };
                require_paths.push(raw_require);
                references.push(reference);
            }
        }

        debug!(
            "Extracted {} references from {} ({:?})",
            references.len(),
            file_path,
            language
        );

        // ── Cross-file require() resolution ──────────────────────────────────
        // For JS/TS files, resolve simple `const X = require('./path')` bindings
        // against the exports map built during the pre-scan phase.  We only do
        // this when the caller supplies a non-empty map.
        if matches!(language, Language::JavaScript | Language::TypeScript)
            && !exports_by_file.is_empty()
        {
            let file_dir = Path::new(file_path).parent().unwrap_or(Path::new(""));

            for (reference, maybe_require_path) in references.iter_mut().zip(require_paths.iter()) {
                // We only resolve simple `const X = require('./path')` bindings.
                // Destructured requires already store the exported property name as
                // callee_symbol, so they don't need cross-file resolution.
                if reference.reference_kind() != ReferenceKind::Import {
                    continue;
                }
                // Skip if there's already an alias set — that means this is a
                // destructured or ES6-aliased import that was already resolved.
                if reference.import_alias().is_some() {
                    continue;
                }

                let raw_require_path = match maybe_require_path {
                    Some(p) => p,
                    None => continue,
                };

                let normalized = normalize_import_path(raw_require_path);

                // Only resolve relative paths (./  or ../).  External packages such as
                // `require('express')` are not in the repo and cannot be resolved.
                if !normalized.starts_with("./") && !normalized.starts_with("../") {
                    continue;
                }

                // Resolve the relative path against the requiring file's directory.
                let resolved = file_dir.join(&normalized);
                // Clean the path by resolving `.` and `..` components so that a path
                // like `routes/../middlewares/x.js` becomes `middlewares/x.js` and
                // matches the normalised keys stored in exports_by_file.
                let resolved_clean = resolved.components().fold(
                    PathBuf::new(),
                    |mut acc, component| match component {
                        std::path::Component::CurDir => acc,
                        std::path::Component::ParentDir => {
                            acc.pop();
                            acc
                        }
                        other => {
                            acc.push(other);
                            acc
                        }
                    },
                );
                // Normalise to a forward-slash string (repo-relative, no leading ./).
                let resolved_str = resolved_clean.to_string_lossy().replace('\\', "/");
                // Strip a leading "./" if Path::join left one.
                let resolved_key = resolved_str
                    .strip_prefix("./")
                    .unwrap_or(&resolved_str)
                    .to_string();

                // Look up in the exports map — try exact match first, then without extension.
                let exported_symbols = exports_by_file.get(&resolved_key).or_else(|| {
                    // Try stripping the file extension (e.g. look up "foo/bar" when
                    // the require was `require('./bar.js')` and the map key is "foo/bar").
                    let without_ext = Path::new(&resolved_key)
                        .with_extension("")
                        .to_string_lossy()
                        .replace('\\', "/");
                    exports_by_file.get(without_ext.as_str())
                });

                let exported_symbols = match exported_symbols {
                    Some(s) if !s.is_empty() => s,
                    _ => continue,
                };

                // For a single default export, replace the local binding name with the
                // actual exported symbol and promote the local binding to import_alias.
                if exported_symbols.len() == 1 {
                    let local_binding = reference.callee_symbol().to_string();
                    *reference = reference
                        .clone()
                        .with_callee_symbol(exported_symbols[0].clone())
                        .with_import_alias(local_binding);
                }
                // If the file has multiple exports we cannot know which one this
                // binding refers to — leave callee_symbol as the local name.
            }
        }

        Ok(references)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_parse_rust_function() {
        let parser = TreeSitterParser::new();
        let content = r#"
fn hello_world() {
    println!("Hello, world!");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;

        let chunks = parser
            .parse_file(content, "test.rs", Language::Rust, "test-repo")
            .await
            .unwrap();

        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn test_parse_python_class() {
        let parser = TreeSitterParser::new();
        let content = r#"
class Calculator:
    def add(self, a, b):
        return a + b

    def subtract(self, a, b):
        return a - b
"#;

        let chunks = parser
            .parse_file(content, "calc.py", Language::Python, "test-repo")
            .await
            .unwrap();

        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn test_parse_php_class() {
        let parser = TreeSitterParser::new();
        let content = r#"
<?php
class Calculator {
    public function add($a, $b) {
        return $a + $b;
    }

    public function subtract($a, $b) {
        return $a - $b;
    }
}
"#;

        let chunks = parser
            .parse_file(content, "calc.php", Language::Php, "test-repo")
            .await
            .unwrap();

        assert!(!chunks.is_empty());
    }

    #[tokio::test]
    async fn test_parse_cpp_method_outside_class() {
        let parser = TreeSitterParser::new();
        let content = std::fs::read_to_string("tests/fixtures/sample_cpp.cpp")
            .expect("failed to read sample_cpp.cpp");

        let chunks = parser
            .parse_file(&content, "sample_cpp.cpp", Language::Cpp, "test-repo")
            .await
            .unwrap();

        let has_method = chunks
            .iter()
            .any(|chunk| chunk.symbol_name() == Some("calculate_area"));

        assert!(has_method, "expected calculate_area to be indexed");
    }

    #[tokio::test]
    async fn test_parse_cpp_does_not_duplicate_out_of_class_method() {
        let parser = TreeSitterParser::new();
        let content = std::fs::read_to_string("tests/fixtures/sample_cpp.cpp")
            .expect("failed to read sample_cpp.cpp");

        let chunks = parser
            .parse_file(&content, "sample_cpp.cpp", Language::Cpp, "test-repo")
            .await
            .unwrap();

        let area_count = chunks
            .iter()
            .filter(|chunk| chunk.symbol_name() == Some("calculate_area"))
            .count();
        assert_eq!(
            area_count, 1,
            "expected calculate_area to appear exactly once"
        );

        let unnamed_area_count = chunks
            .iter()
            .filter(|chunk| {
                chunk.node_type() == NodeType::Function
                    && chunk.symbol_name().is_none()
                    && chunk.content().contains("calculate_area")
            })
            .count();
        assert_eq!(
            unnamed_area_count, 0,
            "expected no unnamed function chunk containing calculate_area"
        );

        let main_count = chunks
            .iter()
            .filter(|chunk| chunk.symbol_name() == Some("main"))
            .count();
        assert_eq!(main_count, 1, "expected main to appear exactly once");
    }

    #[tokio::test]
    async fn test_extract_rust_function_calls() {
        let parser = TreeSitterParser::new();
        let content = r#"
fn helper() -> i32 {
    42
}

fn main() {
    let x = helper();
    println!("Result: {}", x);
}
"#;

        let references = parser
            .extract_references(
                content,
                "test.rs",
                Language::Rust,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // Should find call to `helper`
        let helper_calls: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "helper")
            .collect();
        assert!(!helper_calls.is_empty(), "Should find call to helper()");
        assert_eq!(helper_calls[0].reference_kind(), ReferenceKind::Call);

        // Should find macro invocation `println`
        let println_calls: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "println")
            .collect();
        assert!(
            !println_calls.is_empty(),
            "Should find println! macro invocation"
        );
    }

    #[tokio::test]
    async fn test_extract_python_calls_and_imports() {
        let parser = TreeSitterParser::new();
        let content = r#"
import os

def helper():
    return 42

def main():
    result = helper()
    os.path.exists("/tmp")
"#;

        let references = parser
            .extract_references(
                content,
                "test.py",
                Language::Python,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // Should find call to `helper`
        let helper_calls: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "helper")
            .collect();
        assert!(!helper_calls.is_empty(), "Should find call to helper()");

        // Should find import of `os`
        let os_imports: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "os" && r.reference_kind() == ReferenceKind::Import)
            .collect();
        assert!(!os_imports.is_empty(), "Should find import of os");
    }

    #[tokio::test]
    async fn test_extract_typescript_type_references() {
        let parser = TreeSitterParser::new();
        let content = r#"
interface User {
    name: string;
}

function greet(user: User): string {
    return user.name;
}
"#;

        let references = parser
            .extract_references(
                content,
                "test.ts",
                Language::TypeScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // Should find type reference to `User`
        let user_refs: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "User")
            .collect();
        assert!(!user_refs.is_empty(), "Should find type reference to User");
    }

    #[tokio::test]
    async fn test_extract_references_with_enclosing_scope() {
        let parser = TreeSitterParser::new();
        let content = r#"
fn helper() -> i32 {
    42
}

fn caller() {
    let x = helper();
}
"#;

        let references = parser
            .extract_references(
                content,
                "test.rs",
                Language::Rust,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let helper_call = references
            .iter()
            .find(|r| r.callee_symbol() == "helper")
            .expect("Should find call to helper");

        // The caller should be identified
        assert_eq!(
            helper_call.caller_symbol(),
            Some("caller"),
            "Should identify caller function"
        );
    }

    #[tokio::test]
    async fn test_go_imports_strip_quotes() {
        let parser = TreeSitterParser::new();
        let content = r#"
package main

import (
    "fmt"
    "os"
)

func main() {
    fmt.Println("hello")
}
"#;

        let references = parser
            .extract_references(
                content,
                "main.go",
                Language::Go,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // Check that import paths have quotes stripped
        let fmt_imports: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "fmt" && r.reference_kind() == ReferenceKind::Import)
            .collect();
        assert!(
            !fmt_imports.is_empty(),
            "Should find import of fmt (without quotes)"
        );

        let os_imports: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "os" && r.reference_kind() == ReferenceKind::Import)
            .collect();
        assert!(
            !os_imports.is_empty(),
            "Should find import of os (without quotes)"
        );

        // Verify no imports with quotes
        let quoted_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.reference_kind() == ReferenceKind::Import && r.callee_symbol().starts_with('"')
            })
            .collect();
        assert!(
            quoted_imports.is_empty(),
            "Should not have imports with surrounding quotes"
        );
    }

    #[tokio::test]
    async fn test_go_no_duplicate_package_calls() {
        let parser = TreeSitterParser::new();
        let content = r#"
package main

import "fmt"

func main() {
    fmt.Println("hello")
}
"#;

        let references = parser
            .extract_references(
                content,
                "main.go",
                Language::Go,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // Should find exactly one call to Println (not duplicated by method_call pattern)
        let println_calls: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "Println" && r.reference_kind() == ReferenceKind::Call)
            .collect();
        assert_eq!(
            println_calls.len(),
            1,
            "Should find exactly one call to Println (no duplicates)"
        );
    }

    #[tokio::test]
    async fn test_cpp_includes_strip_quotes_and_brackets() {
        let parser = TreeSitterParser::new();
        let content = r#"
#include <iostream>
#include <vector>
#include "myheader.h"

int main() {
    return 0;
}
"#;

        let references = parser
            .extract_references(
                content,
                "main.cpp",
                Language::Cpp,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // Check system includes have angle brackets stripped
        let iostream_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "iostream" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !iostream_imports.is_empty(),
            "Should find import of iostream (without angle brackets)"
        );

        let vector_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "vector" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !vector_imports.is_empty(),
            "Should find import of vector (without angle brackets)"
        );

        // Check string includes have quotes stripped
        let myheader_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "myheader.h" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !myheader_imports.is_empty(),
            "Should find import of myheader.h (without quotes)"
        );

        // Verify no imports with quotes or brackets
        let raw_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.reference_kind() == ReferenceKind::Import
                    && (r.callee_symbol().starts_with('"')
                        || r.callee_symbol().starts_with('<')
                        || r.callee_symbol().ends_with('"')
                        || r.callee_symbol().ends_with('>'))
            })
            .collect();
        assert!(
            raw_imports.is_empty(),
            "Should not have imports with surrounding delimiters"
        );
    }

    #[tokio::test]
    async fn test_parse_swift_class_and_struct() {
        let parser = TreeSitterParser::new();
        let content = std::fs::read_to_string("tests/fixtures/sample_swift.swift")
            .expect("failed to read sample_swift.swift");

        let chunks = parser
            .parse_file(&content, "sample_swift.swift", Language::Swift, "test-repo")
            .await
            .unwrap();

        assert!(!chunks.is_empty(), "Should extract chunks from Swift file");

        let has_circle = chunks.iter().any(|c| c.symbol_name() == Some("Circle"));
        assert!(has_circle, "Should find Circle class");

        let has_rect = chunks.iter().any(|c| c.symbol_name() == Some("Rectangle"));
        assert!(has_rect, "Should find Rectangle struct");

        let has_shape = chunks.iter().any(|c| c.symbol_name() == Some("Shape"));
        assert!(has_shape, "Should find Shape protocol");
    }

    #[tokio::test]
    async fn test_parse_swift_functions_and_enums() {
        let parser = TreeSitterParser::new();
        let content = std::fs::read_to_string("tests/fixtures/sample_swift.swift")
            .expect("failed to read sample_swift.swift");

        let chunks = parser
            .parse_file(&content, "sample_swift.swift", Language::Swift, "test-repo")
            .await
            .unwrap();

        let has_fn = chunks
            .iter()
            .any(|c| c.symbol_name() == Some("printShapeInfo"));
        assert!(has_fn, "Should find printShapeInfo function");

        let has_enum = chunks.iter().any(|c| c.symbol_name() == Some("Result"));
        assert!(has_enum, "Should find Result enum");
    }

    #[tokio::test]
    async fn test_parse_kotlin_classes_and_functions() {
        let parser = TreeSitterParser::new();
        let content = std::fs::read_to_string("tests/fixtures/sample_kotlin.kt")
            .expect("failed to read sample_kotlin.kt");

        let chunks = parser
            .parse_file(&content, "sample_kotlin.kt", Language::Kotlin, "test-repo")
            .await
            .unwrap();

        assert!(!chunks.is_empty(), "Should extract chunks from Kotlin file");

        let has_circle = chunks.iter().any(|c| c.symbol_name() == Some("Circle"));
        assert!(has_circle, "Should find Circle class");

        let has_rect = chunks.iter().any(|c| c.symbol_name() == Some("Rectangle"));
        assert!(has_rect, "Should find Rectangle data class");

        let has_shape = chunks.iter().any(|c| c.symbol_name() == Some("Shape"));
        assert!(has_shape, "Should find Shape interface");
    }

    #[tokio::test]
    async fn test_parse_kotlin_object_and_enum() {
        let parser = TreeSitterParser::new();
        let content = std::fs::read_to_string("tests/fixtures/sample_kotlin.kt")
            .expect("failed to read sample_kotlin.kt");

        let chunks = parser
            .parse_file(&content, "sample_kotlin.kt", Language::Kotlin, "test-repo")
            .await
            .unwrap();

        let has_math_utils = chunks.iter().any(|c| c.symbol_name() == Some("MathUtils"));
        assert!(has_math_utils, "Should find MathUtils singleton object");

        let has_color = chunks.iter().any(|c| c.symbol_name() == Some("Color"));
        assert!(has_color, "Should find Color enum class");

        let has_print_fn = chunks
            .iter()
            .any(|c| c.symbol_name() == Some("printShapeInfo"));
        assert!(
            has_print_fn,
            "Should find printShapeInfo top-level function"
        );
    }

    #[tokio::test]
    async fn test_parse_kotlin_type_alias() {
        let parser = TreeSitterParser::new();
        let content = std::fs::read_to_string("tests/fixtures/sample_kotlin.kt")
            .expect("failed to read sample_kotlin.kt");

        let chunks = parser
            .parse_file(&content, "sample_kotlin.kt", Language::Kotlin, "test-repo")
            .await
            .unwrap();

        let typedef_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.node_type() == NodeType::TypeDef)
            .collect();
        assert!(
            !typedef_chunks.is_empty(),
            "Should find at least one type alias"
        );

        let has_shape_list = chunks.iter().any(|c| c.symbol_name() == Some("ShapeList"));
        assert!(has_shape_list, "Should find ShapeList type alias");
    }

    #[tokio::test]
    async fn test_extract_kotlin_imports_and_calls() {
        let parser = TreeSitterParser::new();
        let content = r#"
package com.example

import kotlin.math.sqrt
import java.util.ArrayList

fun hypotenuse(a: Double, b: Double): Double {
    return sqrt(a * a + b * b)
}

fun buildList(): ArrayList<String> {
    val list = ArrayList<String>()
    list.add("hello")
    return list
}
"#;

        let references = parser
            .extract_references(
                content,
                "test.kt",
                Language::Kotlin,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        assert!(
            !references.is_empty(),
            "Should extract references from Kotlin"
        );

        let sqrt_imports: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "sqrt" && r.reference_kind() == ReferenceKind::Import)
            .collect();
        assert!(!sqrt_imports.is_empty(), "Should find import of sqrt");

        let sqrt_calls: Vec<_> = references
            .iter()
            .filter(|r| r.callee_symbol() == "sqrt" && r.reference_kind() == ReferenceKind::Call)
            .collect();
        assert!(!sqrt_calls.is_empty(), "Should find call to sqrt");
    }

    #[tokio::test]
    async fn test_extract_kotlin_inheritance() {
        let parser = TreeSitterParser::new();
        let content = r#"
interface Animal {
    fun speak(): String
}

class Dog : Animal {
    override fun speak(): String = "Woof"
}

open class Vehicle(val speed: Int)

class Car(speed: Int) : Vehicle(speed) {
    fun drive() {
        println("Driving at $speed km/h")
    }
}
"#;

        let references = parser
            .extract_references(
                content,
                "test.kt",
                Language::Kotlin,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let animal_refs: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "Animal" && r.reference_kind() == ReferenceKind::Inheritance
            })
            .collect();
        assert!(
            !animal_refs.is_empty(),
            "Should find Animal as an inheritance target"
        );

        let vehicle_refs: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "Vehicle" && r.reference_kind() == ReferenceKind::Inheritance
            })
            .collect();
        assert!(
            !vehicle_refs.is_empty(),
            "Should find Vehicle as an inheritance target"
        );
    }

    #[tokio::test]
    async fn test_extract_kotlin_method_calls() {
        let parser = TreeSitterParser::new();
        let content = r#"
fun example() {
    val list = mutableListOf<String>()
    list.add("hello")
    list.add("world")
    println(list.size)
}
"#;

        let references = parser
            .extract_references(
                content,
                "test.kt",
                Language::Kotlin,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let add_calls: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "add" && r.reference_kind() == ReferenceKind::MethodCall
            })
            .collect();
        assert!(!add_calls.is_empty(), "Should find method calls to add");
    }

    #[tokio::test]
    async fn test_extract_swift_imports_and_calls() {
        let parser = TreeSitterParser::new();
        let content = r#"
import Foundation
import UIKit

func greet(name: String) -> String {
    return "Hello, \(name)!"
}

let message = greet(name: "World")
print(message)
"#;

        let references = parser
            .extract_references(
                content,
                "test.swift",
                Language::Swift,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        assert!(
            !references.is_empty(),
            "Should extract references from Swift"
        );

        let foundation_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "Foundation" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !foundation_imports.is_empty(),
            "Should find import of Foundation"
        );
    }

    #[tokio::test]
    async fn test_extract_js_commonjs_require_import() {
        let parser = TreeSitterParser::new();
        // Simulates a Node.js router file that require()s a middleware module.
        let content = r#"
const express = require('express');
const addSource = require('../middlewares/add-application-source.js');

const router = express.Router();

function setupRoutes(app) {
    router.use(addSource);
    app.use('/api', router);
}

module.exports = setupRoutes;
"#;

        let references = parser
            .extract_references(
                content,
                "routes/na-api-router.js",
                Language::JavaScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // require('express') should be captured as an Import with callee "express"
        let express_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "express" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !express_imports.is_empty(),
            "Should capture `const express = require('express')` as an Import reference"
        );

        // require('../middlewares/add-application-source.js') should be captured
        // as an Import with callee equal to the local binding name "addSource".
        let add_source_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "addSource" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !add_source_imports.is_empty(),
            "Should capture `const addSource = require(...)` as an Import reference with callee 'addSource'"
        );

        // `router.use(addSource)` passes addSource as an argument — it is not a
        // direct function call and should NOT produce a separate Call reference for "addSource".
        // (The `use` method call is captured separately.)
        let use_method_calls: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "use" && r.reference_kind() == ReferenceKind::MethodCall
            })
            .collect();
        assert!(
            !use_method_calls.is_empty(),
            "Should capture router.use(...) as a MethodCall with callee 'use'"
        );
    }

    #[tokio::test]
    async fn test_extract_js_require_does_not_capture_non_require_calls() {
        let parser = TreeSitterParser::new();
        // A pattern like `const x = factory("path")` should NOT be treated as an import.
        let content = r#"
const handler = createHandler('config');

function setupRoutes() {
    handler.listen(3000);
}
"#;

        let references = parser
            .extract_references(
                content,
                "server.js",
                Language::JavaScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // createHandler is called, not require — should NOT appear as an Import.
        let handler_imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "handler" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            handler_imports.is_empty(),
            "`const handler = createHandler(...)` must NOT be captured as an Import (only require() counts)"
        );
    }

    // ── Renamed-import tests ────────────────────────────────────────────────

    /// ES6 named import with alias: `import { foo as bar } from './module'`
    /// The callee should be the original exported name ("foo") and the local
    /// alias ("bar") must be recorded in import_alias.
    #[tokio::test]
    async fn test_extract_es6_named_import_with_alias() {
        let parser = TreeSitterParser::new();
        let content = r#"
import { processRequest as handleReq } from './request-handler';

function main() {
    handleReq({ method: 'GET' });
}
"#;

        let references = parser
            .extract_references(
                content,
                "main.ts",
                Language::TypeScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // The import statement should record the original exported name as callee.
        let import_refs: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "processRequest" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !import_refs.is_empty(),
            "Expected Import reference with callee 'processRequest' \
             from `import {{ processRequest as handleReq }}`"
        );

        // The import_alias must be the local binding name ("handleReq").
        let alias = import_refs[0].import_alias();
        assert_eq!(
            alias,
            Some("handleReq"),
            "Expected import_alias = 'handleReq' for the renamed import, got {:?}",
            alias
        );
    }

    /// ES6 named import WITHOUT alias: `import { foo } from './module'`
    /// The callee is "foo" and import_alias must be None.
    #[tokio::test]
    async fn test_extract_es6_named_import_no_alias() {
        let parser = TreeSitterParser::new();
        let content = r#"
import { processRequest } from './request-handler';

function main() {
    processRequest({ method: 'GET' });
}
"#;

        let references = parser
            .extract_references(
                content,
                "main.ts",
                Language::TypeScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let import_refs: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "processRequest" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !import_refs.is_empty(),
            "Should find import of 'processRequest'"
        );
        assert_eq!(
            import_refs[0].import_alias(),
            None,
            "Non-aliased import must have import_alias = None"
        );
    }

    /// CommonJS shorthand destructure: `const { foo } = require('./module')`
    /// The property name "foo" is both the original and local name; import_alias is None.
    #[tokio::test]
    async fn test_extract_js_commonjs_shorthand_destructure() {
        let parser = TreeSitterParser::new();
        let content = r#"
const { createServer } = require('http');

createServer((req, res) => { res.end('ok'); }).listen(3000);
"#;

        let references = parser
            .extract_references(
                content,
                "server.js",
                Language::JavaScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let import_refs: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "createServer" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !import_refs.is_empty(),
            "Expected Import reference with callee 'createServer' \
             from `const {{ createServer }} = require('http')`"
        );
        assert_eq!(
            import_refs[0].import_alias(),
            None,
            "Shorthand destructure (no rename) must have import_alias = None"
        );
    }

    /// CommonJS renamed destructure: `const { foo: bar } = require('./module')`
    /// The callee is the original property name ("foo") and import_alias is the
    /// local binding ("bar").
    #[tokio::test]
    async fn test_extract_js_commonjs_renamed_destructure() {
        let parser = TreeSitterParser::new();
        let content = r#"
const { createServer: makeServer } = require('http');

makeServer((req, res) => { res.end('ok'); }).listen(3000);
"#;

        let references = parser
            .extract_references(
                content,
                "server.js",
                Language::JavaScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let import_refs: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "createServer" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !import_refs.is_empty(),
            "Expected Import reference with callee 'createServer' \
             from `const {{ createServer: makeServer }} = require('http')`"
        );

        let alias = import_refs[0].import_alias();
        assert_eq!(
            alias,
            Some("makeServer"),
            "Expected import_alias = 'makeServer' for renamed destructure, got {:?}",
            alias
        );
    }

    /// TypeScript renamed destructure: `const { foo: bar } = require('./module')`
    #[tokio::test]
    async fn test_extract_ts_commonjs_renamed_destructure() {
        let parser = TreeSitterParser::new();
        let content = r#"
const { Router: ExpressRouter } = require('express');

const router = new ExpressRouter();
"#;

        let references = parser
            .extract_references(
                content,
                "router.ts",
                Language::TypeScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let import_refs: Vec<_> = references
            .iter()
            .filter(|r| {
                r.callee_symbol() == "Router" && r.reference_kind() == ReferenceKind::Import
            })
            .collect();
        assert!(
            !import_refs.is_empty(),
            "Expected Import reference with callee 'Router' \
             from `const {{ Router: ExpressRouter }} = require('express')`"
        );

        let alias = import_refs[0].import_alias();
        assert_eq!(
            alias,
            Some("ExpressRouter"),
            "Expected import_alias = 'ExpressRouter', got {:?}",
            alias
        );
    }

    /// CommonJS renamed destructure must still be rejected when the call is not `require()`.
    #[tokio::test]
    async fn test_extract_js_destructure_non_require_not_captured() {
        let parser = TreeSitterParser::new();
        let content = r#"
const { createServer: makeServer } = someOtherFactory('config');
"#;

        let references = parser
            .extract_references(
                content,
                "server.js",
                Language::JavaScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let import_refs: Vec<_> = references
            .iter()
            .filter(|r| r.reference_kind() == ReferenceKind::Import)
            .collect();
        assert!(
            import_refs.is_empty(),
            "Destructured non-require() call must NOT be captured as Import"
        );
    }

    // ── Export extraction tests ─────────────────────────────────────────────

    /// `module.exports = identifier` — single default export.
    #[tokio::test]
    async fn test_extract_module_exports_default() {
        let parser = TreeSitterParser::new();
        let content = r#"
function appApplicationSource(req, res, next) { next(); }
module.exports = appApplicationSource;
"#;
        let exports = parser
            .extract_module_exports(content, Language::JavaScript)
            .await;
        assert_eq!(
            exports,
            vec!["appApplicationSource"],
            "Expected single default export"
        );
    }

    /// `module.exports.key = value` — named property export.
    #[tokio::test]
    async fn test_extract_module_exports_named_property() {
        let parser = TreeSitterParser::new();
        let content = r#"
function helper() {}
module.exports.helper = helper;
module.exports.other = function() {};
"#;
        let exports = parser
            .extract_module_exports(content, Language::JavaScript)
            .await;
        assert!(
            exports.contains(&"helper".to_string()),
            "Expected 'helper' in named property exports"
        );
    }

    /// ES6 `export default identifier` and `export function foo`.
    #[tokio::test]
    async fn test_extract_module_exports_es6() {
        let parser = TreeSitterParser::new();
        let content = r#"
export default myFunc;
export function helperFn() {}
export class MyClass {}
"#;
        let exports = parser
            .extract_module_exports(content, Language::TypeScript)
            .await;
        assert!(exports.contains(&"myFunc".to_string()), "Expected 'myFunc'");
        assert!(
            exports.contains(&"helperFn".to_string()),
            "Expected 'helperFn'"
        );
        assert!(
            exports.contains(&"MyClass".to_string()),
            "Expected 'MyClass'"
        );
    }

    /// `module.exports = function namedFn(...)` — inline named function expression.
    /// This is a very common middleware pattern and must be captured.
    #[tokio::test]
    async fn test_extract_module_exports_inline_named_function() {
        let parser = TreeSitterParser::new();
        let content = r#"
module.exports = function appApplicationSource(req, res, next) {
  next();
};
"#;
        let exports = parser
            .extract_module_exports(content, Language::JavaScript)
            .await;
        assert_eq!(
            exports,
            vec!["appApplicationSource"],
            "Inline named function export must be captured"
        );
    }

    /// Non-JS language must return empty.
    #[tokio::test]
    async fn test_extract_module_exports_unsupported_language() {
        let parser = TreeSitterParser::new();
        let content = "fn main() {}";
        let exports = parser.extract_module_exports(content, Language::Rust).await;
        assert!(
            exports.is_empty(),
            "Rust files should have no module exports"
        );
    }

    // ── require() without an exports map ────────────────────────────────────

    /// Without an exports map, `const addSource = require('./sample_middleware.js')`
    /// must keep `addSource` as the callee symbol (no cross-file resolution).
    #[tokio::test]
    async fn test_require_without_exports_map_keeps_local_binding() {
        let parser = TreeSitterParser::new();
        let content = r#"
const addSource = require('./sample_middleware.js');
"#;

        let references = parser
            .extract_references(
                content,
                "router.js",
                Language::JavaScript,
                "test-repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let import_ref = references
            .iter()
            .find(|r| r.reference_kind() == ReferenceKind::Import)
            .expect("Expected Import reference");

        assert_eq!(
            import_ref.callee_symbol(),
            "addSource",
            "Without an exports map the local binding name must be preserved"
        );
        assert!(
            import_ref.import_alias().is_none(),
            "No alias should be set when no exports map is provided"
        );
    }

    // ── Cross-file require resolution ───────────────────────────────────────

    /// End-to-end: `const addSource = require('./sample_middleware.js')` where
    /// `sample_middleware.js` has `module.exports = appApplicationSource`.
    ///
    /// After resolution, `callee_symbol` must be `appApplicationSource` and
    /// `import_alias` must be `addSource`.
    #[tokio::test]
    async fn test_resolve_simple_require_to_exported_symbol() {
        let parser = TreeSitterParser::new();

        let router_content = r#"
const addSource = require('./sample_middleware.js');
"#;

        // Build the exports map as the indexer would: pre-scan sample_middleware.js.
        let middleware_content = r#"
function appApplicationSource(req, res, next) { next(); }
module.exports = appApplicationSource;
"#;
        let mut exports_by_file: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let middleware_exports = parser
            .extract_module_exports(middleware_content, Language::JavaScript)
            .await;
        exports_by_file.insert("sample_middleware.js".to_string(), middleware_exports);

        let references = parser
            .extract_references(
                router_content,
                "router.js",
                Language::JavaScript,
                "test-repo",
                &exports_by_file,
            )
            .await
            .unwrap();

        let import_ref = references
            .iter()
            .find(|r| r.reference_kind() == ReferenceKind::Import)
            .expect("Expected an Import reference");

        assert_eq!(
            import_ref.callee_symbol(),
            "appApplicationSource",
            "callee_symbol must be the resolved exported name"
        );
        assert_eq!(
            import_ref.import_alias(),
            Some("addSource"),
            "import_alias must be the local binding name"
        );
    }

    /// When the required file exports multiple symbols, we cannot resolve
    /// unambiguously — the local binding name must be kept as callee_symbol.
    #[tokio::test]
    async fn test_require_multi_export_not_resolved() {
        let parser = TreeSitterParser::new();

        let content = r#"const utils = require('./utils.js');"#;

        let mut exports_by_file: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        exports_by_file.insert(
            "utils.js".to_string(),
            vec!["helperA".to_string(), "helperB".to_string()],
        );

        let references = parser
            .extract_references(
                content,
                "main.js",
                Language::JavaScript,
                "test-repo",
                &exports_by_file,
            )
            .await
            .unwrap();

        let import_ref = references
            .iter()
            .find(|r| r.reference_kind() == ReferenceKind::Import)
            .expect("Expected an Import reference");

        assert_eq!(
            import_ref.callee_symbol(),
            "utils",
            "With multiple exports the local binding name must be kept"
        );
        assert!(
            import_ref.import_alias().is_none(),
            "No alias should be set when resolution is ambiguous"
        );
    }

    /// External package requires (`require('express')`) must not be modified.
    #[tokio::test]
    async fn test_require_external_package_not_resolved() {
        let parser = TreeSitterParser::new();

        let content = r#"const express = require('express');"#;

        let references = parser
            .extract_references(
                content,
                "app.js",
                Language::JavaScript,
                "test-repo",
                &std::collections::HashMap::new(),
            )
            .await
            .unwrap();

        let import_ref = references
            .iter()
            .find(|r| r.reference_kind() == ReferenceKind::Import)
            .expect("Expected an Import reference");

        assert_eq!(
            import_ref.callee_symbol(),
            "express",
            "External package bindings must keep the local name"
        );
        assert!(import_ref.import_alias().is_none());
    }

    // ── qualify_callee_name tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_php_static_call_qualified_with_class() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class Router {
    public function dispatch() {
        return AssociateAdminDevice::post($request);
    }
}
"#;
        let refs = parser
            .extract_references(
                content,
                "router.php",
                Language::Php,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let post_call = refs
            .iter()
            .find(|r| r.callee_symbol().ends_with("::post"))
            .expect("expected a qualified ::post call");
        assert_eq!(post_call.callee_symbol(), "AssociateAdminDevice::post");
        assert_eq!(post_call.reference_kind(), ReferenceKind::MethodCall);
    }

    #[tokio::test]
    async fn test_php_this_call_qualified_with_enclosing_class() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class AssociateAdminDevice {
    public function post($req) {
        $user = $this->getNewUser($req);
        return $user;
    }
}
"#;
        let refs = parser
            .extract_references(
                content,
                "device.php",
                Language::Php,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let get_user = refs
            .iter()
            .find(|r| r.callee_symbol().ends_with("::getNewUser"))
            .expect("expected a qualified ::getNewUser call");
        assert_eq!(get_user.callee_symbol(), "AssociateAdminDevice::getNewUser");
        assert_eq!(get_user.reference_kind(), ReferenceKind::MethodCall);
    }

    #[tokio::test]
    async fn test_php_self_call_qualified_with_enclosing_class() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class Home {
    public static function create() {
        return self::build();
    }
}
"#;
        let refs = parser
            .extract_references(content, "home.php", Language::Php, "repo", &HashMap::new())
            .await
            .unwrap();

        let build_call = refs
            .iter()
            .find(|r| r.callee_symbol().ends_with("::build"))
            .expect("expected a qualified ::build call");
        assert_eq!(build_call.callee_symbol(), "Home::build");
    }

    #[tokio::test]
    async fn test_php_unknown_receiver_stays_bare() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class Controller {
    public function handle($client) {
        return $client->post('/api');
    }
}
"#;
        let refs = parser
            .extract_references(content, "ctrl.php", Language::Php, "repo", &HashMap::new())
            .await
            .unwrap();

        // $client->post() — receiver is unknown; callee must remain bare "post"
        let post_call = refs
            .iter()
            .find(|r| {
                r.callee_symbol() == "post" && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected a bare post method_call");
        assert_eq!(post_call.callee_symbol(), "post");
    }

    #[tokio::test]
    async fn test_python_self_call_qualified_with_enclosing_class() {
        let parser = TreeSitterParser::new();
        let content = r#"
class Home:
    def get_new_user(self):
        return self.advertise_users()
"#;
        let refs = parser
            .extract_references(
                content,
                "home.py",
                Language::Python,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let advertise = refs
            .iter()
            .find(|r| r.callee_symbol().ends_with("::advertise_users"))
            .expect("expected a qualified ::advertise_users call");
        assert_eq!(advertise.callee_symbol(), "Home::advertise_users");
    }

    #[tokio::test]
    async fn test_js_this_call_qualified_with_enclosing_class() {
        let parser = TreeSitterParser::new();
        let content = r#"
class EventBus {
    publish(event) {
        this.notify(event);
    }
}
"#;
        let refs = parser
            .extract_references(
                content,
                "bus.js",
                Language::JavaScript,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let notify = refs
            .iter()
            .find(|r| r.callee_symbol().ends_with(".notify"))
            .expect("expected a qualified .notify call");
        assert_eq!(notify.callee_symbol(), "EventBus.notify");
    }

    #[tokio::test]
    async fn test_php_parent_call_resolved_to_base_class() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class Child extends Base {
    public function foo() {
        return parent::bar();
    }
}
"#;
        let refs = parser
            .extract_references(content, "child.php", Language::Php, "repo", &HashMap::new())
            .await
            .unwrap();

        let bar_call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("bar") && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected a bar method call");
        // parent:: should resolve to the actual parent class "Base"
        assert_eq!(bar_call.callee_symbol(), "Base::bar");
    }

    #[tokio::test]
    async fn test_php_parent_call_without_extends_stays_bare() {
        let parser = TreeSitterParser::new();
        // A trait can use parent:: but has no extends clause — the call should
        // fall back to the bare method name rather than emitting "parent::bar".
        let content = r#"<?php
trait MyTrait {
    public function foo() {
        return parent::bar();
    }
}
"#;
        let refs = parser
            .extract_references(content, "trait.php", Language::Php, "repo", &HashMap::new())
            .await
            .unwrap();

        let bar_call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("bar") && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected a bar method call");
        // No extends clause → bare name
        assert_eq!(bar_call.callee_symbol(), "bar");
    }

    #[tokio::test]
    async fn test_js_class_static_call_qualified_with_class_name() {
        let parser = TreeSitterParser::new();
        let content = r#"
class Router {
    static handle(req) {
        return req;
    }
}

function dispatch() {
    Router.handle(request);
}
"#;
        let refs = parser
            .extract_references(
                content,
                "app.js",
                Language::JavaScript,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let handle_call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("handle")
                    && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected a handle method call");
        assert_eq!(handle_call.callee_symbol(), "Router.handle");
    }

    #[tokio::test]
    async fn test_ts_class_static_call_qualified_with_class_name() {
        let parser = TreeSitterParser::new();
        let content = r#"
class Logger {
    static info(msg: string): void {
        console.log(msg);
    }
}

function main() {
    Logger.info("hello");
}
"#;
        let refs = parser
            .extract_references(
                content,
                "main.ts",
                Language::TypeScript,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let info_call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("info")
                    && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected an info method call");
        assert_eq!(info_call.callee_symbol(), "Logger.info");
    }

    #[tokio::test]
    async fn test_js_unknown_receiver_stays_bare() {
        let parser = TreeSitterParser::new();
        let content = r#"
class Router {
    static handle(req) { return req; }
}

function dispatch(client) {
    client.send(data);
}
"#;
        let refs = parser
            .extract_references(
                content,
                "app.js",
                Language::JavaScript,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        // client.send() — "client" is not a class defined in this file
        let send_call = refs
            .iter()
            .find(|r| {
                r.callee_symbol() == "send" && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected a bare send method call");
        assert_eq!(send_call.callee_symbol(), "send");
    }

    #[tokio::test]
    async fn test_php_typed_param_qualifies_method_call() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class Service {
    public function process(Home $home) {
        return $home->getNewUser();
    }
}
"#;
        let refs = parser
            .extract_references(
                content,
                "service.php",
                Language::Php,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("getNewUser")
                    && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected a getNewUser method call");
        // $home has type Home from parameter hint → "Home::getNewUser"
        assert_eq!(call.callee_symbol(), "Home::getNewUser");
    }

    #[tokio::test]
    async fn test_php_new_assignment_qualifies_method_call() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
function setup() {
    $home = new Home();
    $home->advertiseUsers();
}
"#;
        let refs = parser
            .extract_references(content, "setup.php", Language::Php, "repo", &HashMap::new())
            .await
            .unwrap();

        let call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("advertiseUsers")
                    && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected an advertiseUsers method call");
        assert_eq!(call.callee_symbol(), "Home::advertiseUsers");
    }

    #[tokio::test]
    async fn test_php_property_type_qualifies_method_call() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class Controller {
    private Home $home;

    public function handle() {
        return $this->home->getNewUser();
    }
}
"#;
        let refs = parser
            .extract_references(content, "ctrl.php", Language::Php, "repo", &HashMap::new())
            .await
            .unwrap();

        // $this->home is a property access yielding a Home — but the
        // $this->home chain is a member_access_expression, not a simple
        // variable_name. The type inference only resolves simple variables.
        // This test documents that chained property access is NOT resolved.
        let call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("getNewUser")
                    && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected a getNewUser method call");
        // Chained access through $this->home is not resolved — stays bare
        assert_eq!(call.callee_symbol(), "getNewUser");
    }

    #[tokio::test]
    async fn test_php_nullable_typed_param_qualifies_method_call() {
        let parser = TreeSitterParser::new();
        let content = r#"<?php
class Handler {
    public function run(?Logger $logger) {
        $logger->info("running");
    }
}
"#;
        let refs = parser
            .extract_references(
                content,
                "handler.php",
                Language::Php,
                "repo",
                &HashMap::new(),
            )
            .await
            .unwrap();

        let call = refs
            .iter()
            .find(|r| {
                r.callee_symbol().contains("info")
                    && r.reference_kind() == ReferenceKind::MethodCall
            })
            .expect("expected an info method call");
        // ?Logger → Logger
        assert_eq!(call.callee_symbol(), "Logger::info");
    }
}
