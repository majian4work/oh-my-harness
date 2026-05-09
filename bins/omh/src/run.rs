use std::env;

use anyhow::{Context, Result, bail};
use runtime::AgentRuntime;

use crate::{auth, init_harness};

/// Default omt endpoint to probe.
const DEFAULT_OMT_ENDPOINT: &str = "http://127.0.0.1:9120";

pub async fn run_oneshot(
    prompt: &str,
    agent: &str,
    continue_last: bool,
    effort: &str,
    model: Option<&str>,
) -> Result<()> {
    let mut harness = init_harness()?;
    register_providers_from_env(&mut harness)?;

    let workspace_root = std::env::current_dir()?;
    harness.connect_mcp_servers(&workspace_root);

    // Attempt to join omt team in the background
    let _team_handle = try_join_omt_team(agent);

    let agent_def = harness
        .agent_registry
        .get(agent)
        .with_context(|| format!("unknown agent: {agent}"))?;

    if harness.provider_registry.list().is_empty() {
        bail!(
            "no provider configured. Set OPENAI_API_KEY or ANTHROPIC_API_KEY, or run 'omh auth login <provider> --key <api_key>'."
        );
    }

    let session_id = if continue_last {
        let sessions = harness
            .session_manager
            .list_for_workspace(1, &workspace_root)?;
        match sessions.first() {
            Some(s) => s.id.clone(),
            None => {
                harness
                    .session_manager
                    .create(agent, "", &workspace_root)?
                    .id
            }
        }
    } else {
        harness
            .session_manager
            .create(agent, "", &workspace_root)?
            .id
    };

    let effort_level = match effort {
        "low" => provider::Effort::Low,
        "high" => provider::Effort::High,
        _ => provider::Effort::Default,
    };

    let runtime = AgentRuntime::new(
        agent.to_string(),
        session_id.clone(),
        agent_def.max_turns.unwrap_or(30),
    );
    let mut runtime = runtime.with_logger(&harness);
    runtime.interactive = false;
    runtime.effort_override = Some(effort_level).filter(|e| *e != provider::Effort::Default);
    if let Some(model_str) = model {
        let (provider_id, model_id) = if let Some((p, m)) = model_str.split_once('/') {
            (Some(p.to_string()), m.to_string())
        } else {
            (None, model_str.to_string())
        };
        runtime.model_override = Some(provider::ModelSpec {
            model_id,
            provider_id,
        });
    }

    let start = std::time::Instant::now();
    let result = runtime.run_turn(&harness, prompt).await?;
    let elapsed = start.elapsed();

    if !result.response.is_empty() {
        println!("{}", result.response);
    }

    let pending = harness
        .background_tasks
        .wait_all(std::time::Duration::from_secs(300))
        .await;
    if pending > 0 {
        eprintln!(
            "⚠ {} background task(s) still running after timeout",
            pending
        );
    }

    let tool_time_ms: u64 = result.tool_calls.iter().map(|tc| tc.duration_ms).sum();
    eprintln!(
        "\n─── {} tool call(s) ({:.1}s) │ {}in + {}out tokens │ {:.1}s ───",
        result.tool_calls_made,
        tool_time_ms as f64 / 1000.0,
        result.input_tokens,
        result.output_tokens,
        elapsed.as_secs_f64()
    );

    Ok(())
}

/// Try to join an omt team on startup (best-effort, non-blocking).
///
/// Checks `OMT_ENDPOINT` env var (or probes the default 127.0.0.1:9120).
/// If reachable, sends a team/join request with this omh instance's info.
/// Returns a JoinHandle that can be awaited or ignored.
fn try_join_omt_team(agent: &str) -> tokio::task::JoinHandle<()> {
    let role = agent.to_string();
    tokio::spawn(async move {
        let omt_url = env::var("OMT_ENDPOINT").unwrap_or_else(|_| DEFAULT_OMT_ENDPOINT.to_string());
        let client = a2a::A2aClient::new();

        // Quick probe
        if !client.probe(&omt_url).await {
            return;
        }

        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "omh".to_string());
        let pid = std::process::id();

        let card = a2a::AgentCard {
            name: format!("{hostname}-{pid}"),
            description: Some(format!("omh agent (role: {role})")),
            url: String::new(), // omh doesn't expose an A2A server
            provider: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: a2a::AgentCapabilities::default(),
            skills: vec![a2a::AgentSkill {
                id: role.clone(),
                name: role.clone(),
                description: Some(format!("Performs {role} tasks")),
                tags: vec![role.clone(), "coding".to_string()],
                examples: vec![],
            }],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
        };

        let request = a2a::TeamJoinRequest {
            card,
            endpoint: String::new(), // omh is not an A2A server
            role: role.clone(),
            capacity: 1,
        };

        match client.team_join(&omt_url, &request).await {
            Ok(resp) if resp.accepted => {
                tracing::info!("joined omt team as {:?} (role={role})", resp.instance_id);
            }
            Ok(resp) => {
                tracing::debug!(
                    "omt team join declined: {}",
                    resp.message.as_deref().unwrap_or("no reason")
                );
            }
            Err(e) => {
                tracing::debug!("failed to join omt team: {e:#}");
            }
        }
    })
}

pub(crate) fn register_providers_from_env(harness: &mut runtime::Harness) -> Result<()> {
    if let Ok(key) = env::var("OPENAI_API_KEY") {
        let base_url =
            env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".to_string());
        let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4.1".to_string());
        let provider = provider::openai_compat::OpenAICompatProvider::new(
            reqwest::Client::new(),
            base_url,
            key,
            model,
        );
        harness
            .provider_registry
            .register("openai", Box::new(provider));
    }

    if let Ok(key) = env::var("ANTHROPIC_API_KEY") {
        let model = env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-0".to_string());
        let provider =
            provider::anthropic::AnthropicProvider::new(reqwest::Client::new(), key, model);
        harness
            .provider_registry
            .register("anthropic", Box::new(provider));
    }

    let creds = auth::Credentials::load().unwrap_or_default();
    for (name, cred) in &creds.providers {
        if harness.provider_registry.get(name).is_some() {
            continue;
        }

        match cred.provider_type {
            auth::ProviderType::OpenAI | auth::ProviderType::Custom => {
                let base_url = cred
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com".to_string());
                let model = cred.model.clone().unwrap_or_else(|| "gpt-4.1".to_string());
                let provider = provider::openai_compat::OpenAICompatProvider::new(
                    reqwest::Client::new(),
                    base_url,
                    cred.api_key.clone(),
                    model,
                );
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
            auth::ProviderType::Anthropic => {
                let model = cred
                    .model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
                let provider = provider::anthropic::AnthropicProvider::new(
                    reqwest::Client::new(),
                    cred.api_key.clone(),
                    model,
                );
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
            auth::ProviderType::Copilot => {
                let provider = provider::copilot::CopilotProvider::new(
                    cred.api_key.clone(),
                    cred.model.clone(),
                );
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
        }
    }

    Ok(())
}
