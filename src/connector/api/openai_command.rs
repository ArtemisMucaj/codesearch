//! `codesearch openai …` — manage OpenAI-compatible endpoints.
//!
//! Register named endpoints (LM Studio, vLLM, hosted OpenAI, …), choose the
//! active one, list each endpoint's models, and pick a model interactively.
//! State lives in `<data_dir>/config.json` under the `openai` section and is
//! used whenever `--llm-target open-ai` runs. The same config is editable at
//! runtime through the serve management API (`/api/llm/endpoints`), so the CLI
//! and a running server share one source of truth.

use anyhow::{Context, Result};

use crate::cli::OpenaiSubcommand;
use crate::connector::adapter::{CodesearchConfig, OpenAiChatClient, OpenAiEndpoint};

mod picker;

/// Entry point dispatched from the router for `codesearch openai <sub>`.
pub async fn run(subcommand: OpenaiSubcommand, data_dir: &str) -> Result<String> {
    match subcommand {
        OpenaiSubcommand::Endpoints { json } => endpoints(data_dir, json),
        OpenaiSubcommand::Add {
            name,
            base_url,
            model,
            api_key,
            set_active,
        } => add(data_dir, name, base_url, model, api_key, set_active),
        OpenaiSubcommand::Use { name } => use_endpoint(data_dir, name),
        OpenaiSubcommand::Models { endpoint, json } => models(data_dir, endpoint, json).await,
        OpenaiSubcommand::Select { endpoint } => select(data_dir, endpoint).await,
    }
}

/// `openai endpoints`: list configured endpoints (keys masked).
fn endpoints(data_dir: &str, json: bool) -> Result<String> {
    let openai = CodesearchConfig::load(data_dir)?.openai.unwrap_or_default();
    if json {
        // Mask keys in the JSON view too.
        let masked: Vec<_> = openai
            .endpoints
            .iter()
            .map(|(name, ep)| {
                serde_json::json!({
                    "name": name,
                    "base_url": ep.base_url,
                    "model": ep.model,
                    "has_key": ep.api_key.as_deref().is_some_and(|k| !k.is_empty()),
                    "active": openai.active.as_deref() == Some(name.as_str()),
                })
            })
            .collect();
        return serde_json::to_string_pretty(&serde_json::json!({
            "active": openai.active,
            "endpoints": masked,
        }))
        .context("failed to serialize endpoints");
    }

    if openai.endpoints.is_empty() {
        return Ok(
            "No OpenAI endpoints configured. Add one with `codesearch openai add`.".to_string(),
        );
    }
    let mut out = String::from("  NAME              MODEL                     URL\n");
    for (name, ep) in &openai.endpoints {
        let marker = if openai.active.as_deref() == Some(name.as_str()) {
            "*"
        } else {
            " "
        };
        out.push_str(&format!(
            "{marker} {:<16}  {:<24}  {}\n",
            name,
            ep.model.as_deref().unwrap_or("-"),
            ep.base_url,
        ));
    }
    out.push_str("\n* = active endpoint");
    Ok(out)
}

/// `openai add`: register or update an endpoint.
fn add(
    data_dir: &str,
    name: String,
    base_url: String,
    model: Option<String>,
    api_key: Option<String>,
    set_active: bool,
) -> Result<String> {
    if name.trim().is_empty() {
        anyhow::bail!("endpoint name must not be empty");
    }
    let mut cfg = CodesearchConfig::load(data_dir)?;
    let openai = cfg.openai_mut();
    openai.endpoints.insert(
        name.clone(),
        OpenAiEndpoint {
            base_url,
            model,
            api_key: api_key.filter(|k| !k.is_empty()),
        },
    );
    let made_active = set_active || openai.active.is_none();
    if made_active {
        openai.active = Some(name.clone());
    }
    cfg.save(data_dir)?;
    Ok(format!(
        "Saved endpoint '{name}'{}.",
        if made_active { " (now active)" } else { "" }
    ))
}

/// `openai use`: set the active endpoint.
fn use_endpoint(data_dir: &str, name: String) -> Result<String> {
    let mut cfg = CodesearchConfig::load(data_dir)?;
    let openai = cfg.openai_mut();
    if !openai.endpoints.contains_key(&name) {
        anyhow::bail!("no endpoint named '{name}' (add it with `codesearch openai add`)");
    }
    openai.active = Some(name.clone());
    cfg.save(data_dir)?;
    Ok(format!("Active endpoint set to '{name}'."))
}

/// `openai models`: list the models an endpoint offers.
async fn models(data_dir: &str, endpoint: Option<String>, json: bool) -> Result<String> {
    let client = OpenAiChatClient::from_config(data_dir, endpoint.as_deref())
        .context("failed to initialise OpenAI client")?;
    let ids = client
        .list_models()
        .await
        .context("failed to list models (is the server reachable?)")?;

    if json {
        return serde_json::to_string_pretty(&ids).context("failed to serialize models");
    }
    if ids.is_empty() {
        return Ok("No models reported by this endpoint.".to_string());
    }
    Ok(ids.join("\n"))
}

/// `openai select`: pick a model interactively and save it to the endpoint.
async fn select(data_dir: &str, endpoint: Option<String>) -> Result<String> {
    // Resolve which endpoint we're configuring: the override, else the active.
    let cfg = CodesearchConfig::load(data_dir)?;
    let openai = cfg.openai.clone().unwrap_or_default();
    let Some(name) = endpoint.or(openai.active.clone()) else {
        anyhow::bail!(
            "no endpoint specified and none is active; pass --endpoint or run `codesearch openai use`"
        );
    };
    if !openai.endpoints.contains_key(&name) {
        anyhow::bail!("no endpoint named '{name}'");
    }

    let client = OpenAiChatClient::from_config(data_dir, Some(&name))
        .context("failed to initialise OpenAI client")?;
    let ids = client
        .list_models()
        .await
        .context("failed to list models (is the server reachable?)")?;
    if ids.is_empty() {
        return Ok(format!("Endpoint '{name}' reported no models."));
    }

    let current = openai.endpoints.get(&name).and_then(|e| e.model.clone());
    let preselected = current
        .as_deref()
        .and_then(|m| ids.iter().position(|id| id == m))
        .unwrap_or(0);

    let chosen = tokio::task::spawn_blocking(move || {
        picker::run(&ids, preselected).map(|choice| choice.map(|i| ids[i].clone()))
    })
    .await
    .map_err(|e| anyhow::anyhow!("model picker task panicked: {e}"))??;

    let Some(chosen) = chosen else {
        return Ok("No model selected — endpoint unchanged.".to_string());
    };

    let mut cfg = CodesearchConfig::load(data_dir)?;
    if let Some(ep) = cfg.openai_mut().endpoints.get_mut(&name) {
        ep.model = Some(chosen.clone());
    }
    cfg.save(data_dir)?;
    Ok(format!(
        "Selected model '{chosen}' for endpoint '{name}'.\n\
         Use it with: codesearch <command> --llm-target open-ai"
    ))
}
