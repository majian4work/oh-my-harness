use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Serialize, de::DeserializeOwned};

use crate::types::*;

pub struct AcpClient {
    base_url: String,
    client: Client,
}

#[derive(Serialize)]
struct ResumeRunRequest {
    input: Vec<AcpMessage>,
}

impl AcpClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client: Client::new(),
        }
    }

    /// Health check
    pub async fn ping(&self) -> Result<()> {
        self.client
            .get(self.endpoint("/ping"))
            .send()
            .await
            .context("failed to send ACP ping request")?
            .error_for_status()
            .context("ACP ping request failed")?;

        Ok(())
    }

    /// List available agents
    pub async fn list_agents(&self) -> Result<Vec<AgentManifest>> {
        self.send_json(self.client.get(self.endpoint("/agents")))
            .await
    }

    /// Get a specific agent's manifest
    pub async fn get_agent(&self, name: &str) -> Result<AgentManifest> {
        self.send_json(self.client.get(self.endpoint(&format!("/agents/{name}"))))
            .await
    }

    /// Create a run (sync mode — blocks until complete)
    pub async fn run_sync(&self, mut request: RunCreateRequest) -> Result<Run> {
        request.mode = RunMode::Sync;
        self.send_json(self.client.post(self.endpoint("/runs")).json(&request))
            .await
    }

    /// Create a run (async mode — returns immediately with run_id)
    pub async fn run_async(&self, mut request: RunCreateRequest) -> Result<Run> {
        request.mode = RunMode::Async;
        self.send_json(self.client.post(self.endpoint("/runs")).json(&request))
            .await
    }

    /// Poll run status
    pub async fn get_run(&self, run_id: &str) -> Result<Run> {
        self.send_json(self.client.get(self.endpoint(&format!("/runs/{run_id}"))))
            .await
    }

    /// Cancel a run
    pub async fn cancel_run(&self, run_id: &str) -> Result<Run> {
        self.send_json(
            self.client
                .post(self.endpoint(&format!("/runs/{run_id}/cancel"))),
        )
        .await
    }

    /// Resume a paused (awaiting) run
    pub async fn resume_run(&self, run_id: &str, input: Vec<AcpMessage>) -> Result<Run> {
        self.send_json(
            self.client
                .post(self.endpoint(&format!("/runs/{run_id}")))
                .json(&ResumeRunRequest { input }),
        )
        .await
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    async fn send_json<T>(&self, request: reqwest::RequestBuilder) -> Result<T>
    where
        T: DeserializeOwned,
    {
        request
            .send()
            .await
            .context("failed to send ACP request")?
            .error_for_status()
            .context("ACP request failed")?
            .json()
            .await
            .context("failed to deserialize ACP response")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_client_new_creates_instance() {
        let client = AcpClient::new("http://localhost:3000/");

        assert_eq!(client.base_url, "http://localhost:3000");
    }
}
