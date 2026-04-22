use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use provider::ModelInfo;
use serde::{Deserialize, Serialize};

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub providers: HashMap<String, ProviderCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveModel {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OmhConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_model: Option<ActiveModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredential {
    pub provider_type: ProviderType,
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    OpenAI,
    Anthropic,
    Copilot,
    Custom,
}

impl Credentials {
    pub fn global_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config/omh/credentials.json")
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
        serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn load() -> Result<Self> {
        let mut merged = Self::load_from(&Self::global_path())?;
        if let Ok(cwd) = std::env::current_dir() {
            let project = Self::load_from(&Self::project_path(&cwd))?;
            merged.providers.extend(project.providers);
        }
        Ok(merged)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::global_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(path, &content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn add(&mut self, name: String, cred: ProviderCredential) {
        self.providers.insert(name, cred);
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.providers.remove(name).is_some()
    }

    pub fn get(&self, name: &str) -> Option<&ProviderCredential> {
        self.providers.get(name)
    }
}

impl OmhConfig {
    pub fn global_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config/omh/config.toml")
    }

    pub fn project_path(workspace_root: &std::path::Path) -> PathBuf {
        workspace_root.join(".omh/config.toml")
    }

    fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn load() -> Result<Self> {
        let global = Self::load_from(&Self::global_path())?;
        if let Ok(cwd) = std::env::current_dir() {
            let project = Self::load_from(&Self::project_path(&cwd))?;
            Ok(Self {
                active_model: project.active_model.or(global.active_model),
            })
        } else {
            Ok(global)
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::global_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        fs::write(path, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderModelsEntry {
    pub cached_at: u64,
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelsCache {
    #[serde(default)]
    pub providers: HashMap<String, ProviderModelsEntry>,
}

const MODELS_CACHE_TTL_SECS: u64 = 86400;

impl ModelsCache {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config/omh/models_cache.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        if !path.exists() {
            return Self::default();
        }
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content)?;
        Ok(())
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn get_all(&self, provider_ids: &[&str]) -> Option<Vec<(String, Vec<ModelInfo>)>> {
        let now = Self::now();
        let mut result = Vec::new();
        for &pid in provider_ids {
            let entry = self.providers.get(pid)?;
            if now.saturating_sub(entry.cached_at) > MODELS_CACHE_TTL_SECS {
                return None;
            }
            if !entry.models.is_empty() {
                result.push((pid.to_string(), entry.models.clone()));
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Some(result)
    }

    pub fn update(&mut self, models: &[(String, Vec<ModelInfo>)]) {
        let now = Self::now();
        for (pid, model_list) in models {
            self.providers.insert(
                pid.clone(),
                ProviderModelsEntry {
                    cached_at: now,
                    models: model_list.clone(),
                },
            );
        }
    }
}

pub fn provider_type_for_name(name: &str) -> ProviderType {
    match name {
        "openai" => ProviderType::OpenAI,
        "anthropic" => ProviderType::Anthropic,
        "copilot" => ProviderType::Copilot,
        _ => ProviderType::Custom,
    }
}

pub fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        "****".to_string()
    } else {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    }
}

pub fn check_env_providers() -> Vec<(String, String)> {
    let mut found = Vec::new();
    if std::env::var("OPENAI_API_KEY").is_ok() {
        found.push(("openai".to_string(), "env OPENAI_API_KEY".to_string()));
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        found.push(("anthropic".to_string(), "env ANTHROPIC_API_KEY".to_string()));
    }
    if read_copilot_hosts_token().is_some() {
        found.push((
            "copilot".to_string(),
            "~/.config/github-copilot/hosts.json".to_string(),
        ));
    }
    found
}

/// Try to read existing OAuth token from GitHub Copilot config files.
pub fn read_copilot_hosts_token() -> Option<String> {
    let config_dir = copilot_config_dir()?;
    for filename in ["hosts.json", "apps.json"] {
        let path = config_dir.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Some(token) = extract_oauth_token(&content, "github.com")
        {
            return Some(token);
        }
    }
    None
}

fn copilot_config_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/github-copilot"))
}

fn extract_oauth_token(contents: &str, domain: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(contents).ok()?;
    let obj = value.as_object()?;
    obj.iter().find_map(|(key, val)| {
        if key.starts_with(domain) {
            val.get("oauth_token")?.as_str().map(ToString::to_string)
        } else {
            None
        }
    })
}

#[derive(Debug, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AccessTokenResponse {
    Success {
        access_token: String,
        token_type: String,
    },
    Pending {
        error: String,
    },
}

/// Start the GitHub OAuth device flow. Returns the device code response.
pub async fn start_device_flow(client: &reqwest::Client) -> Result<DeviceCodeResponse> {
    let response = client
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", GITHUB_CLIENT_ID), ("scope", "")])
        .send()
        .await?;

    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("GitHub device code request failed: {body}");
    }

    Ok(response.json().await?)
}

/// Poll GitHub for the access token until authorized or timeout.
pub async fn poll_for_access_token(
    client: &reqwest::Client,
    device_code: &str,
    interval: u64,
) -> Result<String> {
    let max_attempts = 60;
    let mut poll_interval = interval.max(1);

    for _ in 0..max_attempts {
        tokio::time::sleep(std::time::Duration::from_secs(poll_interval)).await;

        let response = client
            .post(ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", GITHUB_CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;

        let body = response.text().await?;
        match serde_json::from_str::<AccessTokenResponse>(&body)? {
            AccessTokenResponse::Success {
                access_token,
                token_type,
            } => {
                if token_type.eq_ignore_ascii_case("bearer") || token_type.is_empty() {
                    return Ok(access_token);
                }
                bail!("GitHub OAuth returned unexpected token type: {token_type}");
            }
            AccessTokenResponse::Pending { error } => match error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    poll_interval += 5;
                }
                _ => bail!("GitHub OAuth error: {body}"),
            },
        }
    }

    bail!("GitHub OAuth device flow timed out. Please try again.")
}

#[cfg(test)]
mod tests {
    use super::extract_oauth_token;

    #[test]
    fn extracts_oauth_token_for_matching_domain() {
        let token = extract_oauth_token(
            r#"{"github.com":{"oauth_token":"gho_test"},"example.com":{"oauth_token":"other"}}"#,
            "github.com",
        );

        assert_eq!(token.as_deref(), Some("gho_test"));
    }

    #[test]
    fn ignores_non_matching_domain() {
        let token = extract_oauth_token(
            r#"{"enterprise.github.com":{"oauth_token":"gho_test"}}"#,
            "github.com",
        );

        assert!(token.is_none());
    }

    #[test]
    fn extracts_oauth_token_for_prefixed_github_domain() {
        let token = extract_oauth_token(
            r#"{"github.com-enterprise":{"oauth_token":"gho_test"}}"#,
            "github.com",
        );

        assert_eq!(token.as_deref(), Some("gho_test"));
    }
}

pub fn configured_provider_names() -> Result<Vec<String>> {
    let creds = Credentials::load()?;
    let mut names = BTreeSet::new();

    for (name, _) in check_env_providers() {
        names.insert(name);
    }

    for name in creds.providers.keys() {
        names.insert(name.clone());
    }

    Ok(names.into_iter().collect())
}
