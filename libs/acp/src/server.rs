use anyhow::Result;
use async_trait::async_trait;

use crate::types::*;

/// Trait that an agent must implement to be served via ACP
#[async_trait]
pub trait AcpAgent: Send + Sync {
    fn manifest(&self) -> AgentManifest;
    async fn run(
        &self,
        input: Vec<AcpMessage>,
        session_id: Option<&str>,
    ) -> Result<Vec<AcpMessage>>;
}

/// ACP server configuration
pub struct AcpServerConfig {
    pub bind: String,
}

/// ACP server (just the config/registry for now — actual HTTP serving needs axum which we'll add later)
pub struct AcpServer {
    config: AcpServerConfig,
    agents: Vec<Box<dyn AcpAgent>>,
}

impl AcpServer {
    pub fn new(config: AcpServerConfig) -> Self {
        Self {
            config,
            agents: Vec::new(),
        }
    }

    pub fn config(&self) -> &AcpServerConfig {
        &self.config
    }

    pub fn register(&mut self, agent: Box<dyn AcpAgent>) {
        self.agents.push(agent);
    }

    pub fn agents(&self) -> Vec<AgentManifest> {
        self.agents.iter().map(|agent| agent.manifest()).collect()
    }

    // pub async fn serve(self) -> Result<()>;  // TODO: needs axum
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAgent;

    #[async_trait]
    impl AcpAgent for TestAgent {
        fn manifest(&self) -> AgentManifest {
            AgentManifest {
                name: "tester".into(),
                description: "Test agent".into(),
                input_content_types: vec!["text/plain".into()],
                output_content_types: vec!["text/plain".into()],
                metadata: serde_json::Value::Null,
            }
        }

        async fn run(
            &self,
            _input: Vec<AcpMessage>,
            _session_id: Option<&str>,
        ) -> Result<Vec<AcpMessage>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn server_registers_agents_and_lists_manifests() {
        let mut server = AcpServer::new(AcpServerConfig {
            bind: "127.0.0.1:3001".into(),
        });

        server.register(Box::new(TestAgent));

        let manifests = server.agents();
        assert_eq!(server.config().bind, "127.0.0.1:3001");
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "tester");
    }
}
