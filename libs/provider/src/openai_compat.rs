use std::env;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use tracing::info;

use crate::openai_common::{
    OpenAIChatCompletionResponse, ResponsesApiResponse, build_chat_payload,
    build_responses_payload, needs_responses_api, parse_chat_completion_response,
    parse_responses_api_response, spawn_chat_stream, spawn_responses_stream,
};
use crate::{CompletionRequest, CompletionResponse, ModelInfo, Provider, StreamResult};

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";

#[derive(Debug, Clone)]
pub struct OpenAICompatProvider {
    pub client: Client,
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
}

impl OpenAICompatProvider {
    pub fn new(
        client: Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        let provider = Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_model: default_model.into(),
        };

        info!(
            base_url = %provider.base_url,
            default_model = %provider.default_model,
            "initialized openai-compatible provider"
        );

        provider
    }

    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is not set")?;
        let base_url =
            env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_OPENAI_BASE_URL.to_string());

        Ok(Self::new(Client::new(), base_url, api_key, default_model))
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    fn responses_endpoint(&self) -> String {
        format!("{}/v1/responses", self.base_url.trim_end_matches('/'))
    }

    fn effective_model(&self, request: &CompletionRequest) -> String {
        if request.model.trim().is_empty() {
            self.default_model.clone()
        } else {
            request.model.clone()
        }
    }

    fn request_builder(&self, url: &str) -> RequestBuilder {
        self.client
            .post(url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
    }
}

#[async_trait]
impl Provider for OpenAICompatProvider {
    fn name(&self) -> &str {
        "openai_compat"
    }

    fn default_model(&self) -> String {
        self.default_model.clone()
    }

    fn model_for_tier(&self, tier: crate::ModelCostTier) -> String {
        match tier {
            crate::ModelCostTier::Low => "gpt-4o-mini".to_string(),
            crate::ModelCostTier::Medium => "gpt-4.1".to_string(),
            crate::ModelCostTier::High => "o3".to_string(),
        }
    }

    async fn stream(&self, request: CompletionRequest) -> Result<StreamResult> {
        let model = self.effective_model(&request);

        if needs_responses_api(&model) {
            let payload = build_responses_payload(model, &request, true);
            let response = self
                .request_builder(&self.responses_endpoint())
                .json(&payload)
                .send()
                .await?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                bail!(
                    "openai-compatible responses streaming request failed with status {status}: {body}"
                );
            }

            return Ok(spawn_responses_stream(response));
        }

        let payload = build_chat_payload(model, &request, true);
        let response = self
            .request_builder(&self.endpoint())
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("openai-compatible streaming request failed with status {status}: {body}");
        }

        Ok(spawn_chat_stream(response))
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let model = self.effective_model(&request);

        if needs_responses_api(&model) {
            let payload = build_responses_payload(model, &request, false);
            let response = self
                .request_builder(&self.responses_endpoint())
                .json(&payload)
                .send()
                .await?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                bail!("openai-compatible responses request failed with status {status}: {body}");
            }

            let resp: ResponsesApiResponse = response.json().await?;
            return Ok(parse_responses_api_response(resp, "openai-compat"));
        }

        let payload = build_chat_payload(model, &request, false);
        let response = self
            .request_builder(&self.endpoint())
            .json(&payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("openai-compatible completion request failed with status {status}: {body}");
        }

        let resp: OpenAIChatCompletionResponse = response.json().await?;
        parse_chat_completion_response(resp, "openai-compat")
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/v1/models", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            return Ok(vec![]);
        }

        #[derive(Deserialize)]
        struct ModelsResponse {
            data: Vec<ModelEntry>,
        }

        #[derive(Deserialize)]
        struct ModelEntry {
            id: String,
        }

        let body: ModelsResponse = response.json().await?;
        let mut models: Vec<ModelInfo> = body
            .data
            .into_iter()
            .map(|model| ModelInfo {
                name: Some(model.id.clone()),
                id: model.id,
                provider: self.name().to_string(),
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}
