//! Agent registry — tracks known A2A agents for service discovery.
//!
//! Stores agent cards with their endpoint URLs, persists to a JSON file,
//! and provides lookup by name or skill tags.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::client::A2aClient;
use crate::types::{AgentCard, AgentRegistration};

/// A registered agent entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub card: AgentCard,
    /// The base URL where this agent is reachable.
    pub endpoint: String,
    pub registered_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
}

/// Persistent registry of known A2A agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryData {
    agents: HashMap<String, AgentEntry>,
}

pub struct AgentRegistry {
    data: RegistryData,
    persist_path: PathBuf,
    client: A2aClient,
}

impl AgentRegistry {
    /// Load the registry from a JSON file, or create an empty one.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let persist_path = path.into();
        let data = if persist_path.exists() {
            let content = std::fs::read_to_string(&persist_path)
                .with_context(|| format!("failed to read registry: {}", persist_path.display()))?;
            serde_json::from_str(&content)
                .with_context(|| format!("failed to parse registry: {}", persist_path.display()))?
        } else {
            RegistryData {
                agents: HashMap::new(),
            }
        };

        Ok(Self {
            data,
            persist_path,
            client: A2aClient::new(),
        })
    }

    /// Discover an agent at the given endpoint URL and register it.
    ///
    /// Fetches `{endpoint}/.well-known/agent.json`, stores the card,
    /// and persists the registry.
    pub async fn register(&mut self, endpoint: &str) -> Result<&AgentEntry> {
        let card = self
            .client
            .fetch_agent_card(endpoint)
            .await
            .with_context(|| format!("failed to discover agent at {endpoint}"))?;

        let name = card.name.clone();
        let entry = AgentEntry {
            card,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            registered_at: Utc::now(),
            last_seen: Some(Utc::now()),
        };

        self.data.agents.insert(name.clone(), entry);
        self.save()?;

        Ok(self.data.agents.get(&name).unwrap())
    }

    /// Register an agent from a known card (no discovery needed).
    pub fn register_local(&mut self, card: AgentCard, endpoint: &str) -> Result<()> {
        let name = card.name.clone();
        let entry = AgentEntry {
            card,
            endpoint: endpoint.to_string(),
            registered_at: Utc::now(),
            last_seen: Some(Utc::now()),
        };
        self.data.agents.insert(name, entry);
        self.save()
    }

    /// Unregister an agent by name.
    pub fn unregister(&mut self, name: &str) -> Result<bool> {
        let removed = self.data.agents.remove(name).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Get an agent by name.
    pub fn get(&self, name: &str) -> Option<&AgentEntry> {
        self.data.agents.get(name)
    }

    /// List all registered agents.
    pub fn list(&self) -> Vec<&AgentEntry> {
        self.data.agents.values().collect()
    }

    /// Find agents whose skills match any of the given tags.
    pub fn discover_by_tags(&self, tags: &[&str]) -> Vec<&AgentEntry> {
        self.data
            .agents
            .values()
            .filter(|entry| {
                entry.card.skills.iter().any(|skill| {
                    skill.tags.iter().any(|t| tags.contains(&t.as_str()))
                })
            })
            .collect()
    }

    /// Find agents whose skills match a keyword in name or description.
    pub fn discover_by_keyword(&self, keyword: &str) -> Vec<&AgentEntry> {
        let kw = keyword.to_lowercase();
        self.data
            .agents
            .values()
            .filter(|entry| {
                entry.card.name.to_lowercase().contains(&kw)
                    || entry
                        .card
                        .description
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&kw)
                    || entry.card.skills.iter().any(|s| {
                        s.name.to_lowercase().contains(&kw)
                            || s.description
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&kw)
                    })
            })
            .collect()
    }

    /// Refresh an agent's card by re-fetching it.
    pub async fn refresh(&mut self, name: &str) -> Result<()> {
        let endpoint = self
            .data
            .agents
            .get(name)
            .map(|e| e.endpoint.clone())
            .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found"))?;

        let card = self.client.fetch_agent_card(&endpoint).await?;

        if let Some(entry) = self.data.agents.get_mut(name) {
            entry.card = card;
            entry.last_seen = Some(Utc::now());
        }

        self.save()
    }

    /// Check if an agent is reachable by fetching its card.
    pub async fn health_check(&self, name: &str) -> Result<bool> {
        let entry = self
            .data
            .agents
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("agent '{name}' not found"))?;

        match self.client.fetch_agent_card(&entry.endpoint).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Return all registered peer endpoints (excluding a given self-name).
    pub fn peer_endpoints(&self, exclude: &str) -> Vec<String> {
        self.data
            .agents
            .iter()
            .filter(|(name, _)| name.as_str() != exclude)
            .map(|(_, e)| e.endpoint.clone())
            .collect()
    }

    /// Announce our agent card to all known peers via `POST /agents/register`.
    ///
    /// If a peer responds with its own card (bidirectional), we register it too.
    /// Errors on individual peers are logged but don't fail the whole operation.
    pub async fn announce_to_peers(&mut self, our_card: &AgentCard, our_endpoint: &str) -> Result<()> {
        let registration = AgentRegistration {
            card: our_card.clone(),
            endpoint: our_endpoint.to_string(),
        };

        let peers = self.peer_endpoints(&our_card.name);
        if peers.is_empty() {
            return Ok(());
        }

        for peer_endpoint in &peers {
            match self.client.register_with_peer(peer_endpoint, &registration).await {
                Ok(resp) => {
                    tracing::info!("announced to {peer_endpoint}: accepted={}", resp.accepted);
                    // Bidirectional: if peer sent its card back, register it.
                    if let (Some(card), Some(ep)) = (resp.peer_card, resp.peer_endpoint) {
                        let name = card.name.clone();
                        let entry = AgentEntry {
                            card,
                            endpoint: ep,
                            registered_at: Utc::now(),
                            last_seen: Some(Utc::now()),
                        };
                        self.data.agents.insert(name, entry);
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to announce to {peer_endpoint}: {e:#}");
                }
            }
        }

        self.save()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.persist_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.data)?;
        std::fs::write(&self.persist_path, json)
            .with_context(|| format!("failed to save registry: {}", self.persist_path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn test_card(name: &str, tags: &[&str]) -> AgentCard {
        AgentCard {
            name: name.to_string(),
            description: Some(format!("{name} agent")),
            url: format!("http://localhost/{name}"),
            provider: None,
            version: "1.0".to_string(),
            capabilities: AgentCapabilities::default(),
            skills: vec![AgentSkill {
                id: "s1".to_string(),
                name: format!("{name}-skill"),
                description: None,
                tags: tags.iter().map(|t| t.to_string()).collect(),
                examples: vec![],
            }],
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
        }
    }

    #[test]
    fn register_and_discover() {
        let tmp = std::env::temp_dir().join("a2a_test_registry.json");
        let _ = std::fs::remove_file(&tmp);

        let mut reg = AgentRegistry::load(&tmp).unwrap();
        reg.register_local(test_card("coder", &["coding", "rust"]), "http://localhost:8001")
            .unwrap();
        reg.register_local(test_card("reviewer", &["review", "qa"]), "http://localhost:8002")
            .unwrap();
        reg.register_local(test_card("deployer", &["deploy", "infra"]), "http://localhost:8003")
            .unwrap();

        assert_eq!(reg.list().len(), 3);

        // Discover by tags
        let coders = reg.discover_by_tags(&["coding"]);
        assert_eq!(coders.len(), 1);
        assert_eq!(coders[0].card.name, "coder");

        // Discover by keyword
        let found = reg.discover_by_keyword("review");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].card.name, "reviewer");

        // Persistence
        let reg2 = AgentRegistry::load(&tmp).unwrap();
        assert_eq!(reg2.list().len(), 3);

        // Unregister
        let mut reg2 = reg2;
        assert!(reg2.unregister("deployer").unwrap());
        assert_eq!(reg2.list().len(), 2);
        assert!(!reg2.unregister("nonexistent").unwrap());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn get_agent_by_name() {
        let tmp = std::env::temp_dir().join("a2a_test_get.json");
        let _ = std::fs::remove_file(&tmp);

        let mut reg = AgentRegistry::load(&tmp).unwrap();
        reg.register_local(test_card("alpha", &[]), "http://a").unwrap();

        assert!(reg.get("alpha").is_some());
        assert!(reg.get("beta").is_none());

        let _ = std::fs::remove_file(&tmp);
    }
}
