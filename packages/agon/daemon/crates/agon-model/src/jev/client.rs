use super::types::{Question, Request, Response};
use super::{JevError, validate};
use crate::transport::{self, RetryPolicy, TransportError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// Clé d'API : jamais affichée, jamais sérialisée.
#[derive(Clone)]
struct ApiKey(String);

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Clone, Debug)]
pub struct JevConfig {
    /// `https://api.typesafe.ai` (direct) ou `https://openrouter.ai/api` (OpenRouter).
    /// Le client ajoute `/v1/systemone`.
    pub base_url: String,
    /// Identifiant du modèle : `jev-1.13.0` en direct, `typesafe/jev-1.13` ou `~typesafe/jev-latest`
    /// via OpenRouter.
    pub model: String,
    pub timeout: Duration,
    /// Nombre de nouvelles tentatives après la première (429, 5xx, timeout, erreur réseau).
    pub max_retries: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Plafond appliqué à un `retry-after` renvoyé par le serveur.
    pub retry_after_cap: Duration,
}

impl Default for JevConfig {
    fn default() -> Self {
        JevConfig {
            base_url: "https://api.typesafe.ai".into(),
            model: "jev-1.13.0".into(),
            timeout: Duration::from_secs(5),
            max_retries: 3,
            backoff_base: Duration::from_millis(250),
            backoff_max: Duration::from_secs(4),
            retry_after_cap: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct JevClient {
    http: reqwest::Client,
    cfg: JevConfig,
    key: ApiKey,
}

impl JevClient {
    pub fn new(cfg: JevConfig, api_key: impl Into<String>) -> Result<Self, JevError> {
        let key = api_key.into();
        if key.trim().is_empty() {
            return Err(JevError::InvalidRequest("empty API key".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| JevError::InvalidRequest(format!("http client: {e}")))?;
        Ok(JevClient {
            http,
            cfg,
            key: ApiKey(key),
        })
    }

    /// Lit la clé dans la variable d'environnement `var` (jamais depuis un fichier de Form ou de session, §39.2).
    pub fn from_env(cfg: JevConfig, var: &str) -> Result<Self, JevError> {
        match std::env::var(var) {
            Ok(key) if !key.trim().is_empty() => Self::new(cfg, key),
            Ok(_) => Err(JevError::NotConfigured(format!(
                "environment variable {var} is empty"
            ))),
            Err(_) => Err(JevError::NotConfigured(format!(
                "environment variable {var} is not set"
            ))),
        }
    }

    pub fn config(&self) -> &JevConfig {
        &self.cfg
    }

    fn url(&self) -> String {
        format!("{}/v1/systemone", self.cfg.base_url.trim_end_matches('/'))
    }

    fn policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: self.cfg.max_retries,
            backoff_base: self.cfg.backoff_base,
            backoff_max: self.cfg.backoff_max,
            retry_after_cap: self.cfg.retry_after_cap,
        }
    }

    /// Envoie `state` et `questions`, valide la réponse et la renvoie.
    pub async fn evaluate(
        &self,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Response, JevError> {
        let req = Request {
            model: self.cfg.model.clone(),
            state,
            questions,
        };
        validate::request(&req)?;

        let body = transport::post_json(&self.http, &self.url(), &self.key.0, &req, &self.policy())
            .await
            .map_err(|e| match e {
                TransportError::Api { status, message } => JevError::Api { status, message },
                TransportError::Unavailable { attempts, last } => {
                    JevError::Unavailable { attempts, last }
                }
                TransportError::Decode(m) => JevError::Decode(m),
            })?;
        let resp: Response =
            serde_json::from_str(&body).map_err(|e| JevError::Decode(e.to_string()))?;
        validate::response(&req, &resp)?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::testing::{MockServer, Reply};
    use serde_json::json;
    use std::time::Instant;

    fn cfg(base: &str) -> JevConfig {
        JevConfig {
            base_url: base.into(),
            model: "jev-1.13.0".into(),
            timeout: Duration::from_millis(500),
            max_retries: 2,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(5),
            retry_after_cap: Duration::from_secs(1),
        }
    }

    fn one_noul() -> BTreeMap<String, Question> {
        BTreeMap::from([("urgent".to_string(), Question::noul("Is it urgent?"))])
    }

    fn ok_body() -> String {
        json!({"id":"gen-1","model":"typesafe/jev-1.13-20260917","provider":"TypeSafe",
               "answers":{"urgent":{"type":"noul","noul":0.95}},
               "usage":{"input_tokens":10,"output_tokens":5,"cost":0.00001}})
        .to_string()
    }

    #[tokio::test]
    async fn sends_the_documented_request_and_parses_the_answer() {
        let server = MockServer::start(vec![Reply::json(200, &ok_body())]).await;
        let client = JevClient::new(cfg(&server.url()), "sk-test-secret").unwrap();
        let resp = client
            .evaluate(json!("Help! payouts failing"), one_noul())
            .await
            .unwrap();
        assert_eq!(resp.model, "typesafe/jev-1.13-20260917");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/v1/systemone");
        assert_eq!(
            reqs[0].header("authorization").as_deref(),
            Some("Bearer sk-test-secret")
        );
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["model"], "jev-1.13.0");
        assert_eq!(body["state"], "Help! payouts failing");
        assert_eq!(body["questions"]["urgent"]["type"], "noul");
    }

    #[tokio::test]
    async fn base_url_with_path_and_trailing_slash_works_for_openrouter() {
        let server = MockServer::start(vec![Reply::json(200, &ok_body())]).await;
        let client = JevClient::new(cfg(&format!("{}/api/", server.url())), "k").unwrap();
        client.evaluate(json!("s"), one_noul()).await.unwrap();
        assert_eq!(server.requests()[0].path, "/api/v1/systemone");
    }

    #[tokio::test]
    async fn retries_429_honoring_retry_after_then_succeeds() {
        let server = MockServer::start(vec![
            Reply::json(429, r#"{"error":{"code":429,"message":"slow down"}}"#)
                .with_header("retry-after", "0"),
            Reply::json(200, &ok_body()),
        ])
        .await;
        let client = JevClient::new(cfg(&server.url()), "k").unwrap();
        assert!(client.evaluate(json!("s"), one_noul()).await.is_ok());
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn retry_after_delay_is_honored_but_capped() {
        let server = MockServer::start(vec![
            Reply::json(429, "{}").with_header("retry-after", "3600"),
            Reply::json(200, &ok_body()),
        ])
        .await;
        let mut c = cfg(&server.url());
        c.retry_after_cap = Duration::from_millis(150);
        let started = Instant::now();
        JevClient::new(c, "k")
            .unwrap()
            .evaluate(json!("s"), one_noul())
            .await
            .unwrap();
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(140) && took < Duration::from_secs(2),
            "{took:?}"
        );
    }

    #[tokio::test]
    async fn exhausted_retries_report_unavailable() {
        let server = MockServer::start(vec![Reply::json(503, "down"); 3]).await;
        let client = JevClient::new(cfg(&server.url()), "k").unwrap();
        let err = client.evaluate(json!("s"), one_noul()).await.unwrap_err();
        assert!(
            matches!(err, JevError::Unavailable { attempts: 3, .. }),
            "{err:?}"
        );
        assert_eq!(server.requests().len(), 3);
    }

    #[tokio::test]
    async fn client_errors_are_not_retried_and_carry_the_message() {
        for (status, body) in [
            (
                401,
                r#"{"error":{"code":401,"message":"Missing Authentication header"}}"#,
            ),
            (
                402,
                r#"{"error":{"code":402,"message":"Insufficient credits"}}"#,
            ),
            (400, r#"{"message":"Invalid request parameters"}"#),
        ] {
            let server = MockServer::start(vec![Reply::json(status, body)]).await;
            let client = JevClient::new(cfg(&server.url()), "k").unwrap();
            let err = client.evaluate(json!("s"), one_noul()).await.unwrap_err();
            match err {
                JevError::Api { status: s, message } => {
                    assert_eq!(s, status);
                    assert!(!message.is_empty() && !message.contains('{'), "{message}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(
                server.requests().len(),
                1,
                "status {status} must not be retried"
            );
        }
    }

    #[tokio::test]
    async fn timeouts_are_retried_then_reported_unavailable() {
        let server = MockServer::start(vec![
            Reply::json(200, &ok_body())
                .delayed(Duration::from_millis(400));
            2
        ])
        .await;
        let mut c = cfg(&server.url());
        c.timeout = Duration::from_millis(50);
        c.max_retries = 1;
        let err = JevClient::new(c, "k")
            .unwrap()
            .evaluate(json!("s"), one_noul())
            .await
            .unwrap_err();
        assert!(
            matches!(err, JevError::Unavailable { attempts: 2, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn unreachable_server_reports_unavailable() {
        let mut c = cfg("http://127.0.0.1:1");
        c.max_retries = 1;
        let err = JevClient::new(c, "k")
            .unwrap()
            .evaluate(json!("s"), one_noul())
            .await
            .unwrap_err();
        assert!(
            matches!(err, JevError::Unavailable { attempts: 2, .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn inconsistent_response_is_rejected_without_retry() {
        let body = json!({"model":"m","answers":{},"usage":{"input_tokens":1,"output_tokens":1}})
            .to_string();
        let server =
            MockServer::start(vec![Reply::json(200, &body), Reply::json(200, &ok_body())]).await;
        let client = JevClient::new(cfg(&server.url()), "k").unwrap();
        let err = client.evaluate(json!("s"), one_noul()).await.unwrap_err();
        assert!(matches!(err, JevError::InvalidResponse(_)), "{err:?}");
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn garbage_body_is_a_decode_error() {
        let server = MockServer::start(vec![Reply::json(200, "<html>not json</html>")]).await;
        let client = JevClient::new(cfg(&server.url()), "k").unwrap();
        assert!(matches!(
            client.evaluate(json!("s"), one_noul()).await,
            Err(JevError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn invalid_request_never_reaches_the_network() {
        let server = MockServer::start(vec![]).await;
        let client = JevClient::new(cfg(&server.url()), "k").unwrap();
        let q = BTreeMap::from([("c".to_string(), Question::choice("x?", [("only", "one")]))]);
        assert!(matches!(
            client.evaluate(json!("s"), q).await,
            Err(JevError::InvalidRequest(_))
        ));
        assert!(server.requests().is_empty());
    }

    #[test]
    fn api_key_is_redacted_in_debug_output() {
        let client = JevClient::new(JevConfig::default(), "sk-super-secret").unwrap();
        let dbg = format!("{client:?}");
        assert!(
            !dbg.contains("sk-super-secret") && dbg.contains("redacted"),
            "{dbg}"
        );
    }

    #[test]
    fn empty_key_is_refused() {
        assert!(JevClient::new(JevConfig::default(), "  ").is_err());
    }
}
