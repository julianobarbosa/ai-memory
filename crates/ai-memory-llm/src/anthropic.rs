//! Anthropic Messages API client.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited};
use crate::types::{ChatRequest, ChatResponse, Role, Usage};

/// Default Anthropic API base.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Pinned Anthropic API version header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `anthropic-beta` header sent on OAuth (subscription) requests. Mirrors
/// the Claude Code OAuth handshake: the `oauth-2025-04-20` feature is what
/// authorises a subscription bearer token against /v1/messages, and the
/// `claude-code-*` feature matches what the official CLI sends. Values
/// cross-checked against oh-my-pi's `claudeCodeBetaDefaults`.
///
/// NOTE: this header combination is derived from Claude Code's documented OAuth
/// handshake and should be smoke-tested with a real `claude setup-token` token
/// before use in production, as Anthropic may update the required beta values.
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20,claude-code-20250219";

/// Authentication mode for the Anthropic provider.
#[derive(Clone)]
enum AnthropicAuth {
    /// Static API key sent as `x-api-key`.
    ApiKey(SecretString),
    /// OAuth bearer token from a Claude Pro/Max subscription
    /// (obtained via `claude setup-token`).
    OAuth(SecretString),
}

/// Anthropic Messages-API-backed provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    auth: AnthropicAuth,
    base_url: String,
    model: String,
}

impl AnthropicProvider {
    /// Construct a provider given an API key and model id.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the underlying HTTP client cannot
    /// be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        // 300s matches the OpenAI/openai-compat client — same reason:
        // first request after a model swap on a local inference server
        // (Ollama, llama-swap, vLLM) can take 30-90s of cold-load.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::ApiKey(api_key),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
        })
    }

    /// Construct a provider using an OAuth subscription token from
    /// `claude setup-token` (Claude Pro/Max subscription). Hits the same
    /// `/v1/messages` endpoint as `new`, but uses a Bearer token and the
    /// `anthropic-beta: oauth-2025-04-20,claude-code-20250219` header
    /// instead of `x-api-key`.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the underlying HTTP client cannot
    /// be built.
    pub fn new_oauth(token: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::OAuth(token),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
        })
    }

    /// Override the API base URL (mostly for tests against wiremock).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<AnthropicMsg<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Debug, Serialize)]
struct AnthropicMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Tool { name: String },
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    model: String,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text { text: String },
    ToolUse { input: serde_json::Value },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let body = self.build_request(&request, None);
        let response: AnthropicResponse = self.post(&body).await?;
        let text = response
            .content
            .iter()
            .filter_map(|c| match c {
                AnthropicContent::Text { text } => Some(text.as_str()),
                AnthropicContent::ToolUse { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ChatResponse {
            text,
            usage: response.usage.map(|u| Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
            }),
            model: response.model,
        })
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let body = self.build_request(&request, Some(schema));
        let response: AnthropicResponse = self.post(&body).await?;
        for c in response.content {
            if let AnthropicContent::ToolUse { input, .. } = c {
                return Ok(input);
            }
        }
        Err(LlmError::UnexpectedShape(
            "anthropic response had no tool_use block".into(),
        ))
    }
}

