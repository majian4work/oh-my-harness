use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, bail};
use async_trait::async_trait;

use reqwest::{
    Client,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::ModelInfo;
use crate::openai_common::{
    self, OpenAIChatCompletionResponse, ResponsesApiResponse, build_chat_payload,
    build_responses_payload, parse_chat_completion_response, parse_responses_api_response,
    spawn_chat_stream, spawn_responses_stream,
};
use crate::{CompletionRequest, CompletionResponse, Provider, StreamResult};

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_CHAT_URL: &str = "https://api.githubcopilot.com/chat/completions";
const DEFAULT_RESPONSES_URL: &str = "https://api.githubcopilot.com/responses";

const USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
const EDITOR_VERSION: &str = "vscode/1.99.3";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
const OPENAI_INTENT: &str = "conversation-edits";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CopilotToken {
    token: String,
    expires_at: i64,
    endpoints: Option<CopilotEndpoints>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CopilotEndpoints {
    api: Option<String>,
}

pub struct CopilotProvider {
    oauth_token: String,
    client: Client,
    cached_token: Arc<RwLock<Option<CopilotToken>>>,
    model: String,
}

impl CopilotProvider {
    pub fn new(oauth_token: String, model: Option<String>) -> Self {
        Self {
            oauth_token,
            client: Client::new(),
            cached_token: Arc::new(RwLock::new(None)),
            model: model.unwrap_or_else(|| "gpt-4.1".to_string()),
        }
    }

    async fn get_copilot_token(&self) -> Result<CopilotToken> {
        let now = now_unix_seconds();

        if let Some(token) = self.cached_token.read().await.clone()
            && token.expires_at > now + 300
        {
            return Ok(token);
        }

        let response = self
            .client
            .get(TOKEN_EXCHANGE_URL)
            .headers(Self::copilot_headers())
            .header(AUTHORIZATION, format!("token {}", self.oauth_token))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "copilot token exchange failed with status {status}: {body}. Ensure the oauth token was issued for GitHub Copilot OAuth app {GITHUB_CLIENT_ID}"
            );
        }

        let token: CopilotToken = response.json().await?;
        if token.token.trim().is_empty() {
            bail!(
                "copilot token exchange returned an empty session token for GitHub Copilot OAuth app {GITHUB_CLIENT_ID}"
            );
        }

        *self.cached_token.write().await = Some(token.clone());
        Ok(token)
    }

    pub async fn get_session_token(&self) -> Result<String> {
        Ok(self.get_copilot_token().await?.token)
    }

    fn copilot_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
        headers.insert("editor-version", HeaderValue::from_static(EDITOR_VERSION));
        headers.insert(
            "editor-plugin-version",
            HeaderValue::from_static(EDITOR_PLUGIN_VERSION),
        );
        headers.insert(
            "copilot-integration-id",
            HeaderValue::from_static(COPILOT_INTEGRATION_ID),
        );
        headers.insert("openai-intent", HeaderValue::from_static(OPENAI_INTENT));
        headers
    }

    fn chat_url(&self, token: &CopilotToken) -> String {
        token
            .endpoints
            .as_ref()
            .and_then(|endpoints| endpoints.api.as_deref())
            .map(|api| format!("{}/chat/completions", api.trim_end_matches('/')))
            .unwrap_or_else(|| DEFAULT_CHAT_URL.to_string())
    }

    fn responses_url(&self, token: &CopilotToken) -> String {
        token
            .endpoints
            .as_ref()
            .and_then(|endpoints| endpoints.api.as_deref())
            .map(|api| format!("{}/responses", api.trim_end_matches('/')))
            .unwrap_or_else(|| DEFAULT_RESPONSES_URL.to_string())
    }

    /// GPT-5+ models (except gpt-5-mini) require the Responses API.
    fn needs_responses_api(model: &str) -> bool {
        openai_common::needs_responses_api(model)
    }

    fn effective_model(&self, request: &CompletionRequest) -> String {
        if request.model.trim().is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        }
    }

    fn chat_request_builder(&self, token: &CopilotToken) -> reqwest::RequestBuilder {
        self.client
            .post(self.chat_url(token))
            .headers(Self::copilot_headers())
            .bearer_auth(&token.token)
            .header(CONTENT_TYPE, "application/json")
            .header("x-initiator", "user")
    }

    fn responses_request_builder(&self, token: &CopilotToken) -> reqwest::RequestBuilder {
        self.client
            .post(self.responses_url(token))
            .headers(Self::copilot_headers())
            .bearer_auth(&token.token)
            .header(CONTENT_TYPE, "application/json")
            .header("x-initiator", "user")
    }

    async fn stream_responses(
        &self,
        token: &CopilotToken,
        request: &CompletionRequest,
    ) -> Result<StreamResult> {
        let model = self.effective_model(request);
        let payload = build_responses_payload(model, request, true);
        let response = self
            .responses_request_builder(token)
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if let Ok(req_json) = serde_json::to_string(&payload) {
                tracing::error!(status = %status, body = %body, request = %req_json, "copilot responses streaming request failed");
            }
            bail!("copilot responses streaming request failed with status {status}: {body}");
        }

        Ok(spawn_responses_stream(response))
    }

    async fn complete_responses(
        &self,
        token: &CopilotToken,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse> {
        let model = self.effective_model(request);
        let payload = build_responses_payload(model, request, false);
        let response = self
            .responses_request_builder(token)
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if let Ok(req_json) = serde_json::to_string(&payload) {
                tracing::error!(status = %status, body = %body, request = %req_json, "copilot responses request failed");
            }
            bail!("copilot responses request failed with status {status}: {body}");
        }

        let resp: ResponsesApiResponse = response.json().await?;
        Ok(parse_responses_api_response(resp, "copilot"))
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn name(&self) -> &str {
        "copilot"
    }

    fn default_model(&self) -> String {
        self.model.clone()
    }

    fn model_for_tier(&self, tier: crate::ModelCostTier) -> String {
        match tier {
            crate::ModelCostTier::Low => "gpt-4o-mini".to_string(),
            crate::ModelCostTier::Medium => "gpt-4.1".to_string(),
            crate::ModelCostTier::High => "claude-opus-4.6".to_string(),
        }
    }

    async fn stream(&self, request: CompletionRequest) -> Result<StreamResult> {
        let token = self.get_copilot_token().await?;
        let model = self.effective_model(&request);

        if Self::needs_responses_api(&model) {
            return self.stream_responses(&token, &request).await;
        }

        let payload = build_chat_payload(model, &request, true);
        let response = self
            .chat_request_builder(&token)
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if let Ok(req_json) = serde_json::to_string(&payload) {
                tracing::error!(status = %status, body = %body, request = %req_json, "copilot streaming request failed");
            }
            bail!("copilot streaming request failed with status {status}: {body}");
        }

        Ok(spawn_chat_stream(response))
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let token = self.get_copilot_token().await?;
        let model = self.effective_model(&request);

        if Self::needs_responses_api(&model) {
            return self.complete_responses(&token, &request).await;
        }

        let payload = build_chat_payload(model, &request, false);
        let response = self
            .chat_request_builder(&token)
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if let Ok(req_json) = serde_json::to_string(&payload) {
                tracing::error!(status = %status, body = %body, request = %req_json, "copilot completion request failed");
            }
            bail!("copilot completion request failed with status {status}: {body}");
        }

        let resp: OpenAIChatCompletionResponse = response.json().await?;
        parse_chat_completion_response(resp, "copilot")
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let token = self.get_copilot_token().await?;
        let url = self
            .chat_url(&token)
            .trim_end_matches("/chat/completions")
            .to_string()
            + "/models";
        let response = self
            .client
            .get(&url)
            .headers(Self::copilot_headers())
            .bearer_auth(&token.token)
            .send()
            .await?;

        if !response.status().is_success() {
            return Ok(vec![]);
        }

        let body: serde_json::Value = response.json().await?;
        let entries = if let Some(data) = body.get("data").and_then(|value| value.as_array()) {
            data.clone()
        } else if let Some(array) = body.as_array() {
            array.clone()
        } else {
            return Ok(vec![]);
        };

        let mut models: Vec<ModelInfo> = entries
            .into_iter()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str()?.to_string();
                Some(ModelInfo {
                    name: entry
                        .get("name")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string)
                        .or_else(|| Some(id.clone())),
                    id,
                    provider: self.name().to_string(),
                })
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_uses_expected_defaults() {
        let provider = CopilotProvider::new("gho_test".to_string(), None);

        assert_eq!(provider.oauth_token, "gho_test");
        assert_eq!(provider.model, "gpt-4.1");
        assert!(provider.cached_token.blocking_read().is_none());
    }

    #[test]
    fn copilot_headers_include_required_values() {
        let headers = CopilotProvider::copilot_headers();

        assert_eq!(headers.get("user-agent").unwrap(), USER_AGENT);
        assert_eq!(headers.get("editor-version").unwrap(), EDITOR_VERSION);
        assert_eq!(
            headers.get("editor-plugin-version").unwrap(),
            EDITOR_PLUGIN_VERSION
        );
        assert_eq!(
            headers.get("copilot-integration-id").unwrap(),
            COPILOT_INTEGRATION_ID
        );
        assert_eq!(headers.get("openai-intent").unwrap(), OPENAI_INTENT);
    }

    #[tokio::test]
    async fn get_session_token_reuses_valid_cached_token() {
        let provider = CopilotProvider::new("gho_test".to_string(), Some("gpt-5".to_string()));
        let cached = CopilotToken {
            token: "session-token".to_string(),
            expires_at: now_unix_seconds() + 600,
            endpoints: Some(CopilotEndpoints {
                api: Some("https://api.githubcopilot.com".to_string()),
            }),
        };

        *provider.cached_token.write().await = Some(cached);

        let token = provider.get_session_token().await.unwrap();

        assert_eq!(token, "session-token");
    }

    #[test]
    fn needs_responses_api_detects_gpt5_models() {
        assert!(CopilotProvider::needs_responses_api("gpt-5"));
        assert!(CopilotProvider::needs_responses_api("gpt-5.4"));
        assert!(CopilotProvider::needs_responses_api("gpt-5-latest"));
        assert!(!CopilotProvider::needs_responses_api("gpt-5-mini"));
        assert!(!CopilotProvider::needs_responses_api("gpt-4.1"));
        assert!(!CopilotProvider::needs_responses_api("claude-opus-4.6"));
        assert!(!CopilotProvider::needs_responses_api("o3"));
    }
}
