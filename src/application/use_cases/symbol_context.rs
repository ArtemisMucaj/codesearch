use std::sync::Arc;

use serde::{Deserialize, Serialize};

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
    pub async fn get_context(
        &self,
        symbol: &str,
        repository_id: Option<&str>,
        limit: Option<u32>,
    ) -> Result<SymbolContext, DomainError> {
        let mut query = CallGraphQuery::new();
        if let Some(repo_id) = repository_id {
            query = query.with_repository(repo_id);
        }
        if let Some(l) = limit {
            query = query.with_limit(l);
        }

        let (callers_result, callees_result) = tokio::join!(
            self.call_graph.find_callers(symbol, &query),
            self.call_graph.find_callees(symbol, &query),
        );

        let callers = callers_result?;
        let callees = callees_result?;

        let caller_count = callers.len();
        let callee_count = callees.len();

        Ok(SymbolContext {
            symbol: symbol.to_string(),
            callers: callers.iter().map(Self::to_edge_caller).collect(),
            callees: callees.iter().map(Self::to_edge_callee).collect(),
            caller_count,
            callee_count,
        })
    }

    fn to_edge_caller(r: &SymbolReference) -> ContextEdge {
        ContextEdge {
            symbol: r
                .caller_symbol()
                .unwrap_or("<anonymous>")
                .to_string(),
            file_path: r.caller_file_path().to_string(),
            line: r.reference_line(),
            reference_kind: r.reference_kind().to_string(),
        }
    }

    fn to_edge_callee(r: &SymbolReference) -> ContextEdge {
        ContextEdge {
            symbol: r.callee_symbol().to_string(),
            file_path: r.reference_file_path().to_string(),
            line: r.reference_line(),
            reference_kind: r.reference_kind().to_string(),
        }
    }
}