impl AnthropicProvider {
    /// Build the `/v1/messages` body shared by `complete` and
    /// `complete_structured_raw`. Passing a schema turns the call into the
    /// forced-tool structured-output shape. Both paths go through here so the
    /// temperature rule below can't apply to one and silently miss the other.
    fn build_request<'a>(
        &'a self,
        request: &'a ChatRequest,
        schema: Option<serde_json::Value>,
    ) -> AnthropicRequest<'a> {
        let messages: Vec<AnthropicMsg<'a>> = request
            .messages
            .iter()
            .map(|m| AnthropicMsg {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            })
            .collect();
        let (tools, tool_choice) = match schema {
            Some(input_schema) => (
                Some(vec![AnthropicTool {
                    name: "result".into(),
                    description: "Emit the structured result.".into(),
                    input_schema,
                }]),
                Some(AnthropicToolChoice::Tool {
                    name: "result".into(),
                }),
            ),
            None => (None, None),
        };
        // Newer models reject ai-memory's non-default `temperature` with a
        // 400; omit the field so the API applies its own default.
        let temperature = if model_rejects_temperature(&self.model) {
            None
        } else {
            request.temperature
        };
        AnthropicRequest {
            model: &self.model,
            max_tokens: request.max_tokens,
            system: request.system.as_deref(),
            messages,
            temperature,
            tools,
            tool_choice,
        }
    }

    async fn post<B: Serialize, R: DeserializeOwned>(&self, body: &B) -> LlmResult<R> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        debug!(url, "POST anthropic");
        let mut builder = self
            .client
            .post(&url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        // Apply the auth headers through the same helper the tests assert on,
        // so a change to one can't silently diverge from the other.
        for (name, value) in self.auth_headers() {
            builder = builder.header(name, value);
        }
        let resp = builder.json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        response_json_limited::<R>(resp).await
    }

    /// The auth headers for this provider instance: `x-api-key` for a static
    /// key, or `Authorization: Bearer` + `anthropic-beta` for an OAuth
    /// subscription token. The two modes are mutually exclusive — OAuth must
    /// never send `x-api-key` or Anthropic rejects the request. `post` applies
    /// these, and the unit tests assert on them, so both stay in lockstep.
    fn auth_headers(&self) -> Vec<(&'static str, String)> {
        match &self.auth {
            AnthropicAuth::ApiKey(key) => vec![("x-api-key", key.expose_secret().to_string())],
            AnthropicAuth::OAuth(token) => vec![
                ("authorization", format!("Bearer {}", token.expose_secret())),
                ("anthropic-beta", ANTHROPIC_OAUTH_BETA.to_string()),
            ],
        }
    }
}

/// Models that reject ai-memory's non-default `temperature`.
///
/// Anthropic deprecated sampling parameters on the newer models: sending
/// `temperature` to Claude 4.7+ or to Claude Mythos Preview returns a
/// 400 with `` `temperature` is deprecated for this model. `` Every structured
/// call site — bootstrap, consolidation, lint, auto-improve — passes 0.1-0.2,
/// so without this the whole LLM pipeline is unusable on those models.
/// Omitting the field lets the API apply its own default, the same escape
/// hatch `openai.rs::model_requires_default_temperature` uses for gpt-5 /
/// o-series.
///
/// Anything we can't parse as a modern `claude-<family>-<major>[-<minor>]` id
/// keeps the caller's value: the legacy `claude-3-5-sonnet-…` ordering and the
/// whole Claude 4.0-4.6 line still accept sampling parameters, and a gateway
/// proxying a non-Claude model behind this wire format must not be silently
/// stripped either.
fn model_rejects_temperature(model: &str) -> bool {
    if is_mythos_preview(model) {
        return true;
    }
    match claude_family_version(model) {
        Some((major, minor)) => major >= 5 || (major == 4 && minor >= 7),
        None => false,
    }
}

/// Match the dateless preview id, including vendor-prefixed variants.
fn is_mythos_preview(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let Some(rest) = lower
        .find("claude-")
        .map(|start| &lower[start + "claude-".len()..])
    else {
        return false;
    };
    let mut parts = rest.split(|c: char| !c.is_ascii_alphanumeric());
    matches!(
        (parts.next(), parts.next()),
        (Some("mythos"), Some("preview"))
    )
}

/// Parse the `<major>[-<minor>]` version following the family name of a modern
/// Claude id (`claude-opus-5`, `claude-sonnet-4-6`, `claude-opus-5@20260115`,
/// `anthropic.claude-opus-5-v1:0`). Returns `None` for the legacy
/// family-last ordering (`claude-3-5-sonnet-20241022`) and for ids that carry
/// no numeric version (`claude-opus-latest`).
fn claude_family_version(model: &str) -> Option<(u32, u32)> {
    let lower = model.to_ascii_lowercase();
    // Bedrock/Vertex ids carry a vendor prefix and a snapshot suffix around
    // the same `claude-<family>-<version>` core.
    let rest = &lower[lower.find("claude-")? + "claude-".len()..];
    let mut parts = rest.split(|c: char| !c.is_ascii_alphanumeric());
    if !matches!(
        parts.next()?,
        "opus" | "sonnet" | "haiku" | "fable" | "mythos"
    ) {
        return None;
    }
    let major = version_segment(parts.next()?)?;
    let minor = parts.next().and_then(version_segment).unwrap_or(0);
    Some((major, minor))
}

