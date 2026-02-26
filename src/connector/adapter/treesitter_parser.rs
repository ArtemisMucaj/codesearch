use async_trait::async_trait;
use streaming_iterator::StreamingIterator;
use tracing::debug;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::application::ParserService;
use crate::domain::{CodeChunk, DomainError, Language, NodeType};

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
}
