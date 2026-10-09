use super::ModelError;
use super::stream::StreamAccumulator;
use super::types::{ChatRequest, ChatResponse, ChatUsage, Message, ToolCall};
use crate::transport::{self, RetryPolicy, TransportError};
use serde_json::Value;
use std::fmt;
use std::time::Duration;

/// Clé d'API : jamais affichée.
#[derive(Clone)]
struct ApiKey(String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Clone, Debug)]
pub struct ChatConfig {
    /// Racine de l'API compatible OpenAI, avec `/v1` : `https://openrouter.ai/api/v1`.
    /// Le client ajoute `/chat/completions`.
    pub base_url: String,
    pub timeout: Duration,
    pub max_retries: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub retry_after_cap: Duration,
}

impl Default for ChatConfig {
    fn default() -> Self {
        ChatConfig {
            base_url: "https://openrouter.ai/api/v1".into(),
            timeout: Duration::from_secs(120),
            max_retries: 2,
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(8),
            retry_after_cap: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatClient {
    http: reqwest::Client,
    cfg: ChatConfig,
    key: ApiKey,
}

impl ChatClient {
    pub fn new(cfg: ChatConfig, api_key: impl Into<String>) -> Result<Self, ModelError> {
        let key = api_key.into();
        if key.trim().is_empty() {
            return Err(ModelError::InvalidRequest("empty API key".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| ModelError::InvalidRequest(format!("http client: {e}")))?;
        Ok(ChatClient {
            http,
            cfg,
            key: ApiKey(key),
        })
    }

    /// Lit la clé dans la variable d'environnement `var` (jamais dans un fichier de projet, §39.2).
    pub fn from_env(cfg: ChatConfig, var: &str) -> Result<Self, ModelError> {
        match std::env::var(var) {
            Ok(key) if !key.trim().is_empty() => Self::new(cfg, key),
            Ok(_) => Err(ModelError::NotConfigured(format!(
                "environment variable {var} is empty"
            ))),
            Err(_) => Err(ModelError::NotConfigured(format!(
                "environment variable {var} is not set"
            ))),
        }
    }

    fn url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        )
    }

    pub async fn complete(&self, req: &ChatRequest) -> Result<ChatResponse, ModelError> {
        if req.model.trim().is_empty() {
            return Err(ModelError::InvalidRequest(
                "no model configured for this tier".into(),
            ));
        }
        if req.messages.is_empty() {
            return Err(ModelError::InvalidRequest("no message".into()));
        }
        let policy = RetryPolicy {
            max_retries: self.cfg.max_retries,
            backoff_base: self.cfg.backoff_base,
            backoff_max: self.cfg.backoff_max,
            retry_after_cap: self.cfg.retry_after_cap,
        };
        let body = transport::post_json(&self.http, &self.url(), &self.key.0, req, &policy)
            .await
            .map_err(|e| match e {
                TransportError::Api { status, message } => ModelError::Api { status, message },
                TransportError::Unavailable { attempts, last } => {
                    ModelError::Unavailable { attempts, last }
                }
                TransportError::Decode(m) => ModelError::Decode(m),
            })?;
        parse(&body, &req.model)
    }
}

impl ChatClient {
    /// Réponse en flux SSE. Les nouvelles tentatives ne valent que tant qu'aucun octet de la
    /// réponse n'a été lu : une fois le flux commencé, une coupure est une erreur (les fragments
    /// déjà affichés ne peuvent pas être rejoués).
    pub async fn complete_stream(
        &self,
        req: &ChatRequest,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatResponse, ModelError> {
        if req.model.trim().is_empty() {
            return Err(ModelError::InvalidRequest(
                "no model configured for this tier".into(),
            ));
        }
        if req.messages.is_empty() {
            return Err(ModelError::InvalidRequest("no message".into()));
        }
        let mut body = serde_json::to_value(req)
            .map_err(|e| ModelError::InvalidRequest(format!("request: {e}")))?;
        body["stream"] = Value::Bool(true);
        body["stream_options"] = serde_json::json!({"include_usage": true});

        let policy = RetryPolicy {
            max_retries: self.cfg.max_retries,
            backoff_base: self.cfg.backoff_base,
            backoff_max: self.cfg.backoff_max,
            retry_after_cap: self.cfg.retry_after_cap,
        };
        let attempts = policy.max_retries + 1;
        let mut last = String::new();
        for attempt in 0..attempts {
            let mut retry_after = None;
            match self
                .http
                .post(self.url())
                .bearer_auth(&self.key.0)
                .json(&body)
                .send()
                .await
            {
                Ok(mut res) if res.status().as_u16() == 200 => {
                    let is_sse = res
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.contains("text/event-stream"));
                    if !is_sse {
                        // Le fournisseur a ignoré `stream: true` : réponse JSON ordinaire.
                        let text = res
                            .text()
                            .await
                            .map_err(|e| ModelError::Decode(e.to_string()))?;
                        let r = parse(&text, &req.model)?;
                        if !r.text().is_empty() {
                            on_delta(r.text());
                        }
                        return Ok(r);
                    }
                    let mut acc = StreamAccumulator::new(&req.model, on_delta);
                    loop {
                        match res.chunk().await {
                            Ok(Some(bytes)) => acc.feed(&bytes)?,
                            Ok(None) => break,
                            Err(e) => {
                                return Err(ModelError::Unavailable {
                                    attempts: attempt + 1,
                                    last: format!("stream interrupted: {e}"),
                                });
                            }
                        }
                    }
                    return acc.finish();
                }
                Ok(res) => {
                    let status = res.status().as_u16();
                    retry_after = res
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let text = res
                        .text()
                        .await
                        .map_err(|e| ModelError::Decode(e.to_string()))?;
                    let message = transport::error_message(&text);
                    if !transport::retryable(status) {
                        return Err(ModelError::Api { status, message });
                    }
                    last = format!("http {status}: {message}");
                }
                Err(e) if e.is_timeout() || e.is_connect() || e.is_request() => {
                    last = format!("network: {e}")
                }
                Err(e) => return Err(ModelError::Decode(e.to_string())),
            }
            if attempt + 1 < attempts {
                tokio::time::sleep(policy.delay(attempt, retry_after)).await;
            }
        }
        Err(ModelError::Unavailable { attempts, last })
    }
}

impl super::ModelProvider for ChatClient {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, ModelError> {
        self.complete(&request).await
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<ChatResponse, ModelError> {
        self.complete_stream(&request, on_delta).await
    }
}

/// Interprète le corps d'une réponse 200. OpenRouter peut renvoyer une erreur du fournisseur
/// *dans* un corps 200 (`{"error": {...}}` sans `choices`).
fn parse(body: &str, requested: &str) -> Result<ChatResponse, ModelError> {
    let v: Value = serde_json::from_str(body).map_err(|e| ModelError::Decode(e.to_string()))?;
    let choice = v.pointer("/choices/0");
    if choice.is_none() {
        if let Some(err) = v.get("error") {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown provider error")
                .to_string();
            let code = err.get("code").and_then(Value::as_u64).unwrap_or(502) as u16;
            return Err(if code == 429 || code >= 500 {
                ModelError::Unavailable {
                    attempts: 1,
                    last: format!("provider error {code}: {message}"),
                }
            } else {
                ModelError::Api {
                    status: code,
                    message,
                }
            });
        }
        return Err(ModelError::InvalidResponse(
            "no `choices` in the response".into(),
        ));
    }
    let choice = choice.expect("checked above");
    let msg = choice
        .get("message")
        .ok_or_else(|| ModelError::InvalidResponse("choice without `message`".into()))?;

    let content = msg
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty());
    let tool_calls: Vec<ToolCall> = match msg.get("tool_calls") {
        None | Some(Value::Null) => vec![],
        Some(tc) => serde_json::from_value(tc.clone())
            .map_err(|e| ModelError::InvalidResponse(format!("tool_calls: {e}")))?,
    };
    for tc in &tool_calls {
        if tc.id.is_empty() || tc.function.name.is_empty() {
            return Err(ModelError::InvalidResponse(
                "tool call without id or name".into(),
            ));
        }
    }
    if content.is_none()
        && tool_calls.is_empty()
        && choice.get("finish_reason").and_then(Value::as_str) != Some("stop")
    {
        return Err(ModelError::InvalidResponse(
            "empty assistant message".into(),
        ));
    }
    let usage: ChatUsage = v
        .get("usage")
        .and_then(|u| serde_json::from_value(u.clone()).ok())
        .unwrap_or_default();
    Ok(ChatResponse {
        message: Message::Assistant {
            content,
            tool_calls,
        },
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(requested)
            .to_string(),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ModelProvider;
    use crate::jev::testing::{MockServer, Reply};
    use serde_json::json;

    fn cfg(base: &str) -> ChatConfig {
        ChatConfig {
            base_url: base.into(),
            timeout: Duration::from_millis(500),
            max_retries: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(5),
            retry_after_cap: Duration::from_millis(50),
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "vendor/model".into(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

    fn text_body(text: &str) -> String {
        json!({"id":"g1","model":"vendor/model-2026","choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":10,"completion_tokens":3,"cost":0.00001}})
        .to_string()
    }

    #[tokio::test]
    async fn sends_to_chat_completions_with_bearer_and_parses_text() {
        let server = MockServer::start(vec![Reply::json(200, &text_body("hello"))]).await;
        let c = ChatClient::new(cfg(&format!("{}/api/v1/", server.url())), "sk-secret").unwrap();
        let r = c.chat(req()).await.unwrap();
        assert_eq!(
            (r.text(), r.finish_reason.as_deref(), r.model.as_str()),
            ("hello", Some("stop"), "vendor/model-2026")
        );
        assert_eq!(
            (
                r.usage.prompt_tokens,
                r.usage.completion_tokens,
                r.usage.cost
            ),
            (10, 3, Some(0.00001))
        );

        let seen = &server.requests()[0];
        assert_eq!(seen.path, "/api/v1/chat/completions");
        assert_eq!(
            seen.header("authorization").as_deref(),
            Some("Bearer sk-secret")
        );
        let body: Value = serde_json::from_str(&seen.body).unwrap();
        assert_eq!(body["model"], "vendor/model");
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    fn sse(chunks: &[Value]) -> String {
        let mut out = String::new();
        for c in chunks {
            out.push_str(&format!("data: {c}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        out
    }

    #[tokio::test]
    async fn streams_text_and_asks_for_usage() {
        let body = sse(&[
            json!({"model":"vendor/model-2026","choices":[{"delta":{"content":"hel"}}]}),
            json!({"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}),
        ]);
        let server = MockServer::start(vec![Reply::sse(&body)]).await;
        let c = ChatClient::new(cfg(&server.url()), "sk-secret").unwrap();
        let mut seen = Vec::new();
        let r = c
            .chat_stream(req(), &mut |d: &str| seen.push(d.to_string()))
            .await
            .unwrap();
        assert_eq!(seen.concat(), "hello");
        assert_eq!(r.text(), "hello");
        assert_eq!(r.usage.prompt_tokens, 7);
        let sent: Value = serde_json::from_str(&server.requests()[0].body).unwrap();
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn streaming_retries_before_the_stream_starts_but_not_after() {
        let ok = sse(&[json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
        let server = MockServer::start(vec![Reply::json(503, "down"), Reply::sse(&ok)]).await;
        let c = ChatClient::new(cfg(&server.url()), "k").unwrap();
        let r = c.chat_stream(req(), &mut |_: &str| {}).await.unwrap();
        assert_eq!(r.text(), "ok");
        assert_eq!(server.requests().len(), 2);

        // Flux coupé net : erreur, sans nouvelle tentative (les fragments sont déjà affichés).
        let cut = "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n";
        let server = MockServer::start(vec![Reply::sse(cut)]).await;
        let c = ChatClient::new(cfg(&server.url()), "k").unwrap();
        let err = c.chat_stream(req(), &mut |_: &str| {}).await.unwrap_err();
        assert!(matches!(err, ModelError::Unavailable { .. }), "{err:?}");
        assert_eq!(server.requests().len(), 1);

        let server =
            MockServer::start(vec![Reply::json(401, r#"{"error":{"message":"bad"}}"#)]).await;
        let c = ChatClient::new(cfg(&server.url()), "k").unwrap();
        let err = c.chat_stream(req(), &mut |_: &str| {}).await.unwrap_err();
        assert!(
            matches!(err, ModelError::Api { status: 401, .. }),
            "{err:?}"
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_provider_that_ignores_streaming_still_works() {
        let server = MockServer::start(vec![Reply::json(200, &text_body("plain"))]).await;
        let c = ChatClient::new(cfg(&server.url()), "k").unwrap();
        let mut seen = Vec::new();
        let r = c
            .chat_stream(req(), &mut |d: &str| seen.push(d.to_string()))
            .await
            .unwrap();
        assert_eq!((r.text(), seen), ("plain", vec!["plain".to_string()]));
    }

    #[tokio::test]
    async fn parses_tool_calls_with_null_content() {
        let body = json!({"model":"m","choices":[{"message":{"role":"assistant","content":null,"tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"fs_read","arguments":"{\"path\":\"a.txt\"}"}}]},
            "finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
        .to_string();
        let server = MockServer::start(vec![Reply::json(200, &body)]).await;
        let r = ChatClient::new(cfg(&server.url()), "k")
            .unwrap()
            .chat(req())
            .await
            .unwrap();
        assert_eq!(r.text(), "");
        assert_eq!(r.tool_calls().len(), 1);
        assert_eq!(r.tool_calls()[0].function.name, "fs_read");
        assert_eq!(r.tool_calls()[0].args().unwrap(), json!({"path": "a.txt"}));
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn retries_transient_errors_but_not_client_errors() {
        let server = MockServer::start(vec![
            Reply::json(503, "down"),
            Reply::json(200, &text_body("ok")),
        ])
        .await;
        assert!(
            ChatClient::new(cfg(&server.url()), "k")
                .unwrap()
                .chat(req())
                .await
                .is_ok()
        );
        assert_eq!(server.requests().len(), 2);

        let server =
            MockServer::start(vec![Reply::json(401, r#"{"error":{"message":"bad key"}}"#)]).await;
        let err = ChatClient::new(cfg(&server.url()), "k")
            .unwrap()
            .chat(req())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ModelError::Api { status: 401, message } if message == "bad key"),
            "{err:?}"
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn exhausted_retries_report_unavailable() {
        let server = MockServer::start(vec![Reply::json(502, "bad gateway"); 3]).await;
        let err = ChatClient::new(cfg(&server.url()), "k")
            .unwrap()
            .chat(req())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ModelError::Unavailable { attempts: 3, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn an_error_hidden_in_a_200_body_is_recognized() {
        // OpenRouter : la panne du fournisseur arrive avec un statut 200.
        let b = json!({"error": {"code": 502, "message": "provider disconnected"}}).to_string();
        let server = MockServer::start(vec![Reply::json(200, &b)]).await;
        let err = ChatClient::new(cfg(&server.url()), "k")
            .unwrap()
            .chat(req())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ModelError::Unavailable { last, .. } if last.contains("provider disconnected")),
            "{err:?}"
        );

        let b = json!({"error": {"code": 400, "message": "context too long"}}).to_string();
        let server = MockServer::start(vec![Reply::json(200, &b)]).await;
        let err = ChatClient::new(cfg(&server.url()), "k")
            .unwrap()
            .chat(req())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ModelError::Api { status: 400, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn malformed_responses_are_rejected() {
        for body in [
            "not json".to_string(),
            json!({"choices": []}).to_string(),
            json!({"choices": [{"message": {"role": "assistant", "content": null}, "finish_reason": "length"}]}).to_string(),
            json!({"choices": [{"message": {"role": "assistant", "tool_calls": [{"id": "", "function": {"name": "x", "arguments": "{}"}}]}}]}).to_string(),
        ] {
            let server = MockServer::start(vec![Reply::json(200, &body)]).await;
            assert!(ChatClient::new(cfg(&server.url()), "k").unwrap().chat(req()).await.is_err(), "{body}");
        }
    }

    #[tokio::test]
    async fn an_empty_stop_message_is_accepted_as_empty_text() {
        let b = json!({"choices": [{"message": {"role": "assistant", "content": ""}, "finish_reason": "stop"}]}).to_string();
        let server = MockServer::start(vec![Reply::json(200, &b)]).await;
        let r = ChatClient::new(cfg(&server.url()), "k")
            .unwrap()
            .chat(req())
            .await
            .unwrap();
        assert_eq!(r.text(), "");
    }

    #[tokio::test]
    async fn invalid_requests_never_reach_the_network() {
        let server = MockServer::start(vec![]).await;
        let c = ChatClient::new(cfg(&server.url()), "k").unwrap();
        let mut r = req();
        r.model = " ".into();
        assert!(matches!(
            c.chat(r).await,
            Err(ModelError::InvalidRequest(_))
        ));
        let mut r = req();
        r.messages.clear();
        assert!(c.chat(r).await.is_err());
        assert!(server.requests().is_empty());
    }

    #[test]
    fn the_key_is_redacted_and_required() {
        let c = ChatClient::new(ChatConfig::default(), "sk-super-secret").unwrap();
        assert!(!format!("{c:?}").contains("sk-super-secret"));
        assert!(ChatClient::new(ChatConfig::default(), "").is_err());
    }
}