/// A version segment is one or two digits; a longer digit run is a date
/// snapshot (`claude-opus-4-20250514`), not a minor version.
fn version_segment(segment: &str) -> Option<u32> {
    if segment.is_empty() || segment.len() > 2 || !segment.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    segment.parse().ok()
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use serde_json::json;

    use crate::types::ChatMessage;

    use super::*;

    #[test]
    fn api_key_provider_sends_x_api_key_no_authorization() {
        let provider =
            AnthropicProvider::new(SecretString::from("sk-ant-test"), "claude-sonnet-4-6").unwrap();
        let headers = provider.auth_headers();
        let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"x-api-key"), "expected x-api-key header");
        assert!(
            !names.contains(&"authorization"),
            "api-key mode must NOT send authorization header"
        );
        assert!(
            !names.contains(&"anthropic-beta"),
            "api-key mode must NOT send anthropic-beta header"
        );
        let key_val = headers
            .iter()
            .find(|(n, _)| *n == "x-api-key")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert_eq!(key_val, "sk-ant-test");
    }

    #[test]
    fn oauth_provider_sends_bearer_and_beta_no_x_api_key() {
        let provider =
            AnthropicProvider::new_oauth(SecretString::from("tok-oauth-test"), "claude-sonnet-4-6")
                .unwrap();
        let headers = provider.auth_headers();
        let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
        assert!(
            !names.contains(&"x-api-key"),
            "oauth mode must NOT send x-api-key header"
        );
        assert!(
            names.contains(&"authorization"),
            "expected authorization header"
        );
        assert!(
            names.contains(&"anthropic-beta"),
            "expected anthropic-beta header"
        );
        let auth_val = headers
            .iter()
            .find(|(n, _)| *n == "authorization")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert_eq!(auth_val, "Bearer tok-oauth-test");
        let beta_val = headers
            .iter()
            .find(|(n, _)| *n == "anthropic-beta")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert!(
            beta_val.contains("oauth-2025-04-20"),
            "anthropic-beta must contain oauth-2025-04-20"
        );
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 256,
            temperature: Some(0.2),
        }
    }

    /// Serialized `/v1/messages` body for `model`, with the caller's
    /// temperature set to 0.2 — what bootstrap / consolidation actually send.
    fn body_for(model: &str, schema: Option<serde_json::Value>) -> serde_json::Value {
        let provider = AnthropicProvider::new(SecretString::from("sk-ant-test"), model).unwrap();
        let request = chat_request();
        serde_json::to_value(provider.build_request(&request, schema)).unwrap()
    }

    #[test]
    fn build_request_omits_temperature_for_models_that_deprecated_it() {
        // The structured path is the one that actually broke in the field:
        // bootstrap sends temperature 0.2 and Anthropic answers 400
        // "`temperature` is deprecated for this model".
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
            "anthropic.claude-mythos-preview-v1:0",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "anthropic.claude-opus-5-v1:0",
        ] {
            let body = body_for(model, Some(json!({})));
            assert!(
                body.get("temperature").is_none(),
                "temperature must be omitted for {model}"
            );
        }
    }

    #[test]
    fn build_request_keeps_temperature_for_models_that_still_accept_it() {
        // Determinism matters for consolidation output, so the models that
        // still honour sampling params must keep the caller's 0.2.
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-haiku-4-5-20251001",
            "claude-opus-4-20250514",
            "claude-3-5-sonnet-20241022",
        ] {
            let body = body_for(model, None);
            let temp = body["temperature"]
                .as_f64()
                .unwrap_or_else(|| panic!("temperature must be forwarded for {model}, got {body}"));
            assert!((temp - 0.2).abs() < 1e-6, "{model}: got {temp}");
        }
    }

    #[test]
    fn build_request_keeps_the_structured_tool_shape() {
        // The temperature rule must not disturb the forced-tool wiring the
        // structured path depends on.
        let body = body_for("claude-opus-5", Some(json!({"type": "object"})));
        assert_eq!(body["tools"][0]["name"], json!("result"));
        assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "result"})
        );

        let plain = body_for("claude-opus-5", None);
        assert!(plain.get("tools").is_none());
        assert!(plain.get("tool_choice").is_none());
    }

    #[test]
    fn with_base_url_is_preserved_after_oauth_construction() {
        let provider = AnthropicProvider::new_oauth(SecretString::from("tok"), "claude-sonnet-4-6")
            .unwrap()
            .with_base_url("http://localhost:9999");
        assert_eq!(provider.base_url, "http://localhost:9999");
    }
}
