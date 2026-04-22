use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, Input, Password, Select};
use memory::{MemoryEntry, MemoryKind, MemorySource, Scope};
use provider::{CompletionRequest, SystemMessage};
use runtime::harness::AgentOverride;

use crate::{
    AuthCmd, EvolutionCmd, MemoryCmd, SnapshotCmd,
    auth::{
        self, Credentials, ModelsCache, ProviderCredential, check_env_providers, mask_key,
        provider_type_for_name,
    },
};

pub async fn cmd_sessions(limit: usize) -> Result<()> {
    let harness = crate::init_harness()?;
    let summaries = harness.session_manager.list(limit)?;

    if summaries.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    println!(
        "{:<40} {:<14} {:<8} {:<6} {}",
        "ID", "Agent", "Model", "Msgs", "Title"
    );
    println!("{}", "─".repeat(90));
    for s in &summaries {
        let title = if s.title.is_empty() {
            "(untitled)"
        } else {
            &s.title
        };
        let model_short = if s.model.len() > 12 {
            &s.model[..12]
        } else {
            &s.model
        };
        println!(
            "{:<40} {:<14} {:<8} {:<6} {}",
            s.id, s.agent_name, model_short, s.message_count, title
        );
    }
    println!("\n{} session(s)", summaries.len());

    Ok(())
}

pub async fn cmd_memory(cmd: MemoryCmd) -> Result<()> {
    let harness = crate::init_harness()?;

    match cmd {
        MemoryCmd::List => {
            let entries = harness.memory.list(&Scope::Global, None).await?;
            if entries.is_empty() {
                println!("No memory entries.");
            } else {
                for entry in &entries {
                    println!(
                        "[{}] ({:?}/{:?}) {}",
                        entry.id,
                        entry.scope,
                        entry.kind,
                        truncate(&entry.content, 80)
                    );
                }
                println!("\n{} entries", entries.len());
            }
        }
        MemoryCmd::Search { query } => {
            let entries = harness.memory.recall(&query, &Scope::Global, 10).await?;
            if entries.is_empty() {
                println!("No matches for '{query}'.");
            } else {
                for entry in &entries {
                    println!(
                        "[{}] ({:?}) {}",
                        entry.id,
                        entry.scope,
                        truncate(&entry.content, 80)
                    );
                }
            }
        }
        MemoryCmd::Add { content } => {
            let now = now_millis();
            let entry = MemoryEntry {
                id: ulid::Ulid::new(),
                scope: Scope::Global,
                kind: MemoryKind::Fact,
                content,
                source: MemorySource::UserAuthored,
                confidence: 1.0,
                reinforcement_count: 0,
                supersedes: None,
                created_at: now,
                updated_at: now,
            };
            harness.memory.remember(entry).await?;
            println!("Memory entry added.");
        }
        MemoryCmd::Forget { id } => {
            let ulid: ulid::Ulid = id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid ULID: {id}"))?;
            harness.memory.forget(ulid).await?;
            println!("Memory entry forgotten.");
        }
    }

    Ok(())
}

pub async fn cmd_evolution(cmd: EvolutionCmd) -> Result<()> {
    let harness = crate::init_harness()?;

    match cmd {
        EvolutionCmd::Log => {
            let entries = harness
                .memory
                .list(&Scope::Global, Some(MemoryKind::Rule))
                .await?;
            if entries.is_empty() {
                println!("No evolution entries.");
            } else {
                for entry in &entries {
                    println!(
                        "[{}] confidence={:.2} {}",
                        entry.id,
                        entry.confidence,
                        truncate(&entry.content, 70)
                    );
                }
            }
        }
        EvolutionCmd::Revert { id } => {
            let ulid: ulid::Ulid = id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid ULID: {id}"))?;
            harness.memory.forget(ulid).await?;
            println!("Evolution entry reverted (forgotten).");
        }
        EvolutionCmd::Consolidate => {
            let report = harness.evolution.consolidate(&Scope::Global)?;
            println!(
                "Consolidation: merged={}, pruned={}",
                report.merged, report.pruned
            );
        }
        EvolutionCmd::Pause => {
            println!("Evolution paused (not persisted — restart re-enables).");
        }
        EvolutionCmd::Resume => {
            println!("Evolution resumed.");
        }
    }

    Ok(())
}

