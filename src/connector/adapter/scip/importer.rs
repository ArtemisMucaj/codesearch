use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use protobuf::Message as ProtobufMessage;
use scip::types::symbol_information::Kind as SymbolKind;
use scip::types::SymbolRole;
use tracing::debug;

use crate::domain::{Language, ReferenceKind, SymbolReference};

// Bit-mask constants derived from the SCIP protobuf SymbolRole enum so that
// they stay in sync with the upstream specification automatically.
const ROLE_DEFINITION: i32 = SymbolRole::Definition as i32;
const ROLE_IMPORT: i32 = SymbolRole::Import as i32;
const ROLE_READ_ACCESS: i32 = SymbolRole::ReadAccess as i32;

/// Parses a `.scip` index file and converts its occurrences into
/// [`SymbolReference`] entries compatible with the existing DuckDB call graph
/// schema.
pub struct ScipImporter;

impl ScipImporter {
    /// Parse `scip_path` and return a map of `file_path → Vec<SymbolReference>`.
    ///
    /// The returned file paths are relative (as stored in the SCIP index).
    /// Each [`SymbolReference`] has `repository_id` set to `repo_id`.
    pub async fn import(
        scip_path: &Path,
        repo_id: &str,
    ) -> Result<HashMap<String, Vec<SymbolReference>>, anyhow::Error> {
        let bytes = tokio::fs::read(scip_path)
            .await
            .with_context(|| format!("failed to read SCIP index at {:?}", scip_path))?;

        // Deserialise on a blocking thread so we don't stall the async runtime.
        let repo_id = repo_id.to_string();
        tokio::task::spawn_blocking(move || {
            let index = scip::types::Index::parse_from_bytes(&bytes)
                .context("failed to parse SCIP protobuf")?;

            let mut by_file: HashMap<String, Vec<SymbolReference>> = HashMap::new();

            for doc in &index.documents {
                let language = scip_language_to_domain(&doc.language, &doc.relative_path);
                if !matches!(
                    language,
                    Language::JavaScript | Language::TypeScript | Language::Php
                ) {
                    continue;
                }

                let refs = process_document(doc, &repo_id, language);
                if !refs.is_empty() {
                    debug!(
                        "SCIP: {} references from {}",
                        refs.len(),
                        doc.relative_path
                    );
                    by_file
                        .entry(doc.relative_path.clone())
                        .or_default()
                        .extend(refs);
                }
            }

            Ok(by_file)
        })
        .await
        .context("SCIP import task panicked")?
    }
}

// ---------------------------------------------------------------------------
// Per-document processing
// ---------------------------------------------------------------------------

/// Convert one SCIP [`Document`] into a flat list of [`SymbolReference`]s.
fn process_document(
    doc: &scip::types::Document,
    repo_id: &str,
    language: Language,
) -> Vec<SymbolReference> {
    // Build symbol-kind lookup: symbol_str → SymbolKind
    let kind_map: HashMap<&str, SymbolKind> = doc
        .symbols
        .iter()
        .map(|si| (si.symbol.as_str(), si.kind.enum_value_or_default()))
        .collect();

    // Collect all Definition occurrences for callable symbols (function/method/constructor).
    // We sort them by line so we can do a backwards scan to find the enclosing scope for
    // each reference occurrence.
    let mut scope_defs: Vec<ScopeDef> = doc
        .occurrences
        .iter()
        .filter(|occ| {
            (occ.symbol_roles & ROLE_DEFINITION) != 0
                && is_callable_kind(kind_map.get(occ.symbol.as_str()).copied())
                && !occ.range.is_empty()
        })
        .map(|occ| ScopeDef {
            line: occ.range[0] as u32,
            symbol: normalize_symbol(&occ.symbol),
            enclosing_scope: extract_enclosing_scope(&occ.symbol),
        })
        .collect();
    scope_defs.sort_by_key(|s| s.line);

    let mut refs = Vec::new();

    for occ in &doc.occurrences {
        // Skip definitions and occurrences without a range.
        if (occ.symbol_roles & ROLE_DEFINITION) != 0 || occ.range.is_empty() {
            continue;
        }

        // Skip local symbols (they are internal to the function and produce noise).
        if occ.symbol.starts_with("local ") {
            continue;
        }

        let callee_symbol = normalize_symbol(&occ.symbol);
        if callee_symbol.is_empty() {
            continue;
        }

        let occ_line = occ.range[0] as u32;
        let occ_col = if occ.range.len() > 1 {
            occ.range[1] as u32
        } else {
            0
        };

        let callee_kind = kind_map.get(occ.symbol.as_str()).copied();
        let reference_kind = infer_reference_kind(occ.symbol_roles, callee_kind);

        // Find the enclosing function/method via backwards scan.
        let enclosing = find_enclosing_scope(&scope_defs, occ_line);

        let mut sym_ref = SymbolReference::new(
            enclosing.as_ref().map(|s| s.symbol.clone()),
            callee_symbol,
            doc.relative_path.clone(),
            doc.relative_path.clone(),
            occ_line + 1, // SCIP is 0-indexed; our model is 1-indexed
            occ_col + 1,
            reference_kind,
            language,
            repo_id.to_string(),
        );

        if let Some(scope) = enclosing {
            if let Some(enc) = scope.enclosing_scope {
                sym_ref = sym_ref.with_enclosing_scope(enc);
            }
        }

        refs.push(sym_ref);
    }

    refs
}

