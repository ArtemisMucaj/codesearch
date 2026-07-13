use anyhow::{Context, Result};

use crate::cli::{OutputFormat, OutputFormatTextJson, VizLevel};
use crate::domain::{CommunityCoupling, CouplingElementKind, CouplingReport, GraphLevel};

use super::super::Container;

pub struct CouplingsController<'a> {
    container: &'a Container,
}

impl<'a> CouplingsController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    /// Detect coupling elements in the repository's Leiden communities and
    /// render the report.
    pub async fn couplings(
        &self,
        repository: Option<String>,
        level: VizLevel,
        format: OutputFormatTextJson,
    ) -> Result<String> {
        let repository_id = self
            .container
            .resolve_repository_id(repository.as_deref())
            .await;
        let level = match level {
            VizLevel::File => GraphLevel::File,
            VizLevel::Symbol => GraphLevel::Symbol,
        };
        let report = self
            .container
            .coupling_detection_use_case()
            .detect(&repository_id, level)
            .await
            .context("detecting coupling elements")?;

        let format: OutputFormat = format.into();
        Ok(match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).context("serializing coupling report")?
            }
            OutputFormat::Vimgrep => {
                anyhow::bail!("vimgrep output format is not supported for couplings")
            }
            OutputFormat::Text => render_text(&report),
        })
    }
}

fn render_text(report: &CouplingReport) -> String {
    let noun = report.level.node_noun();
    let mut out = format!(
        "Coupling analysis for `{}` ({} level)\n\
         {} communities — {} internally fragile, {} with verified couplers\n\
         ────────────────────────────────────────────────────\n",
        report.repository_id,
        noun,
        report.total_communities,
        report.fragile_communities,
        report.communities.len(),
    );
    if report.communities.is_empty() {
        out.push_str(
            "No coupling elements found: no community is held together by a single \
             node or edge at the probed resolutions.\n",
        );
        return out;
    }
    for (i, community) in report.communities.iter().enumerate() {
        out.push_str(&render_community(i + 1, community, noun));
    }
    out
}

fn render_community(rank: usize, c: &CommunityCoupling, noun: &str) -> String {
    let mut out = format!(
        "{:>3}. {} ({} {}s) — holds to γ≤{}, splits at γ={} into {} + {}\n",
        rank,
        c.community_id,
        c.size,
        noun,
        c.gamma_hold,
        c.gamma_split,
        c.sub_block_a.len(),
        c.sub_block_b.len(),
    );
    out.push_str(&render_block("block A", &c.sub_block_a));
    out.push_str(&render_block("block B", &c.sub_block_b));
    out.push_str("     couplers:\n");
    for coupler in &c.couplers {
        let (kind, element) = match coupler.kind {
            CouplingElementKind::Node => ("node", coupler.elements.join("")),
            CouplingElementKind::Edge => ("edge", coupler.elements.join(" ↔ ")),
        };
        out.push_str(&format!(
            "     • {} {} — strength {:.2} (split probability {:.2} vs baseline {:.2}), \
             cut share {:.2}",
            kind,
            element,
            coupler.coupling_strength,
            coupler.split_probability,
            coupler.baseline_split_probability,
            coupler.min_cut_share,
        ));
        if coupler.kind == CouplingElementKind::Node {
            out.push_str(&format!(", participation {:.2}", coupler.participation));
        }
        out.push_str(&format!(
            ", active γ {}–{}\n",
            coupler.gamma_low, coupler.gamma_high
        ));
    }
    out
}

fn render_block(label: &str, members: &[String]) -> String {
    const PREVIEW: usize = 4;
    let mut line = format!("     {} ({}): ", label, members.len());
    line.push_str(
        &members
            .iter()
            .take(PREVIEW)
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
    );
    if members.len() > PREVIEW {
        line.push_str(&format!(", … {} more", members.len() - PREVIEW));
    }
    line.push('\n');
    line
}