pub async fn cmd_snapshot(cmd: SnapshotCmd) -> Result<()> {
    let _harness = crate::init_harness()?;

    match cmd {
        SnapshotCmd::List { session_id } => {
            println!("Snapshots for session {session_id}:");
            println!("  (snapshot listing requires git log integration — use `git log` for now)");
        }
        SnapshotCmd::Diff { snapshot_id } => {
            println!("Diff for snapshot {snapshot_id}:");
            println!("  (use `git diff {snapshot_id}` directly)");
        }
        SnapshotCmd::Revert { snapshot_id } => {
            println!("Reverting to snapshot {snapshot_id}...");
            let workspace_root = std::env::current_dir()?;
            let mgr = snapshot::GitSnapshot::new(&workspace_root);
            let snap_id = snapshot::SnapshotId(snapshot_id.clone());
            mgr.revert_to(&snap_id)?;
            println!("Reverted to {snapshot_id}.");
        }
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.replace('\n', " ")
    } else {
        format!("{}…", s[..max].replace('\n', " "))
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub async fn cmd_auth(cmd: AuthCmd) -> Result<()> {
    match cmd {
        AuthCmd::Login {
            provider,
            key,
            base_url,
            model,
        } => {
            let provider = match provider {
                Some(p) => p,
                None => select_provider()?,
            };
            let key = if provider == "copilot" {
                match key {
                    Some(k) => k,
                    None => copilot_login().await?,
                }
            } else {
                match key {
                    Some(k) => k,
                    None => prompt_api_key(&provider)?,
                }
            };
            let provider_type = provider_type_for_name(&provider);
            let mut creds = Credentials::load()?;
            creds.add(
                provider.clone(),
                ProviderCredential {
                    provider_type,
                    api_key: key,
                    base_url,
                    model,
                },
            );
            creds.save()?;
            println!("✓ Provider '{}' configured successfully", provider);
            println!("  Credentials saved to {}", Credentials::global_path().display());
        }
        AuthCmd::Logout { provider } => {
            let mut creds = Credentials::load()?;
            if creds.remove(&provider) {
                creds.save()?;
                println!("✓ Provider '{}' removed", provider);
            } else {
                println!("Provider '{}' not found in credentials", provider);
            }
        }
        AuthCmd::List => {
            let creds = Credentials::load()?;
            if creds.providers.is_empty() {
                println!("No providers configured.");
                println!("Use 'omh auth login <provider> --key <api_key>' to add one.");
            } else {
                println!("Configured providers:");
                let mut names: Vec<_> = creds.providers.keys().cloned().collect();
                names.sort();
                for name in names {
                    let cred = creds
                        .get(&name)
                        .expect("provider name collected from credentials must exist");
                    let model_info = cred.model.as_deref().unwrap_or("default");
                    println!(
                        "  {} ({:?}) key={} model={}",
                        name,
                        cred.provider_type,
                        mask_key(&cred.api_key),
                        model_info
                    );
                }
            }
        }
        AuthCmd::Status => {
            let creds = Credentials::load()?;
            let env_keys = check_env_providers();

            println!("Provider Status:");
            println!("─────────────────────────────────────");

            for (name, source) in &env_keys {
                println!("  ✓ {} (from {})", name, source);
            }
            for name in creds.providers.keys() {
                if !env_keys.iter().any(|(env_name, _)| env_name == name) {
                    println!("  ✓ {} (from credentials.json)", name);
                }
            }

            if env_keys.is_empty() && creds.providers.is_empty() {
                println!("  No providers configured.");
                println!(
                    "\n  Set environment variables (OPENAI_API_KEY, ANTHROPIC_API_KEY, GITHUB_COPILOT_TOKEN)"
                );
                println!("  Or use: omh auth login <provider> --key <api_key>");
            }
        }
    }

    Ok(())
}

async fn copilot_login() -> Result<String> {
    if let Some(token) = auth::read_copilot_hosts_token() {
        println!("Found existing GitHub Copilot token in ~/.config/github-copilot/");
        let use_existing = Confirm::new()
            .with_prompt("Use this token?")
            .default(true)
            .interact()?;
        if use_existing {
            return Ok(token);
        }
    }

    println!("Starting GitHub OAuth device flow...");
    let client = reqwest::Client::new();
    let device = auth::start_device_flow(&client).await?;

    println!("\n  Open: {}", device.verification_uri);
    println!("  Enter code: {}\n", device.user_code);
    println!("Waiting for authorization...");

    let token = auth::poll_for_access_token(&client, &device.device_code, device.interval).await?;
    println!("✓ Authorization successful!");
    Ok(token)
}

const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI (GPT-4, GPT-4.1, o3, ...)"),
    ("anthropic", "Anthropic (Claude Sonnet, Opus, ...)"),
    ("copilot", "GitHub Copilot"),
    ("custom", "Custom OpenAI-compatible endpoint"),
];

fn select_provider() -> Result<String> {
    let items: Vec<&str> = KNOWN_PROVIDERS.iter().map(|(_, desc)| *desc).collect();
    let selection = Select::new()
        .with_prompt("Select a provider")
        .items(&items)
        .default(0)
        .interact()?;

    let (name, _) = KNOWN_PROVIDERS[selection];
    if name == "custom" {
        let custom_name: String = Input::new().with_prompt("Provider name").interact_text()?;
        if custom_name.is_empty() {
            bail!("provider name cannot be empty");
        }
        Ok(custom_name)
    } else {
        Ok(name.to_string())
    }
}

fn prompt_api_key(provider: &str) -> Result<String> {
    let key = Password::new()
        .with_prompt(format!("API key for {provider}"))
        .interact()?;
    if key.is_empty() {
        bail!("API key cannot be empty");
    }
    Ok(key)
}

pub async fn cmd_update_best_models(global: bool) -> Result<()> {
    let mut harness = crate::init_harness()?;
    crate::cli::register_providers_from_env(&mut harness)?;

    if harness.provider_registry.list().is_empty() {
        bail!("no provider configured. Run 'omh auth login' first.");
    }

    println!("Refreshing model cache from all providers (validating accessibility)...");
    let old_cache = ModelsCache::load();
    let fresh_models = harness.provider_registry.list_all_models_validated().await;

    if fresh_models.is_empty() {
        bail!("no models returned from any provider");
    }

    let mut new_cache = ModelsCache::load();
    new_cache.update(&fresh_models);
    new_cache.save()?;

    let changed = has_model_changes(&old_cache, &new_cache);
    if !changed {
        println!("No model changes detected. Nothing to do.");
        return Ok(());
    }

    println!("Model changes detected. Asking LLM for optimal agent assignments...");

    let builtin_agents: Vec<_> = harness
        .agent_registry
        .all()
        .into_iter()
        .filter(|a| matches!(a.source, agent::AgentSource::Builtin))
        .collect();

    let mut agent_info = String::new();
    for a in &builtin_agents {
        let current_model = a.model.as_ref().map(|m| m.model_id.as_str()).unwrap_or("none");
        agent_info.push_str(&format!(
            "- {} ({:?}): {}, cost={:?}, current_model={}\n",
            a.name, a.mode, a.description, a.cost, current_model
        ));
    }

    let mut model_info = String::new();
    for (provider_id, models) in &fresh_models {
        for m in models {
            let name = m.name.as_deref().unwrap_or(&m.id);
            model_info.push_str(&format!("- provider={}, id={}, name={}\n", provider_id, m.id, name));
        }
    }

    let prompt = format!(
        "You are configuring an AI agent orchestration framework.\n\
         Each builtin agent needs the best available model assigned based on its role and cost tier.\n\n\
         ## Builtin Agents\n{agent_info}\n\
         ## Available Models\n{model_info}\n\
         Respond with ONLY a JSON object mapping agent names to their optimal model config.\n\
         Format: {{\"agent_name\": {{\"model\": \"model_id\", \"provider\": \"provider_id\"}}, ...}}\n\
         Rules:\n\
         - Expensive agents (oracle, orchestrator, planner, reviewer) should use the most capable model\n\
         - Cheap/free agents (worker, librarian, explore) should use fast, cost-effective models\n\
         - Only use models from the available list above\n\
         - Include ALL builtin agents in the response"
    );

    let resolved = harness
        .provider_registry
        .resolve_model(None, Some(provider::ModelCostTier::Medium))
        .ok_or_else(|| anyhow::anyhow!("no provider available for LLM call"))?;

    let provider = harness
        .provider_registry
        .get(&resolved.provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider not found: {}", resolved.provider_id))?;

    let request = CompletionRequest {
        model: resolved.model_id.clone(),
        system: vec![SystemMessage {
            content: "You are a helpful assistant. Respond only with valid JSON.".to_string(),
            cache_control: false,
        }],
        messages: vec![message::Message::user("req", prompt)],
        tools: vec![],
        temperature: Some(0.0),
        max_tokens: Some(2000),
    };

    let response = provider.complete(request).await?;
    let response_text = response.message.text();

    let json_str = extract_json(&response_text)
        .ok_or_else(|| anyhow::anyhow!("LLM returned invalid response:\n{response_text}"))?;

    let recommendations: std::collections::HashMap<String, AgentModelRecommendation> =
        serde_json::from_str(json_str).with_context(|| {
            format!("failed to parse LLM response as JSON:\n{response_text}")
        })?;

    let mut overrides = std::collections::HashMap::new();
    let builtin_names: std::collections::BTreeSet<_> =
        builtin_agents.iter().map(|a| a.name.as_str()).collect();

    for (name, rec) in &recommendations {
        if !builtin_names.contains(name.as_str()) {
            eprintln!("  ⚠ skipping unknown agent: {name}");
            continue;
        }
        println!("  {} → {} ({})", name, rec.model, rec.provider);
        overrides.insert(
            name.clone(),
            AgentOverride {
                model: Some(rec.model.clone()),
                provider: Some(rec.provider.clone()),
            },
        );
    }

    if overrides.is_empty() {
        println!("No valid recommendations. Nothing written.");
        return Ok(());
    }

    let workspace_root = std::env::current_dir()?;
    runtime::Harness::write_agent_overrides(&workspace_root, &overrides, global)?;

    let target = if global {
        "~/.config/omh/config.toml"
    } else {
        ".omh/config.toml"
    };
    println!("\nWrote agent overrides to {target}");
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct AgentModelRecommendation {
    model: String,
    provider: String,
}

fn has_model_changes(old: &ModelsCache, new: &ModelsCache) -> bool {
    if old.providers.len() != new.providers.len() {
        return true;
    }
    for (pid, new_entry) in &new.providers {
        match old.providers.get(pid) {
            None => return true,
            Some(old_entry) => {
                let old_ids: std::collections::BTreeSet<_> =
                    old_entry.models.iter().map(|m| &m.id).collect();
                let new_ids: std::collections::BTreeSet<_> =
                    new_entry.models.iter().map(|m| &m.id).collect();
                if old_ids != new_ids {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_json(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.starts_with('{') {
        return Some(text);
    }
    if let Some(start) = text.find("```json") {
        let start = start + 7;
        if let Some(end) = text[start..].find("```") {
            return Some(text[start..start + end].trim());
        }
    }
    if let Some(start) = text.find("```") {
        let start = start + 3;
        let inner = text[start..].trim_start();
        if inner.starts_with('{') {
            if let Some(end) = inner.find("```") {
                return Some(inner[..end].trim());
            }
        }
    }
    if let Some(start) = text.find('{') {
        let rest = &text[start..];
        let mut depth = 0;
        for (i, ch) in rest.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&rest[..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub async fn cmd_diagnose(session_id: &str) -> Result<()> {
    let harness = crate::init_harness()?;
    let dump_dir = harness.session_manager.dump_dir(session_id);

    if !dump_dir.exists() {
        bail!(
            "No dumps found for session {session_id}. Run with OMH_LOG=trace to enable dumps."
        );
    }

    let mut agents: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dump_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            agents.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    agents.sort();

    let mut total_anomalies = 0;

    for agent in &agents {
        let agent_dir = dump_dir.join(agent);
        let mut turn = 1u32;

        println!("═══ Agent: {} ═══", agent);

        loop {
            let request_path = agent_dir.join(format!("turn_{turn:03}_request.json"));
            let response_path = agent_dir.join(format!("turn_{turn:03}_response.json"));
            let tool_results_path = agent_dir.join(format!("turn_{turn:03}_tool_results.json"));

            if !request_path.exists() {
                break;
            }

            let request_text = std::fs::read_to_string(&request_path)?;
            let request: serde_json::Value = serde_json::from_str(&request_text)?;

            let model = request.get("model").and_then(|v| v.as_str()).unwrap_or("?");
            let msg_count = request
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);

            if response_path.exists() {
                let response_text = std::fs::read_to_string(&response_path)?;
                let response: serde_json::Value = serde_json::from_str(&response_text)?;
                let tool_calls = count_tool_calls(&response);
                let response_text_content = extract_text_content(&response);

                println!(
                    "  Turn {turn}: model={model} msgs={msg_count} tool_calls={tool_calls}"
                );

                let anomalies = check_turn_anomalies(
                    turn,
                    &request,
                    &response,
                    tool_results_path.exists().then(|| tool_results_path.as_path()),
                )?;

                for anomaly in &anomalies {
                    println!("    ⚠ {anomaly}");
                    total_anomalies += 1;
                }

                if !response_text_content.is_empty() {
                    let preview = if response_text_content.len() > 120 {
                        format!("{}...", &response_text_content[..120])
                    } else {
                        response_text_content.clone()
                    };
                    println!("    text: {preview}");
                }
            }

            turn += 1;
        }

        println!();
    }

    if total_anomalies == 0 {
        println!("✓ No anomalies detected.");
    } else {
        println!("⚠ {} anomaly(ies) detected.", total_anomalies);
    }

    Ok(())
}

fn count_tool_calls(response: &serde_json::Value) -> usize {
    response
        .get("parts")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .count()
        })
        .unwrap_or(0)
}

fn extract_text_content(response: &serde_json::Value) -> String {
    response
        .get("parts")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                        p.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn check_turn_anomalies(
    turn: u32,
    request: &serde_json::Value,
    response: &serde_json::Value,
    tool_results_path: Option<&std::path::Path>,
) -> Result<Vec<String>> {
    let mut anomalies = Vec::new();

    let messages = request
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut tool_result_data: Vec<(String, usize)> = Vec::new();
    for msg in &messages {
        if let Some(parts) = msg.get("parts").and_then(|v| v.as_array()) {
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    let content = part
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let is_error = part
                        .get("is_error")
                        .and_then(|e| e.as_bool())
                        .unwrap_or(false);
                    if !is_error && !content.is_empty() {
                        let id = part
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("?")
                            .to_string();
                        tool_result_data.push((id, content.len()));
                    }
                }
            }
        }
    }

    // Anomaly 1: model received non-empty tool_results but final text says "no files" / "inaccessible"
    let response_text = extract_text_content(response).to_lowercase();
    let negative_phrases = [
        "no usable file",
        "could not inspect",
        "inaccessible",
        "no readable",
        "no visible",
        "could not reliably",
        "did not return",
        "returned no",
        "no project files",
    ];

    let has_substantial_data = tool_result_data.iter().any(|(_, len)| *len > 100);
    let has_negative_conclusion = negative_phrases
        .iter()
        .any(|phrase| response_text.contains(phrase));

    if has_substantial_data && has_negative_conclusion {
        let total_bytes: usize = tool_result_data.iter().map(|(_, len)| *len).sum();
        anomalies.push(format!(
            "DATA_IGNORED: Model received {} bytes of tool_result data across {} results but concluded negatively",
            total_bytes,
            tool_result_data.len()
        ));
    }

    // Anomaly 2: model called the same tool with same args as a previous turn
    if turn > 1 {
        let response_tools = extract_tool_calls(response);
        let prev_tool_calls = extract_request_tool_calls(&messages);

        for (name, args) in &response_tools {
            for (prev_name, prev_args) in &prev_tool_calls {
                if name == prev_name && args == prev_args {
                    anomalies.push(format!(
                        "DUPLICATE_CALL: Model re-called {name} with identical args as a previous turn"
                    ));
                    break;
                }
            }
        }
    }

    // Anomaly 3: subagent spawn failure in tool_results
    if let Some(path) = tool_results_path {
        let tr_text = std::fs::read_to_string(path)?;
        let tr: serde_json::Value = serde_json::from_str(&tr_text)?;
        if let Some(parts) = tr.get("parts").and_then(|v| v.as_array()) {
            for part in parts {
                let is_error = part
                    .get("is_error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false);
                let content = part
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                if is_error && content.contains("token count") && content.contains("exceeds") {
                    anomalies.push(format!(
                        "TOKEN_OVERFLOW: Subagent failed with token limit: {}",
                        &content[..content.len().min(120)]
                    ));
                }
                if is_error && content.contains("failed") {
                    anomalies.push(format!(
                        "TOOL_ERROR: {}",
                        &content[..content.len().min(120)]
                    ));
                }
            }
        }
    }

    Ok(anomalies)
}

fn extract_tool_calls(response: &serde_json::Value) -> Vec<(String, String)> {
    response
        .get("parts")
        .and_then(|v| v.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                        let input = p
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_default();
                        Some((name.to_string(), input))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn extract_request_tool_calls(messages: &[serde_json::Value]) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    for msg in messages {
        if let Some(parts) = msg.get("parts").and_then(|v| v.as_array()) {
            for part in parts {
                if part.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = part.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let input = part
                        .get("input")
                        .map(|i| i.to_string())
                        .unwrap_or_default();
                    calls.push((name.to_string(), input));
                }
            }
        }
    }
    calls
}