// ---------------------------------------------------------------------------
// Scope resolution helpers
// ---------------------------------------------------------------------------

/// A function/method definition found in the document.
#[derive(Debug, Clone)]
struct ScopeDef {
    /// 0-indexed start line of the identifier.
    line: u32,
    /// Normalised symbol name (e.g. `render`, `MyClass#render`).
    symbol: String,
    /// Enclosing class/namespace if any (e.g. `MyClass`).
    enclosing_scope: Option<String>,
}

/// Returns the innermost enclosing function definition at or before `line`.
///
/// **Heuristic (start-line only):** [`ScopeDef`] records only the start line
/// of each callable definition because SCIP occurrence ranges cover the
/// *identifier token*, not the full function body.  The algorithm therefore
/// picks the candidate with the greatest start line that is still ≤ `line`
/// ("best predecessor").
///
/// **Known limitation:** a reference that appears *after* a function's opening
/// line but *outside* its body (e.g. a module-level statement between two
/// function definitions) will be incorrectly attributed to the preceding
/// function.  Fixing this would require end-line information, which SCIP does
/// not provide for definition occurrences.
fn find_enclosing_scope(scope_defs: &[ScopeDef], line: u32) -> Option<ScopeDef> {
    scope_defs
        .iter()
        .filter(|s| s.line <= line)
        .last()
        .cloned()
}

// ---------------------------------------------------------------------------
// Symbol name normalisation
// ---------------------------------------------------------------------------

/// Convert a full SCIP symbol string into a compact, human-readable name.
///
/// SCIP symbol format: `<scheme> <manager> <pkg-name> <version> <descriptor>+`
///
/// Examples:
/// ```text
/// scip-typescript npm . . ButtonComponent#render().
///   → ButtonComponent#render
///
/// scip-php . . . MyClass#myMethod().
///   → MyClass#myMethod
///
/// local 42
///   → (empty — local symbols are filtered out by the caller)
/// ```
fn normalize_symbol(symbol: &str) -> String {
    if symbol.starts_with("local ") {
        return String::new();
    }

    // SCIP symbol: <scheme> <space> <3 package parts> <space> <descriptors>
    // Split on spaces to find the descriptor portion (everything after the
    // first 4 space-separated tokens: scheme + 3 package fields).
    let parts: Vec<&str> = symbol.splitn(5, ' ').collect();
    let descriptor = match parts.get(4) {
        Some(d) => *d,
        None => return symbol.to_string(), // unexpected format, keep as-is
    };

    // Strip trailing punctuation used by SCIP descriptor syntax:
    // Methods end with `).`, types end with `#`, namespaces end with `/`, terms end with `.`
    // We want to keep separators between components (e.g. `MyClass#render`) but remove
    // the trailing suffix of the last component.
    let cleaned = descriptor
        .trim_end_matches("().")
        .trim_end_matches('.')
        .trim_end_matches('#')
        .trim_end_matches('/');

    // Remove backtick escaping used for identifiers with special characters.
    let unescaped = cleaned.replace('`', "");

    unescaped
}

