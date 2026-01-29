use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Language;

/// Represents a chunk of code extracted from a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeChunk {
    pub id: String,
    pub file_path: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub language: Language,
    pub node_type: NodeType,
    pub symbol_name: Option<String>,
    pub parent_symbol: Option<String>,
    pub repository_id: String,
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

    pub fn with_symbol_name(mut self, name: impl Into<String>) -> Self {
        self.symbol_name = Some(name.into());
        self
    }

    pub fn with_parent_symbol(mut self, parent: impl Into<String>) -> Self {
        self.parent_symbol = Some(parent.into());
        self
    }

    pub fn location(&self) -> String {
        format!("{}:{}-{}", self.file_path, self.start_line, self.end_line)
    }
}

/// Types of AST nodes that can be indexed.
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
}

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
