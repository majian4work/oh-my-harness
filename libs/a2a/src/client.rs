//! A2A HTTP client — discovers agents and sends tasks.

use anyhow::{Context, Result, bail};
use reqwest::Client;

use crate::types::*;

/// Client for communicating with A2A agents.
pub struct A2aClient {
    http: Client,
}

impl A2aClient {
    pub fn new() -> Self {
        Self {
            http: Client::new(),
        }
    }

    /// Fetch an agent's card from `{base_url}/.well-known/agent.json`.
    pub async fn fetch_agent_card(&self, base_url: &str) -> Result<AgentCard> {
        let url = format!(
            "{}/.well-known/agent.json",
            base_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to fetch agent card from {url}"))?;

        if !resp.status().is_success() {
            bail!(
                "agent card request failed: {} {}",
                resp.status(),
                url
            );
        }

        resp.json::<AgentCard>()
            .await
            .with_context(|| format!("failed to parse agent card from {url}"))
    }

    /// Send a task to a remote agent (JSON-RPC `tasks/send`).
    pub async fn send_task(
        &self,
        endpoint: &str,
        params: TaskSendParams,
    ) -> Result<Task> {
        self.rpc_call(endpoint, "tasks/send", &params).await
    }

    /// Get task status (JSON-RPC `tasks/get`).
    pub async fn get_task(
        &self,
        endpoint: &str,
        params: TaskQueryParams,
    ) -> Result<Task> {
        self.rpc_call(endpoint, "tasks/get", &params).await
    }

    /// Cancel a task (JSON-RPC `tasks/cancel`).
    pub async fn cancel_task(
        &self,
        endpoint: &str,
        params: TaskIdParams,
    ) -> Result<Task> {
        self.rpc_call(endpoint, "tasks/cancel", &params).await
    }

    /// Push-register our agent card with a remote peer.
    ///
    /// Posts to `{peer_base}/agents/register`. If the peer responds with
    /// its own card (bidirectional), returns it so we can register it locally.
    pub async fn register_with_peer(
        &self,
        peer_base: &str,
        registration: &AgentRegistration,
    ) -> Result<AgentRegistrationResponse> {
        let url = format!(
            "{}/agents/register",
            peer_base.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .json(registration)
            .send()
            .await
            .with_context(|| format!("failed to register with peer {url}"))?;

        if !resp.status().is_success() {
            bail!("peer registration failed: {} {}", resp.status(), url);
        }

        resp.json::<AgentRegistrationResponse>()
            .await
            .with_context(|| format!("failed to parse registration response from {url}"))
    }

    /// Low-level JSON-RPC call.
    async fn rpc_call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        method: &str,
        params: &P,
    ) -> Result<R> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::Value::Number(1.into())),
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };

        let resp = self
            .http
            .post(endpoint)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("A2A RPC {method} to {endpoint} failed"))?;

        if !resp.status().is_success() {
            bail!(
                "A2A RPC {method} returned {}",
                resp.status()
            );
        }

        let rpc_resp: JsonRpcResponse = resp
            .json()
            .await
            .context("failed to parse JSON-RPC response")?;

        if let Some(err) = rpc_resp.error {
            bail!("A2A RPC error {}: {}", err.code, err.message);
        }

        let result = rpc_resp
            .result
            .context("JSON-RPC response has no result")?;

        serde_json::from_value(result).context("failed to deserialize RPC result")
    }

    // ── Team endpoints ──────────────────────────────────────────────

    /// Join a team managed by the remote server.
    pub async fn team_join(
        &self,
        base_url: &str,
        request: &TeamJoinRequest,
    ) -> Result<TeamJoinResponse> {
        let url = format!("{}/team/join", base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .json(request)
            .send()
            .await
            .with_context(|| format!("team join to {url} failed"))?;

        if !resp.status().is_success() {
            bail!("team join returned {}", resp.status());
        }

        resp.json::<TeamJoinResponse>()
            .await
            .with_context(|| format!("failed to parse team join response from {url}"))
    }

    /// Leave a team.
    pub async fn team_leave(
        &self,
        base_url: &str,
        request: &TeamLeaveRequest,
    ) -> Result<()> {
        let url = format!("{}/team/leave", base_url.trim_end_matches('/'));
        let _ = self
            .http
            .post(&url)
            .json(request)
            .send()
            .await
            .with_context(|| format!("team leave to {url} failed"))?;
        Ok(())
    }

    /// Send heartbeat to the team.
    pub async fn team_heartbeat(
        &self,
        base_url: &str,
        request: &TeamHeartbeatRequest,
    ) -> Result<()> {
        let url = format!("{}/team/heartbeat", base_url.trim_end_matches('/'));
        let _ = self
            .http
            .post(&url)
            .json(request)
            .send()
            .await
            .with_context(|| format!("team heartbeat to {url} failed"))?;
        Ok(())
    }

    /// Get current team status.
    pub async fn team_status(&self, base_url: &str) -> Result<TeamStatusResponse> {
        let url = format!("{}/team/status", base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("team status from {url} failed"))?;

        if !resp.status().is_success() {
            bail!("team status returned {}", resp.status());
        }

        resp.json::<TeamStatusResponse>()
            .await
            .with_context(|| format!("failed to parse team status from {url}"))
    }

    /// Probe whether an omt endpoint is reachable (quick HEAD/GET check).
    pub async fn probe(&self, base_url: &str) -> bool {
        let url = format!(
            "{}/.well-known/agent.json",
            base_url.trim_end_matches('/')
        );
        self.http
            .get(&url)
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }
}

impl Default for A2aClient {
    fn default() -> Self {
        Self::new()
    }
}