/// Extract the enclosing scope (e.g. class name) from a SCIP symbol descriptor.
///
/// For `ButtonComponent#render().` → returns `Some("ButtonComponent")`
/// For `parseFile().`              → returns `None`
fn extract_enclosing_scope(symbol: &str) -> Option<String> {
    let parts: Vec<&str> = symbol.splitn(5, ' ').collect();
    let descriptor = parts.get(4)?;

    // Find the last `#` or `/` separator — anything before it is the scope.
    if let Some(pos) = descriptor.rfind(|c| c == '#' || c == '/') {
        let scope = descriptor[..pos]
            .trim_end_matches('.')
            .trim_end_matches('#')
            .trim_end_matches('/')
            .replace('`', "");
        if !scope.is_empty() {
            return Some(scope);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Kind / role helpers
// ---------------------------------------------------------------------------

/// `true` when the SCIP SymbolKind represents something callable (function,
/// method, constructor, etc.).
fn is_callable_kind(kind: Option<SymbolKind>) -> bool {
    matches!(
        kind,
        Some(
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::StaticMethod
                | SymbolKind::Constructor
                | SymbolKind::AbstractMethod
                | SymbolKind::Getter
                | SymbolKind::Setter
        )
    )
}

/// Map SCIP occurrence roles + callee kind to a [`ReferenceKind`].
fn infer_reference_kind(
    roles: i32,
    callee_kind: Option<SymbolKind>,
) -> ReferenceKind {
    if (roles & ROLE_IMPORT) != 0 {
        return ReferenceKind::Import;
    }

    match callee_kind {
        Some(
            SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::TypeAlias
            | SymbolKind::Type,
        ) => ReferenceKind::TypeReference,
        Some(SymbolKind::Constructor) => ReferenceKind::Instantiation,
        Some(
            SymbolKind::Method
            | SymbolKind::AbstractMethod
            | SymbolKind::StaticMethod
            | SymbolKind::Getter
            | SymbolKind::Setter,
        ) => ReferenceKind::MethodCall,
        Some(SymbolKind::Function) => ReferenceKind::Call,
        _ => {
            if (roles & ROLE_READ_ACCESS) != 0 {
                ReferenceKind::VariableReference
            } else {
                ReferenceKind::Unknown
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Language mapping
// ---------------------------------------------------------------------------

/// Map the SCIP `Document.language` string to our domain [`Language`] enum.
///
/// SCIP encodes language as a free-form string (e.g. `"JavaScript"`, `"PHP"`)
/// as defined by the SCIP specification
/// (<https://github.com/sourcegraph/scip/blob/main/scip.proto>).
///
/// When the indexer leaves `language` empty (scip-typescript does this for JS
/// files with an inferred tsconfig), we fall back to the file extension.
fn scip_language_to_domain(lang: &str, path: &str) -> Language {
    match lang.to_lowercase().as_str() {
        "javascript" | "javascriptreact" => Language::JavaScript,
        "typescript" | "typescriptreact" => Language::TypeScript,
        "php" => Language::Php,
        "rust" => Language::Rust,
        "python" => Language::Python,
        "go" => Language::Go,
        "" => Language::from_path(Path::new(path)),
        _ => Language::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_symbol_method() {
        let sym = "scip-typescript npm . . ButtonComponent#render().";
        assert_eq!(normalize_symbol(sym), "ButtonComponent#render");
    }

    #[test]
    fn test_normalize_symbol_function() {
        let sym = "scip-typescript npm . . parseFile().";
        assert_eq!(normalize_symbol(sym), "parseFile");
    }

    #[test]
    fn test_normalize_symbol_php_method() {
        let sym = "scip-php . . . MyClass#myMethod().";
        assert_eq!(normalize_symbol(sym), "MyClass#myMethod");
    }

    #[test]
    fn test_normalize_symbol_local() {
        assert_eq!(normalize_symbol("local 42"), "");
    }

    #[test]
    fn test_extract_enclosing_scope_method() {
        let sym = "scip-typescript npm . . ButtonComponent#render().";
        assert_eq!(
            extract_enclosing_scope(sym),
            Some("ButtonComponent".to_string())
        );
    }

    #[test]
    fn test_extract_enclosing_scope_top_level() {
        let sym = "scip-typescript npm . . parseFile().";
        assert_eq!(extract_enclosing_scope(sym), None);
    }

    #[test]
    fn test_find_enclosing_scope_basic() {
        let defs = vec![
            ScopeDef {
                line: 5,
                symbol: "render".to_string(),
                enclosing_scope: Some("MyClass".to_string()),
            },
            ScopeDef {
                line: 15,
                symbol: "update".to_string(),
                enclosing_scope: Some("MyClass".to_string()),
            },
        ];

        // Reference at line 10 → inside render (line 5)
        let scope = find_enclosing_scope(&defs, 10).unwrap();
        assert_eq!(scope.symbol, "render");

        // Reference at line 20 → inside update (line 15)
        let scope = find_enclosing_scope(&defs, 20).unwrap();
        assert_eq!(scope.symbol, "update");

        // Reference at line 3 → no enclosing scope yet
        assert!(find_enclosing_scope(&defs, 3).is_none());
    }

    #[test]
    fn test_scip_language_mapping() {
        assert_eq!(
            scip_language_to_domain("TypeScript", "foo.ts"),
            Language::TypeScript
        );
        assert_eq!(
            scip_language_to_domain("JavaScriptReact", "foo.jsx"),
            Language::JavaScript
        );
        assert_eq!(scip_language_to_domain("PHP", "foo.php"), Language::Php);
        assert_eq!(
            scip_language_to_domain("Haskell", "foo.hs"),
            Language::Unknown
        );
        // Empty language field → infer from extension
        assert_eq!(
            scip_language_to_domain("", "api/index.js"),
            Language::JavaScript
        );
        assert_eq!(
            scip_language_to_domain("", "src/main.ts"),
            Language::TypeScript
        );
    }
}
