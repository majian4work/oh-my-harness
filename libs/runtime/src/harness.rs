use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent::AgentRegistry;
use anyhow::Result;
use bus::{AgentEvent, EventBus, McpServerStatus};
use evolution::{EvolutionEngine, EvolutionPolicy};
use hook::HookRunner;
use hook::builtins::{AuditTrailHook, ErrorTrackerHook, PermissionGuardHook};
use mcp::{McpClient, McpToolBridge, McpToolProxy, McpTransport};
use memory::{MarkdownMemoryStore, MemoryStore};
use provider::ProviderRegistry;
use serde::{Deserialize, Serialize};
use session::SessionManager;
use skill::{SkillRegistry, SkillTool};
use tool::{ToolRegistry, builtins};

use crate::background::BackgroundTaskManager;
use crate::spawn_tool::SpawnAgentTool;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentOverride {
    pub model: Option<String>,
    pub provider: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ProjectConfig {
    #[serde(default)]
    mcp: HashMap<String, McpServerConfig>,
    #[serde(default)]
    agents: HashMap<String, AgentOverride>,
}

#[derive(Debug, Deserialize)]
struct McpServerConfig {
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

pub struct Harness {
    pub agent_registry: AgentRegistry,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub provider_registry: ProviderRegistry,
    pub session_manager: SessionManager,
    pub bus: EventBus,
    pub hook_runner: HookRunner,
    pub memory: Arc<dyn MemoryStore>,
    pub evolution: EvolutionEngine,
    pub background_tasks: BackgroundTaskManager,
    pub mcp_bridge: Mutex<McpToolBridge>,
    pub mcp_statuses: Mutex<Vec<McpServerStatus>>,
    pub agent_overrides: HashMap<String, AgentOverride>,
}

impl Harness {
    pub fn init(workspace_root: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_sessions_dir(workspace_root, dirs::sessions_dir())
    }

    pub fn init_with_sessions_dir(
        workspace_root: impl AsRef<Path>,
        sessions_dir: PathBuf,
    ) -> Result<Self> {
        let workspace_root = workspace_root.as_ref();
        let harness_dir = workspace_root.join(".omh");

        std::fs::create_dir_all(&harness_dir)?;

        let session_manager = SessionManager::new(&sessions_dir)?;
        let memory: Arc<dyn MemoryStore> = Arc::new(MarkdownMemoryStore::open(workspace_root)?);
        let evolution = EvolutionEngine::new(Arc::clone(&memory), EvolutionPolicy::default());
        let agent_registry = AgentRegistry::load(workspace_root)?;
        let skill_registry = Arc::new(SkillRegistry::load(workspace_root)?);

        let tool_registry = ToolRegistry::new();
        builtins::register_builtins(&tool_registry);
        tool_registry.register(Box::new(SpawnAgentTool));
        tool_registry.register(Box::new(SkillTool::new(Arc::clone(&skill_registry))));
        let tool_registry = Arc::new(tool_registry);

        let bus = EventBus::new(256);

        let (mcp_bridge, mcp_statuses) = (Mutex::new(McpToolBridge::new()), Mutex::new(Vec::new()));

        let agent_overrides = Self::load_agent_overrides(workspace_root);

        let mut hook_runner = HookRunner::new();
        hook_runner.register(Box::new(PermissionGuardHook::new(
            agent_registry.clone(),
            Arc::clone(&tool_registry),
        )));
        hook_runner.register(Box::new(ErrorTrackerHook::new(5)));
        for audit_hook in AuditTrailHook::all() {
            hook_runner.register(Box::new(audit_hook));
        }

        Ok(Self {
            agent_registry,
            skill_registry,
            tool_registry,
            provider_registry: ProviderRegistry::new(),
            session_manager,
            background_tasks: BackgroundTaskManager::new(8, bus.clone()),
            bus,
            hook_runner,
            memory,
            evolution,
            mcp_bridge,
            mcp_statuses,
            agent_overrides,
        })
    }

    pub fn connect_mcp_servers(&self, workspace_root: &Path) {
        let (bridge, statuses) = Self::load_mcp_servers(workspace_root, &self.tool_registry);

        {
            let mut current = self.mcp_bridge.lock().unwrap();
            for client in bridge.into_clients() {
                current.add_client(client);
            }
        }

        if !statuses.is_empty() {
            let mut current = self.mcp_statuses.lock().unwrap();
            *current = statuses.clone();
            self.bus
                .publish(AgentEvent::McpServersChanged { servers: statuses });
        }
    }

    fn load_agent_overrides(workspace_root: &Path) -> HashMap<String, AgentOverride> {
        let mut overrides = HashMap::new();

        {
            let global_path = dirs::config_dir().join("config.toml");
            if let Ok(content) = std::fs::read_to_string(&global_path) {
                if let Ok(config) = toml::from_str::<ProjectConfig>(&content) {
                    overrides.extend(config.agents);
                }
            }
        }

        let project_path = workspace_root.join(".omh/config.toml");
        if let Ok(content) = std::fs::read_to_string(&project_path) {
            if let Ok(config) = toml::from_str::<ProjectConfig>(&content) {
                overrides.extend(config.agents);
            }
        }

        overrides
    }

    pub fn write_agent_overrides(
        workspace_root: &Path,
        overrides: &HashMap<String, AgentOverride>,
        global: bool,
    ) -> Result<()> {
        let config_path = if global {
            dirs::config_dir().join("config.toml")
        } else {
            workspace_root.join(".omh/config.toml")
        };

        let mut config: toml::Value = if let Ok(content) = std::fs::read_to_string(&config_path) {
            toml::from_str(&content).unwrap_or(toml::Value::Table(Default::default()))
        } else {
            toml::Value::Table(Default::default())
        };

        let mut agents_table = toml::map::Map::new();
        for (name, ov) in overrides {
            let mut entry = toml::map::Map::new();
            if let Some(model) = &ov.model {
                entry.insert("model".to_string(), toml::Value::String(model.clone()));
            }
            if let Some(provider) = &ov.provider {
                entry.insert(
                    "provider".to_string(),
                    toml::Value::String(provider.clone()),
                );
            }
            agents_table.insert(name.clone(), toml::Value::Table(entry));
        }

        if let Some(table) = config.as_table_mut() {
            table.insert("agents".to_string(), toml::Value::Table(agents_table));
        }

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(&config)?;
        std::fs::write(&config_path, content)?;
        Ok(())
    }

    fn builtin_mcp_servers() -> HashMap<String, McpServerConfig> {
        let mut servers = HashMap::new();
        servers.insert(
            "context7".to_string(),
            McpServerConfig {
                command: String::new(),
                args: Vec::new(),
                env: HashMap::new(),
                url: Some("https://mcp.context7.com/mcp".to_string()),
                headers: HashMap::new(),
                enabled: true,
            },
        );
        servers.insert(
            "exa".to_string(),
            McpServerConfig {
                command: String::new(),
                args: Vec::new(),
                env: HashMap::new(),
                url: Some("https://mcp.exa.ai/mcp".to_string()),
                headers: HashMap::new(),
                enabled: true,
            },
        );
        servers
    }

    fn load_mcp_servers(
        workspace_root: &Path,
        tool_registry: &ToolRegistry,
    ) -> (McpToolBridge, Vec<McpServerStatus>) {
        let mut merged_mcp = Self::builtin_mcp_servers();

        {
            let global_path = dirs::config_dir().join("config.toml");
            if let Ok(content) = std::fs::read_to_string(&global_path) {
                if let Ok(config) = toml::from_str::<ProjectConfig>(&content) {
                    merged_mcp.extend(config.mcp);
                }
            }
        }

        let project_path = workspace_root.join(".omh/config.toml");
        if let Ok(content) = std::fs::read_to_string(&project_path) {
            if let Ok(config) = toml::from_str::<ProjectConfig>(&content) {
                merged_mcp.extend(config.mcp);
            }
        }

        let mut bridge = McpToolBridge::new();
        let mut statuses = Vec::new();

        if merged_mcp.is_empty() {
            return (bridge, statuses);
        }

        for (name, server_config) in &merged_mcp {
            if !server_config.enabled {
                tracing::info!(mcp = %name, "MCP server disabled by config");
                continue;
            }
            let transport = if let Some(url) = &server_config.url {
                McpTransport::StreamableHttp {
                    uri: url.clone(),
                    headers: server_config.headers.clone(),
                }
            } else {
                McpTransport::Stdio {
                    command: server_config.command.clone(),
                    args: server_config.args.clone(),
                    env: server_config.env.clone(),
                }
            };

            match McpClient::connect(transport) {
                Ok(client) => {
                    let tools_count = match client.list_tools() {
                        Ok(tools) => {
                            for spec in &tools {
                                let tool_client = client.clone();
                                let tool_spec = spec.clone();
                                tool_registry
                                    .register(Box::new(McpToolProxy::new(tool_client, tool_spec)));
                            }
                            tools.len()
                        }
                        Err(_) => 0,
                    };
                    bridge.add_client(client);
                    statuses.push(McpServerStatus {
                        name: name.clone(),
                        status: "connected".to_string(),
                        tools_count,
                    });
                }
                Err(e) => {
                    tracing::warn!("MCP server '{}' failed to connect: {}", name, e);
                    statuses.push(McpServerStatus {
                        name: name.clone(),
                        status: format!("error: {e}"),
                        tools_count: 0,
                    });
                }
            }
        }

        (bridge, statuses)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn init_creates_valid_harness() {
        let temp_dir = unique_temp_dir();
        let harness = Harness::init(&temp_dir).unwrap();

        assert!(dirs::sessions_dir().exists());
        assert!(temp_dir.join(".omh/memory").exists());
        assert!(harness.agent_registry.get("orchestrator").is_some());
        assert!(harness.skill_registry.get("update-best-models").is_some());
        assert!(harness.tool_registry.get_spec("skill").is_some());
        assert!(harness.tool_registry.get_spec("spawn_agent").is_some());
        assert!(harness.provider_registry.list().is_empty());

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    fn unique_temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("runtime-harness-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(Path::new(&path)).unwrap();
        path
    }
}
