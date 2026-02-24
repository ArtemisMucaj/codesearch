use serde::Deserialize;
use zed_extension_api::{self as zed, SlashCommand, SlashCommandOutput, SlashCommandOutputSection};

// Matches the JSON schema produced by `codesearch search --format json`.
#[derive(Deserialize)]
struct SearchResult {
    file_path: String,
    start_line: u32,
    #[allow(dead_code)]
    end_line: u32,
    score: f32,
    language: String,
    node_type: String,
    symbol_name: Option<String>,
    content: String,
    #[allow(dead_code)]
    repository_id: String,
}

struct CodesearchExtension;

impl zed::Extension for CodesearchExtension {
    fn new() -> Self {
        CodesearchExtension
    }

    fn complete_slash_command_argument(
        &self,
        _command: SlashCommand,
        _args: Vec<String>,
    ) -> Result<Vec<zed::SlashCommandArgumentCompletion>, String> {
        // No tab-completions for free-form queries or symbol names.
        Ok(vec![])
    }

    fn run_slash_command(
        &self,
        command: SlashCommand,
        args: Vec<String>,
        worktree: Option<&zed::Worktree>,
    ) -> Result<SlashCommandOutput, String> {
        let arg = args.join(" ");
        let arg = arg.trim();

        match command.name.as_str() {
            "codesearch" => run_search(arg, worktree),
            "codesearch-impact" => run_impact(arg, worktree),
            "codesearch-context" => run_context(arg, worktree),
            name => Err(format!("unknown codesearch slash command: {name}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn run_search(query: &str, _worktree: Option<&zed::Worktree>) -> Result<SlashCommandOutput, String> {
    if query.is_empty() {
        return Err("Usage: /codesearch <natural language query>\nExample: /codesearch error handling for network timeouts".into());
    }

    let output = std::process::Command::new("codesearch")
        .args(["search", query, "--format", "json", "--num", "10"])
        .output()
        .map_err(|e| format!("failed to run codesearch — is it on your PATH? ({e})"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("codesearch exited with an error:\n{stderr}"));
    }

    let results: Vec<SearchResult> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("failed to parse codesearch output: {e}"))?;

    if results.is_empty() {
        return Ok(SlashCommandOutput {
            text: format!("No results found for `{query}`."),
            sections: vec![],
        });
    }

    let mut text = String::new();
    let mut sections: Vec<SlashCommandOutputSection> = Vec::new();

    for (i, r) in results.iter().enumerate() {
        let symbol = r.symbol_name.as_deref().unwrap_or(r.node_type.as_str());
        let section_start = text.len();

        text.push_str(&format!(
            "### {}. `{}` — {}:{} (score: {:.3})\n",
            i + 1,
            symbol,
            r.file_path,
            r.start_line,
            r.score,
        ));
        text.push_str(&format!("```{}\n{}\n```\n\n", r.language, r.content));

        let section_end = text.len();
        sections.push(SlashCommandOutputSection {
            range: (section_start..section_end).into(),
            icon: zed::SlashCommandOutputSectionIcon::Code,
            label: format!("{}:{}", r.file_path, r.start_line),
        });
    }

    Ok(SlashCommandOutput { text, sections })
}

// ---------------------------------------------------------------------------
// Impact analysis
// ---------------------------------------------------------------------------

fn run_impact(symbol: &str, _worktree: Option<&zed::Worktree>) -> Result<SlashCommandOutput, String> {
    if symbol.is_empty() {
        return Err("Usage: /codesearch-impact <symbol>\nExample: /codesearch-impact authenticate".into());
    }

    let output = std::process::Command::new("codesearch")
        .args(["impact", symbol, "--format", "text"])
        .output()
        .map_err(|e| format!("failed to run codesearch — is it on your PATH? ({e})"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("codesearch exited with an error:\n{stderr}"));
    }

    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let len = text.len();

    Ok(SlashCommandOutput {
        sections: vec![SlashCommandOutputSection {
            range: (0..len).into(),
            icon: zed::SlashCommandOutputSectionIcon::Code,
            label: format!("Impact: {symbol}"),
        }],
        text,
    })
}

// ---------------------------------------------------------------------------
// Symbol context
// ---------------------------------------------------------------------------

fn run_context(symbol: &str, _worktree: Option<&zed::Worktree>) -> Result<SlashCommandOutput, String> {
    if symbol.is_empty() {
        return Err("Usage: /codesearch-context <symbol>\nExample: /codesearch-context validate_email".into());
    }

    let output = std::process::Command::new("codesearch")
        .args(["context", symbol, "--format", "text"])
        .output()
        .map_err(|e| format!("failed to run codesearch — is it on your PATH? ({e})"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("codesearch exited with an error:\n{stderr}"));
    }

    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let len = text.len();

    Ok(SlashCommandOutput {
        sections: vec![SlashCommandOutputSection {
            range: (0..len).into(),
            icon: zed::SlashCommandOutputSectionIcon::Code,
            label: format!("Context: {symbol}"),
        }],
        text,
    })
}

zed::register_extension!(CodesearchExtension);
