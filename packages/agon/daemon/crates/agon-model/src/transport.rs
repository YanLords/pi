//! Transport HTTP partagé par les clients Jev et chat : POST JSON authentifié, avec retry sur les
//! erreurs transitoires (429, 5xx, timeout, réseau), backoff exponentiel et `retry-after` plafonné.
//! Aucune interprétation du corps : chaque client valide sa propre réponse.

use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Nouvelles tentatives après la première.
    pub max_retries: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Plafond appliqué à un `retry-after` renvoyé par le serveur.
    pub retry_after_cap: Duration,
}

impl RetryPolicy {
    pub(crate) fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        match retry_after {
            Some(d) => d.min(self.retry_after_cap),
            None => (self.backoff_base * 2u32.saturating_pow(attempt)).min(self.backoff_max),
        }
    }
}

#[derive(Debug)]
pub enum TransportError {
    /// Erreur non transitoire (400, 401, 402, 403, 404, 413…).
    Api {
        status: u16,
        message: String,
    },
    /// Injoignable ou en surcharge après épuisement des tentatives.
    Unavailable {
        attempts: u32,
        last: String,
    },
    Decode(String),
}

pub(crate) fn retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Message lisible : `{"error":{"message":..}}`, `{"message":..}`, sinon le corps tronqué.
pub fn error_message(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let msg = parsed.as_ref().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.pointer("/error"))
            .or_else(|| v.pointer("/message"))
            .or_else(|| v.pointer("/detail"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    msg.unwrap_or_else(|| body.trim().to_owned())
        .chars()
        .take(300)
        .collect()
}

/// Envoie `body` en JSON et renvoie le corps d'une réponse 200.
pub async fn post_json(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
    body: &impl Serialize,
    policy: &RetryPolicy,
) -> Result<String, TransportError> {
    let attempts = policy.max_retries + 1;
    let mut last = String::new();
    for attempt in 0..attempts {
        let mut retry_after = None;
        match http.post(url).bearer_auth(bearer).json(body).send().await {
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
                    .map_err(|e| TransportError::Decode(e.to_string()))?;
                if status == 200 {
                    return Ok(text);
                }
                let message = error_message(&text);
                if !retryable(status) {
                    return Err(TransportError::Api { status, message });
                }
                last = format!("http {status}: {message}");
            }
            Err(e) if e.is_timeout() || e.is_connect() || e.is_request() => {
                last = format!("network: {e}")
            }
            Err(e) => return Err(TransportError::Decode(e.to_string())),
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(policy.delay(attempt, retry_after)).await;
        }
    }
    Err(TransportError::Unavailable { attempts, last })
}
