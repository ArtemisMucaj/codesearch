use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::application::use_cases::pattern_utils::build_fuzzy_pattern;
use crate::application::{CallGraphQuery, CallGraphUseCase};
use crate::domain::{DomainError, SymbolReference};

/// A single dependency entry shown in the context view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEdge {
    /// The other symbol in the relationship.
    pub symbol: String,
    /// File where the reference occurs.
    pub file_path: String,
    /// Line number of the reference.
    pub line: u32,
    /// Kind of reference (e.g. "call", "type_reference").
    pub reference_kind: String,
    /// Local alias at the import/require site, if the symbol was renamed.
    /// For example `bar` in `import { foo as bar }` or `const { foo: bar } = require(...)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_alias: Option<String>,
}

/// 360-degree view of a symbol's call-graph relationships.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolContext {
    /// The symbol being examined.
    pub symbol: String,
    /// Who calls / references this symbol (inbound edges).
    pub callers: Vec<ContextEdge>,
    /// What this symbol calls / references (outbound edges).
    pub callees: Vec<ContextEdge>,
    /// Total number of inbound references.
    pub caller_count: usize,
    /// Total number of outbound references.
    pub callee_count: usize,
}

/// Default maximum number of fully-qualified symbols to resolve from a short name
/// when the exact match returns no results. Caps the ambiguity fan-out.
const FALLBACK_RESOLUTION_LIMIT: u32 = 10;

/// Use case: return a complete in + out dependency view for a named symbol.
pub struct SymbolContextUseCase {
    call_graph: Arc<CallGraphUseCase>,
}

impl SymbolContextUseCase {
    pub fn new(call_graph: Arc<CallGraphUseCase>) -> Self {
        Self { call_graph }
    }

    /// Fetch callers and callees for `symbol` in parallel and combine them.
    ///
    /// `repository_id` – optional filter; `limit` caps each direction independently.
    /// `is_regex`      – when `true`, `symbol` is used as-is as a POSIX regex
    ///                   (no auto-wrapping).  When `false` (the default), the
    ///                   symbol is first tried as an exact match; if that returns
    ///                   nothing it is automatically wrapped as `.*<symbol>.*` so
    ///                   that `codesearch context load` finds every FQN containing
    ///                   the substring "load".  Pass `--regex` to supply your own
    ///                   full pattern without auto-wrapping.
    pub async fn get_context(
        &self,
        symbol: &str,
        repository_id: Option<&str>,
        limit: Option<u32>,
        is_regex: bool,
    ) -> Result<SymbolContext, DomainError> {
        let mut query = CallGraphQuery::new();
        if let Some(repo_id) = repository_id {
            query = query.with_repository(repo_id);
        }
        if let Some(l) = limit {
            query = query.with_limit(l);
        }
        if is_regex {
            query = query.with_regex();
        }

        if !is_regex {
            // Try exact match first (existing fast-path, only in suffix mode).
            let (callers_result, callees_result) = tokio::join!(
                self.call_graph.find_callers(symbol, &query),
                self.call_graph.find_callees(symbol, &query),
            );

            let callers = callers_result?;
            let callees = callees_result?;

            if !callers.is_empty() || !callees.is_empty() {
                let caller_count = callers.len();
                let callee_count = callees.len();

                return Ok(SymbolContext {
                    symbol: symbol.to_string(),
                    callers: callers.iter().map(Self::to_edge_caller).collect(),
                    callees: callees.iter().map(Self::to_edge_callee).collect(),
                    caller_count,
                    callee_count,
                });
            }

            // Zero edges found — confirm whether the symbol actually exists in the
            // DB before expanding to fuzzy/regex.  resolve_symbols queries both
            // callee_symbol and caller_symbol with a suffix match; we then require
            // an exact string match so that a short name like "new" does not
            // falsely match a fully-qualified "MyStruct::new" and trigger an
            // early return.
            let probe = self
                .call_graph
                .resolve_symbols(symbol, &query, 1)
                .await?;
            let exact_resolved = probe.iter().any(|s| s == symbol);
            if exact_resolved {
                return Ok(SymbolContext {
                    symbol: symbol.to_string(),
                    callers: vec![],
                    callees: vec![],
                    caller_count: 0,
                    callee_count: 0,
                });
            }
        }

        // Regex mode  — use pattern as-is.
        // Auto-wrap mode — wrap as `.*<escaped>.*` so a bare name like "loadFile"
        // matches any FQN that contains it as a substring.
        let (resolve_pattern, resolve_query) = if is_regex {
            (symbol.to_string(), query.clone())
        } else {
            (
                format!(".*{}.*", build_fuzzy_pattern(symbol)),
                query.clone().with_regex(),
            )
        };

        let resolved = self
            .call_graph
            .resolve_symbols(&resolve_pattern, &resolve_query, FALLBACK_RESOLUTION_LIMIT)
            .await?;

        if resolved.is_empty() {
            return Ok(SymbolContext {
                symbol: symbol.to_string(),
                callers: vec![],
                callees: vec![],
                caller_count: 0,
                callee_count: 0,
            });
        }

        // If exactly one symbol matched, use it directly.
        // If multiple matched, aggregate results from all of them.
        let resolved_symbol = if resolved.len() == 1 {
            resolved[0].clone()
        } else {
            // Use the first match but collect from all
            resolved[0].clone()
        };

        let mut all_callers = Vec::new();
        let mut all_callees = Vec::new();

        for sym in &resolved {
            let (cr, ce) = tokio::join!(
                self.call_graph.find_callers(sym, &query),
                self.call_graph.find_callees(sym, &query),
            );
            all_callers.extend(cr?);
            all_callees.extend(ce?);
        }

        // Deduplicate by reference ID: the same DB row can appear more than once
        // when multiple resolved symbols share callers/callees, which would inflate
        // the counts and produce duplicate edges in the output.
        let mut seen: HashSet<String> = HashSet::new();
        all_callers.retain(|r| seen.insert(r.id().to_string()));
        seen.clear();
        all_callees.retain(|r| seen.insert(r.id().to_string()));

        let caller_count = all_callers.len();
        let callee_count = all_callees.len();

        let display_symbol = if resolved.len() == 1 {
            resolved_symbol
        } else {
            format!("{} (resolved {} symbols)", symbol, resolved.len())
        };

        Ok(SymbolContext {
            symbol: display_symbol,
            callers: all_callers.iter().map(Self::to_edge_caller).collect(),
            callees: all_callees.iter().map(Self::to_edge_callee).collect(),
            caller_count,
            callee_count,
        })
    }

    fn to_edge_caller(r: &SymbolReference) -> ContextEdge {
        ContextEdge {
            symbol: r.caller_symbol().unwrap_or("<anonymous>").to_string(),
            file_path: r.caller_file_path().to_string(),
            line: r.reference_line(),
            reference_kind: r.reference_kind().to_string(),
            import_alias: r.import_alias().map(str::to_string),
        }
    }

    fn to_edge_callee(r: &SymbolReference) -> ContextEdge {
        ContextEdge {
            symbol: r.callee_symbol().to_string(),
            file_path: r.reference_file_path().to_string(),
            line: r.reference_line(),
            reference_kind: r.reference_kind().to_string(),
            import_alias: r.import_alias().map(str::to_string),
        }
    }
}

