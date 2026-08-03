//! Coupling-element detection: find the glue holding fragile Leiden
//! communities together.
//!
//! A **coupling element** is defined counterfactually: node or edge X couples
//! community C when C is a single community in the baseline Leiden partition,
//! but removing X makes C's nodes split into two blocks A and B. The
//! load-bearing case is a community that is *internally* two latent sub-blocks
//! held together by one file, symbol, or dependency — the classic hub-like
//! dependency / modularity-violation smell.
//!
//! The whole filter-then-verify pipeline (localize fragile communities, score
//! candidates from the A↔B min cut and participation coefficients, verify each
//! by ablation across seeded re-clusterings, then sweep the resolution ladder)
//! lives in the domain-agnostic [`leiden_coupling`] crate. This use case builds
//! the codesearch graph, runs [`leiden_coupling::analyze`], and maps the
//! generic result back into the [`CouplingReport`] domain type — attaching the
//! repository id, graph level, and the stable, content-addressed community id
//! that matches the `clusters` / `symbol-clusters` commands.

use std::sync::Arc;

// Leiden itself moved out to the `leiden` / `leiden_coupling` crates on this
// branch, so only the graph builders still come from the file-level module.
use leiden_coupling::{analyze, CommunityCoupling as CrateCoupling, Coupler, CouplerKind};

use super::cluster_detection::{build_file_leiden_graph, qualify_namespace_graph};
use super::{FileRelationshipUseCase, SymbolClusterDetectionUseCase};
use crate::domain::{
    namespace_scope_id, stable_community_id, CommunityCoupling, CouplingElement,
    CouplingElementKind, CouplingReport, DomainError, GraphLevel,
};

// ── Use case ──────────────────────────────────────────────────────────────

/// Use case: detect coupling elements in a repository's Leiden communities, at
/// either the file or the symbol level.
pub struct CouplingDetectionUseCase {
    file_graph: Arc<FileRelationshipUseCase>,
    symbol_clusters: Arc<SymbolClusterDetectionUseCase>,
}

impl CouplingDetectionUseCase {
    pub fn new(
        file_graph: Arc<FileRelationshipUseCase>,
        symbol_clusters: Arc<SymbolClusterDetectionUseCase>,
    ) -> Self {
        Self {
            file_graph,
            symbol_clusters,
        }
    }

    /// Run the full filter-then-verify pipeline on `repository_id` at `level`.
    ///
    /// The graph and baseline partition are rebuilt with the same code paths
    /// as cluster / symbol-community detection, so community ids in the report
    /// match those commands' output for unchanged memberships.
    pub async fn detect(
        &self,
        repository_id: &str,
        level: GraphLevel,
    ) -> Result<CouplingReport, DomainError> {
        let (names, graph, id_prefix) = match level {
            GraphLevel::File => {
                let fg = self
                    .file_graph
                    .build_graph(Some(&[repository_id.to_string()]), 1, false)
                    .await?;
                let (files, g) = build_file_leiden_graph(&fg);
                (files, g, "c")
            }
            GraphLevel::Symbol => {
                let sg = self
                    .symbol_clusters
                    .build_symbol_graph(repository_id)
                    .await?;
                (sg.symbols, sg.graph, "s")
            }
        };

        let analysis = analyze(&graph, &names);
        let communities = analysis
            .communities
            .into_iter()
            .map(|c| map_community(c, id_prefix))
            .collect();

        Ok(CouplingReport {
            repository_id: repository_id.to_string(),
            level,
            total_communities: analysis.total_communities,
            fragile_communities: analysis.fragile_communities,
            communities,
        })
    }

    /// Run the coupling pipeline over the **namespace-wide** graph — every
    /// repository in `namespace`, cross-repository edges included — at `level`.
    ///
    /// This is where global couplings earn their keep: a coupler that splits a
    /// namespace-wide community is the shared file/symbol welding two
    /// repositories together (a leaky service boundary). File-level couplers are
    /// reported as `repo:path` (the qualified node labels), so it's clear which
    /// repository's element is the glue; symbol-level couplers are FQNs, which
    /// are already globally unique.
    ///
    /// The report is keyed under the per-namespace sentinel scope id, matching
    /// the namespace cluster / symbol-community runs.
    pub async fn detect_namespace(
        &self,
        namespace: &str,
        level: GraphLevel,
    ) -> Result<CouplingReport, DomainError> {
        let (names, graph, id_prefix) = match level {
            GraphLevel::File => {
                // Every repo, cross-repo edges included, nodes qualified `repo:path`
                // exactly as the namespace file-cluster / graph-view paths do.
                let fg = qualify_namespace_graph(self.file_graph.build_graph(None, 1, true).await?);
                let (files, g) = build_file_leiden_graph(&fg);
                (files, g, "c")
            }
            GraphLevel::Symbol => {
                let sg = self
                    .symbol_clusters
                    .build_namespace_symbol_graph(Some(namespace))
                    .await?;
                (sg.symbols, sg.graph, "s")
            }
        };

        // Same two-step as the per-repository path above: the crate analyses the
        // graph, then `map_community` attaches the stable id. (Pre-extraction
        // this was one `analyze_graph(.., id_prefix)` call; the id derivation is
        // codesearch's, so it stayed here when Leiden moved out.)
        let analysis = analyze(&graph, &names);
        let communities = analysis
            .communities
            .into_iter()
            .map(|c| map_community(c, id_prefix))
            .collect();

        Ok(CouplingReport {
            repository_id: namespace_scope_id(namespace),
            level,
            total_communities: analysis.total_communities,
            fragile_communities: analysis.fragile_communities,
            communities,
        })
    }
}

// ── Mapping crate results → codesearch domain types ───────────────────────

/// Map one generic [`leiden_coupling::CommunityCoupling`] into the codesearch
/// [`CommunityCoupling`], attaching the stable community id derived from the
/// (already sorted) member names.
fn map_community(c: CrateCoupling, id_prefix: &str) -> CommunityCoupling {
    CommunityCoupling {
        community_id: stable_community_id(id_prefix, &c.members),
        size: c.size,
        gamma_hold: c.gamma_hold,
        gamma_split: c.gamma_split,
        sub_block_a: c.block_a,
        sub_block_b: c.block_b,
        couplers: c.couplers.into_iter().map(map_coupler).collect(),
    }
}

/// Map one generic [`leiden_coupling::Coupler`] into a codesearch
/// [`CouplingElement`].
fn map_coupler(c: Coupler) -> CouplingElement {
    CouplingElement {
        kind: match c.kind {
            CouplerKind::Node => CouplingElementKind::Node,
            CouplerKind::Edge => CouplingElementKind::Edge,
        },
        elements: c.elements,
        participation: c.participation,
        min_cut_share: c.min_cut_share,
        baseline_split_probability: c.baseline_split_probability,
        split_probability: c.split_probability,
        coupling_strength: c.coupling_strength,
        gamma_low: c.gamma_low,
        gamma_high: c.gamma_high,
    }
}
