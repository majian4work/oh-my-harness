use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use provider::{
    anthropic::AnthropicProvider, copilot::CopilotProvider, openai_compat::OpenAICompatProvider,
};
use runtime::Harness;
use serde::Deserialize;
use tracing::Level;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub providers: HashMap<String, ProviderCredential>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderCredential {
    pub provider_type: ProviderType,
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    OpenAI,
    Anthropic,
    Copilot,
    Custom,
}

impl Credentials {
    pub fn global_path() -> PathBuf {
        dirs::cache_dir().join("credentials.json")
    }

    pub fn project_path(workspace_root: &std::path::Path) -> PathBuf {
        workspace_root.join(".omh/credentials.json")
    }

    fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn load() -> Result<Self> {
        let mut merged = Self::load_from(&Self::global_path())?;
        if let Ok(cwd) = std::env::current_dir() {
            let project = Self::load_from(&Self::project_path(&cwd))?;
            merged.providers.extend(project.providers);
        }
        Ok(merged)
    }
}

pub fn parse_log_level(log: &str) -> Level {
    match log.to_ascii_lowercase().as_str() {
        "error" => Level::ERROR,
        "warn" => Level::WARN,
        "info" => Level::INFO,
        "debug" => Level::DEBUG,
        "trace" => Level::TRACE,
        _ => Level::INFO,
    }
}

pub fn init_harness() -> Result<Harness> {
    let workspace_root: PathBuf =
        std::env::current_dir().context("failed to determine current directory")?;
    Harness::init(&workspace_root).with_context(|| {
        format!(
            "failed to initialize harness at {}",
            workspace_root.display()
        )
    })
}

pub fn register_providers_from_env(harness: &mut Harness) -> Result<()> {
    if let Ok(key) = env::var("OPENAI_API_KEY") {
        let base_url =
            env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".to_string());
        let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4.1".to_string());
        let provider = OpenAICompatProvider::new(reqwest::Client::new(), base_url, key, model);
        harness
            .provider_registry
            .register("openai", Box::new(provider));
    }

    if let Ok(key) = env::var("ANTHROPIC_API_KEY") {
        let model = env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-0".to_string());
        let provider = AnthropicProvider::new(reqwest::Client::new(), key, model);
        harness
            .provider_registry
            .register("anthropic", Box::new(provider));
    }

    let creds = Credentials::load().unwrap_or_default();
    for (name, cred) in &creds.providers {
        if harness.provider_registry.get(name).is_some() {
            continue;
        }

        match cred.provider_type {
            ProviderType::OpenAI | ProviderType::Custom => {
                let base_url = cred
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com".to_string());
                let model = cred.model.clone().unwrap_or_else(|| "gpt-4.1".to_string());
                let provider = OpenAICompatProvider::new(
                    reqwest::Client::new(),
                    base_url,
                    cred.api_key.clone(),
                    model,
                );
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
            ProviderType::Anthropic => {
                let model = cred
                    .model
                    .clone()
                    .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
                let provider =
                    AnthropicProvider::new(reqwest::Client::new(), cred.api_key.clone(), model);
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
            ProviderType::Copilot => {
                let provider = CopilotProvider::new(cred.api_key.clone(), cred.model.clone());
                harness
                    .provider_registry
                    .register(name.clone(), Box::new(provider));
            }
        }
    }

    Ok(())
}
