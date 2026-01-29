use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Language;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeChunk {
    id: String,
    file_path: String,
    content: String,
    start_line: u32,
    end_line: u32,
    language: Language,
    node_type: NodeType,
    symbol_name: Option<String>,
    parent_symbol: Option<String>,
    repository_id: String,
}

impl CodeChunk {
    pub fn new(
        file_path: String,
        content: String,
        start_line: u32,
        end_line: u32,
        language: Language,
        node_type: NodeType,
        repository_id: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            file_path,
            content,
            start_line,
            end_line,
            language,
            node_type,
            symbol_name: None,
            parent_symbol: None,
            repository_id,
        }
    }

    /// Reconstitutes from persisted data (used by adapters).
    #[allow(clippy::too_many_arguments)]
    pub fn reconstitute(
        id: String,
        file_path: String,
        content: String,
        start_line: u32,
        end_line: u32,
        language: Language,
        node_type: NodeType,
        symbol_name: Option<String>,
        parent_symbol: Option<String>,
        repository_id: String,
    ) -> Self {
        Self {
            id,
            file_path,
            content,
            start_line,
            end_line,
            language,
            node_type,
            symbol_name,
            parent_symbol,
            repository_id,
        }
    }

    pub fn with_symbol_name(mut self, name: impl Into<String>) -> Self {
        self.symbol_name = Some(name.into());
        self
    }

    pub fn with_parent_symbol(mut self, parent: impl Into<String>) -> Self {
        self.parent_symbol = Some(parent.into());
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn start_line(&self) -> u32 {
        self.start_line
    }

    pub fn end_line(&self) -> u32 {
        self.end_line
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn node_type(&self) -> NodeType {
        self.node_type
    }

    pub fn symbol_name(&self) -> Option<&str> {
        self.symbol_name.as_deref()
    }

    pub fn parent_symbol(&self) -> Option<&str> {
        self.parent_symbol.as_deref()
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn location(&self) -> String {
        format!("{}:{}-{}", self.file_path, self.start_line, self.end_line)
    }

    /// Returns the number of lines in this chunk.
    pub fn line_count(&self) -> u32 {
        self.end_line.saturating_sub(self.start_line) + 1
    }

    pub fn is_callable(&self) -> bool {
        matches!(self.node_type, NodeType::Function)
    }

    pub fn is_type_definition(&self) -> bool {
        matches!(
            self.node_type,
            NodeType::Class
                | NodeType::Struct
                | NodeType::Enum
                | NodeType::Interface
                | NodeType::TypeDef
        )
    }

    pub fn preview(&self, max_lines: usize) -> String {
        self.content
            .lines()
            .take(max_lines)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn belongs_to_repository(&self, repository_id: &str) -> bool {
        self.repository_id == repository_id
    }

    pub fn qualified_name(&self) -> Option<String> {
        match (&self.parent_symbol, &self.symbol_name) {
            (Some(parent), Some(name)) => Some(format!("{}::{}", parent, name)),
            (None, Some(name)) => Some(name.clone()),
            _ => None,
        }
    }
}

/// Represents the type of code construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Function,
    Class,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Constant,
    TypeDef,
    Interface,
    Block,
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::Function => "function",
            NodeType::Class => "class",
            NodeType::Struct => "struct",
            NodeType::Enum => "enum",
            NodeType::Trait => "trait",
            NodeType::Impl => "impl",
            NodeType::Module => "module",
            NodeType::Constant => "constant",
            NodeType::TypeDef => "typedef",
            NodeType::Interface => "interface",
            NodeType::Block => "block",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
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

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_chunk_creation() {
        let chunk = CodeChunk::new(
            "src/lib.rs".to_string(),
            "fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
            10,
            12,
            Language::Rust,
            NodeType::Function,
            "repo-123".to_string(),
        )
        .with_symbol_name("add");

        assert_eq!(chunk.file_path(), "src/lib.rs");
        assert_eq!(chunk.symbol_name(), Some("add"));
        assert_eq!(chunk.line_count(), 3);
        assert!(chunk.is_callable());
        assert!(!chunk.is_type_definition());
    }

    #[test]
    fn test_qualified_name() {
        let chunk = CodeChunk::new(
            "src/lib.rs".to_string(),
            "fn method() {}".to_string(),
            1,
            1,
            Language::Rust,
            NodeType::Function,
            "repo".to_string(),
        )
        .with_symbol_name("method")
        .with_parent_symbol("MyStruct");

        assert_eq!(chunk.qualified_name(), Some("MyStruct::method".to_string()));
    }

    #[test]
    fn test_location_format() {
        let chunk = CodeChunk::new(
            "test.rs".to_string(),
            "code".to_string(),
            5,
            10,
            Language::Rust,
            NodeType::Function,
            "repo".to_string(),
        );

        assert_eq!(chunk.location(), "test.rs:5-10");
    }
}
