//! Unified memory recall for the TUI: search (or browse) both the flat memory
//! items and the virtual-filesystem nodes in one ranked list.
//!
//! - A **query** runs hybrid semantic + keyword recall over items *and* nodes,
//!   fuses each modality with RRF, and interleaves the two into one list.
//! - An **empty query** browses everything: all items and all nodes, newest
//!   first — the "show me everything" view.
//!
//! The result is a flat `Vec<MemoryEntry>` the TUI renders as a single list,
//! where each entry is either a memory item or a filesystem node.

use std::collections::HashMap;
use std::sync::Arc;

use crate::application::interfaces::{EmbeddingService, MemoryRepository};
use crate::application::use_cases::memory_search::MemorySearchUseCase;
use crate::application::use_cases::memory_summary::MEMORY_ROOT_URI;
use crate::domain::{DomainError, MemoryItem, MemoryNode, NodeKind};

/// RRF dampening constant (matches [`MemorySearchUseCase`]).
const RRF_K: f32 = 60.0;

/// How many candidates the node legs retrieve before fusion.
const NODE_CANDIDATES_PER_LEG: usize = 20;

/// Sort rank for a node kind in the browse (default) view, so the virtual
/// filesystem reads top-down: the rollup first, then sessions, then resources.
fn node_kind_rank(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Memory => 0,
        NodeKind::Session => 1,
        NodeKind::Resource => 2,
    }
}

/// One entry in the unified memory list: a flat item or a filesystem node.
#[derive(Debug, Clone)]
pub enum MemoryEntry {
    Item { item: MemoryItem, score: f32 },
    Node { node: MemoryNode, score: f32 },
}

impl MemoryEntry {
    pub fn score(&self) -> f32 {
        match self {
            MemoryEntry::Item { score, .. } | MemoryEntry::Node { score, .. } => *score,
        }
    }

    /// Short kind label for the list (`preference`, `session`, …).
    pub fn kind_label(&self) -> String {
        match self {
            MemoryEntry::Item { item, .. } => item.kind().to_string(),
            MemoryEntry::Node { node, .. } => node.kind().to_string(),
        }
    }

    /// Primary label shown in the list: the item name or the node URI.
    pub fn label(&self) -> String {
        match self {
            MemoryEntry::Item { item, .. } => item.name().to_string(),
            MemoryEntry::Node { node, .. } => node.uri().to_string(),
        }
    }
}

/// Combined search/browse over memory items and virtual-filesystem nodes.
pub struct MemoryBrowseUseCase {
    memory_repo: Arc<dyn MemoryRepository>,
    embedding_service: Arc<dyn EmbeddingService>,
    item_search: MemorySearchUseCase,
}

impl MemoryBrowseUseCase {
    pub fn new(
        memory_repo: Arc<dyn MemoryRepository>,
        embedding_service: Arc<dyn EmbeddingService>,
    ) -> Self {
        let item_search =
            MemorySearchUseCase::new(Arc::clone(&memory_repo), Arc::clone(&embedding_service));
        Self {
            memory_repo,
            embedding_service,
            item_search,
        }
    }

    /// Search (non-empty query) or browse (empty query) the whole store.
    /// Returns up to `limit` entries, best first.
    pub async fn execute(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, DomainError> {
        let query = query.trim();
        if query.is_empty() {
            return self.browse(limit).await;
        }

        // Items: reuse the existing hybrid (semantic+keyword+RRF) recall.
        let items = self.item_search.execute(query, None, limit).await?;
        // Nodes: run the same two legs and fuse them here.
        let nodes = self.search_nodes(query, limit).await?;

        // Interleave the two ranked lists by score so items and nodes mix.
        let mut entries: Vec<MemoryEntry> = items
            .into_iter()
            .map(|(item, score)| MemoryEntry::Item { item, score })
            .chain(
                nodes
                    .into_iter()
                    .map(|(node, score)| MemoryEntry::Node { node, score }),
            )
            .collect();
        entries.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        entries.truncate(limit);
        Ok(entries)
    }

    /// Hybrid semantic + keyword recall over nodes, fused with RRF.
    async fn search_nodes(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryNode, f32)>, DomainError> {
        let semantic = if self.embedding_service.embeddings_enabled() {
            let vector = self.embedding_service.embed_query(query).await?;
            self.memory_repo
                .search_nodes_semantic(&vector, None, NODE_CANDIDATES_PER_LEG)
                .await?
        } else {
            Vec::new()
        };
        let keyword = self
            .memory_repo
            .search_nodes_keyword(query, None, NODE_CANDIDATES_PER_LEG)
            .await?;

        let mut fused: HashMap<String, (MemoryNode, f32)> = HashMap::new();
        for results in [semantic, keyword] {
            for (rank, (node, _score)) in results.into_iter().enumerate() {
                let contribution = 1.0 / (RRF_K + rank as f32 + 1.0);
                fused
                    .entry(node.uri().to_string())
                    .and_modify(|(_, score)| *score += contribution)
                    .or_insert((node, contribution));
            }
        }
        let mut results: Vec<(MemoryNode, f32)> = fused.into_values().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    /// Browse the whole virtual filesystem (empty query — the default view).
    ///
    /// Shows the filesystem first, in tree order (the `memory://memory` rollup,
    /// then sessions, then resources), followed by the flat memory items. No
    /// relevance ranking is applied; this is a structural view, not a search.
    async fn browse(&self, limit: usize) -> Result<Vec<MemoryEntry>, DomainError> {
        let items = self.memory_repo.list_items(None).await?;
        let mut nodes = self.memory_repo.list_nodes(None).await?;

        // Order the filesystem top-down: rollup → sessions → resources, and
        // within a kind keep newest-first (list_nodes already returns that), so
        // the rollup ("read this first") is always the first entry.
        nodes.sort_by(|a, b| {
            node_kind_rank(a.kind())
                .cmp(&node_kind_rank(b.kind()))
                .then_with(|| {
                    // Keep the canonical rollup URI pinned to the very top.
                    (b.uri() == MEMORY_ROOT_URI).cmp(&(a.uri() == MEMORY_ROOT_URI))
                })
                .then_with(|| b.updated_at().cmp(&a.updated_at()))
        });

        // Score 0.0 in browse mode — the list is structural, not ranked.
        let mut entries: Vec<MemoryEntry> = Vec::with_capacity(items.len() + nodes.len());
        entries.extend(
            nodes
                .into_iter()
                .map(|node| MemoryEntry::Node { node, score: 0.0 }),
        );
        entries.extend(
            items
                .into_iter()
                .map(|item| MemoryEntry::Item { item, score: 0.0 }),
        );
        entries.truncate(limit);
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MemoryKind;

    #[test]
    fn entry_accessors() {
        let item = MemoryItem::new(
            "id1".into(),
            MemoryKind::Fact,
            "duckdb_locks".into(),
            "content".into(),
            None,
            0,
            0,
            0,
        );
        let e = MemoryEntry::Item { item, score: 0.5 };
        assert_eq!(e.kind_label(), "fact");
        assert_eq!(e.label(), "duckdb_locks");
        assert_eq!(e.score(), 0.5);
    }
}
