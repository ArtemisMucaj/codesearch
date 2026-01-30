use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    HCL,
    Php,
    Unknown,
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Language::Rust,
            "py" => Language::Python,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "tsx" => Language::TypeScript,
            "go" => Language::Go,
            "hcl" | "tf" => Language::HCL,
            "php" => Language::Php,
            _ => Language::Unknown,
        }
    }

    pub fn from_path(path: &Path) -> Self {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(Self::from_extension)
            .unwrap_or(Language::Unknown)
    }

    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "rust" => Language::Rust,
            "python" => Language::Python,
            "javascript" => Language::JavaScript,
            "typescript" => Language::TypeScript,
            "go" => Language::Go,
            "hcl" => Language::HCL,
            "php" => Language::Php,
            _ => Language::Unknown,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
            Language::HCL => "hcl",
            Language::Php => "php",
            Language::Unknown => "unknown",
        }
    }

    pub fn is_known(&self) -> bool {
        !matches!(self, Language::Unknown)
    }

    pub fn primary_extension(&self) -> &'static str {
        match self {
            Language::Rust => "rs",
            Language::Python => "py",
            Language::JavaScript => "js",
            Language::TypeScript => "ts",
            Language::Go => "go",
            Language::HCL => "hcl",
            Language::Php => "php",
            Language::Unknown => "",
        }
    }

    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["rs"],
            Language::Python => &["py"],
            Language::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Language::TypeScript => &["ts", "tsx"],
            Language::Go => &["go"],
            Language::HCL => &["hcl", "tf"],
            Language::Php => &["php"],
            Language::Unknown => &[],
        }
    }

    pub fn uses_braces(&self) -> bool {
        matches!(
            self,
            Language::Rust
                | Language::JavaScript
                | Language::TypeScript
                | Language::Go
                | Language::HCL
                | Language::Php
        )
    }

    pub fn is_statically_typed(&self) -> bool {
        matches!(
            self,
            Language::Rust | Language::TypeScript | Language::Go | Language::Php
        )
    }

    pub fn all_supported() -> Vec<Language> {
        vec![
            Language::Rust,
            Language::Python,
            Language::JavaScript,
            Language::TypeScript,
            Language::Go,
            Language::HCL,
            Language::Php,
        ]
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_from_extension() {
        assert_eq!(Language::from_extension("rs"), Language::Rust);
        assert_eq!(Language::from_extension("py"), Language::Python);
        assert_eq!(Language::from_extension("js"), Language::JavaScript);
        assert_eq!(Language::from_extension("ts"), Language::TypeScript);
        assert_eq!(Language::from_extension("go"), Language::Go);
        assert_eq!(Language::from_extension("hcl"), Language::HCL);
        assert_eq!(Language::from_extension("php"), Language::Php);
        assert_eq!(Language::from_extension("txt"), Language::Unknown);
    }

    #[test]
    fn test_language_from_path() {
        assert_eq!(
            Language::from_path(Path::new("src/main.rs")),
            Language::Rust
        );
        assert_eq!(
            Language::from_path(Path::new("script.py")),
            Language::Python
        );
    }

    #[test]
    fn test_language_from_str() {
        assert_eq!(Language::parse("rust"), Language::Rust);
        assert_eq!(Language::parse("PYTHON"), Language::Python);
        assert_eq!(Language::parse("unknown_lang"), Language::Unknown);
    }

    #[test]
    fn test_is_known() {
        assert!(Language::Rust.is_known());
        assert!(Language::Python.is_known());
        assert!(!Language::Unknown.is_known());
    }

    #[test]
    fn test_extensions() {
        assert_eq!(
            Language::JavaScript.extensions(),
            &["js", "jsx", "mjs", "cjs"]
        );
        assert_eq!(Language::Rust.extensions(), &["rs"]);
    }

    #[test]
    fn test_all_supported() {
        let supported = Language::all_supported();
        assert!(supported.contains(&Language::Rust));
        assert!(supported.contains(&Language::Python));
        assert!(supported.contains(&Language::HCL));
        assert!(supported.contains(&Language::Php));
        assert!(!supported.contains(&Language::Unknown));
    }
}
