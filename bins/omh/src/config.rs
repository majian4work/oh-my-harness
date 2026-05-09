use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use runtime::Harness;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveModel {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OmhConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_model: Option<ActiveModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<provider::Effort>,
}

impl OmhConfig {
    pub fn global_path() -> PathBuf {
        dirs::config_dir().join("config.toml")
    }

    pub fn project_path(workspace_root: &Path) -> PathBuf {
        workspace_root.join(".omh/config.toml")
    }

    fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
    }

    /// Load config: project `.omh/config.toml` overrides global `~/.config/omh/config.toml`.
    pub fn load(workspace_root: &Path) -> Result<Self> {
        let global = Self::load_from(&Self::global_path())?;
        let project = Self::load_from(&Self::project_path(workspace_root))?;
        // Project takes precedence over global.
        Ok(Self {
            active_model: project.active_model.or(global.active_model),
            effort: project.effort.or(global.effort),
        })
    }
}

/// Resolved defaults for model and effort.
pub struct Defaults {
    pub provider_id: String,
    pub model_id: String,
    pub effort: provider::Effort,
}

/// Resolve active model and effort: config.toml → agent spec → provider default.
pub fn resolve_defaults(harness: &Harness, workspace_root: &Path) -> Defaults {
    let config = OmhConfig::load(workspace_root).unwrap_or_default();
    let effort = config.effort.unwrap_or_default();

    // 1. Config.toml active_model (project > global)
    if let Some(active) = &config.active_model {
        if harness.provider_registry.get(&active.provider_id).is_some() {
            return Defaults {
                provider_id: active.provider_id.clone(),
                model_id: active.model_id.clone(),
                effort,
            };
        }
    }

    // 2. Agent spec → provider registry resolve
    let agent_spec = harness
        .agent_registry
        .get("orchestrator")
        .and_then(|a| a.model.as_ref())
        .map(|m| provider::ModelSpec {
            model_id: m.model_id.clone(),
            provider_id: m.provider_id.clone(),
        });

    if let Some(resolved) = harness
        .provider_registry
        .resolve_model(agent_spec.as_ref(), None)
    {
        return Defaults {
            provider_id: resolved.provider_id,
            model_id: resolved.model_id,
            effort,
        };
    }

    Defaults {
        provider_id: String::new(),
        model_id: String::new(),
        effort,
    }
}
