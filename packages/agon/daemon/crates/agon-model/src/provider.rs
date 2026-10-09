//! `DecisionProvider` (§30) : l'abstraction que le Kernel utilise pour interroger Jev.
//! Trois implémentations : le client réseau, un enregistreur et un rejeu (§33, scénario 7).

use crate::jev::{JevClient, JevError, Question, Response};
use agon_core::{Digest, sha256_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::Mutex;

pub trait DecisionProvider {
    fn decide(
        &self,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> impl Future<Output = Result<Response, JevError>> + Send;
}

impl DecisionProvider for JevClient {
    fn decide(
        &self,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> impl Future<Output = Result<Response, JevError>> + Send {
        self.evaluate(state, questions)
    }
}

/// Empreinte de ce qui a été demandé (state + questions), indépendante du modèle configuré.
pub fn request_digest(state: &Value, questions: &BTreeMap<String, Question>) -> Digest {
    sha256_of(&(state, questions)).expect("request is serializable")
}

/// Un échange enregistré : de quoi rejouer la réponse et vérifier qu'on rejoue la même question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exchange {
    pub request: Digest,
    pub response: Response,
}

/// Enveloppe un provider et enregistre chaque échange réussi.
pub struct Recorder<P> {
    inner: P,
    log: Mutex<Vec<Exchange>>,
}

impl<P> Recorder<P> {
    pub fn new(inner: P) -> Self {
        Recorder {
            inner,
            log: Mutex::new(Vec::new()),
        }
    }

    pub fn exchanges(&self) -> Vec<Exchange> {
        self.log.lock().unwrap().clone()
    }
}

impl<P: DecisionProvider + Sync> DecisionProvider for Recorder<P> {
    async fn decide(
        &self,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Response, JevError> {
        let request = request_digest(&state, &questions);
        let response = self.inner.decide(state, questions).await?;
        self.log.lock().unwrap().push(Exchange {
            request,
            response: response.clone(),
        });
        Ok(response)
    }
}

/// Rejoue des échanges enregistrés, dans l'ordre, sans aucun appel réseau. Toute divergence
/// (question différente, échanges épuisés) est une erreur : le rejeu doit être exact.
pub struct ReplayProvider {
    queue: Mutex<VecDeque<Exchange>>,
}

impl ReplayProvider {
    pub fn new(exchanges: Vec<Exchange>) -> Self {
        ReplayProvider {
            queue: Mutex::new(exchanges.into()),
        }
    }

    pub fn remaining(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

impl DecisionProvider for ReplayProvider {
    fn decide(
        &self,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> impl Future<Output = Result<Response, JevError>> + Send {
        let next = self.queue.lock().unwrap().pop_front();
        let actual = request_digest(&state, &questions);
        async move {
            let ex =
                next.ok_or_else(|| JevError::ReplayMismatch("no recorded exchange left".into()))?;
            if ex.request != actual {
                return Err(JevError::ReplayMismatch(format!(
                    "recorded request {} but replaying {}",
                    ex.request, actual
                )));
            }
            Ok(ex.response)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::JevConfig;
    use crate::jev::testing::{MockServer, Reply};
    use serde_json::json;
    use std::time::Duration;

    fn q(text: &str) -> BTreeMap<String, Question> {
        BTreeMap::from([("urgent".to_string(), Question::noul(text))])
    }

    fn body(noul: f64) -> String {
        json!({"model":"jev-1.13.0","answers":{"urgent":{"type":"noul","noul":noul}},
               "usage":{"input_tokens":1,"output_tokens":1}})
        .to_string()
    }

    fn client(server: &MockServer) -> JevClient {
        let cfg = JevConfig {
            base_url: server.url(),
            timeout: Duration::from_millis(500),
            ..JevConfig::default()
        };
        JevClient::new(cfg, "k").unwrap()
    }

    #[tokio::test]
    async fn recorded_session_replays_identically_without_the_network() {
        let server = MockServer::start(vec![
            Reply::json(200, &body(0.9)),
            Reply::json(200, &body(0.2)),
        ])
        .await;
        let rec = Recorder::new(client(&server));
        let a = rec.decide(json!("s1"), q("first?")).await.unwrap();
        let b = rec.decide(json!("s2"), q("second?")).await.unwrap();
        assert_eq!(server.requests().len(), 2);

        // Rejeu : le serveur n'est plus sollicité.
        let replay = ReplayProvider::new(rec.exchanges());
        assert_eq!(replay.decide(json!("s1"), q("first?")).await.unwrap(), a);
        assert_eq!(replay.decide(json!("s2"), q("second?")).await.unwrap(), b);
        assert_eq!(replay.remaining(), 0);
        assert_eq!(server.requests().len(), 2, "replay must not call the API");
    }

    #[tokio::test]
    async fn replay_refuses_a_different_question_or_an_exhausted_log() {
        let server = MockServer::start(vec![Reply::json(200, &body(0.9))]).await;
        let rec = Recorder::new(client(&server));
        rec.decide(json!("s1"), q("first?")).await.unwrap();

        let replay = ReplayProvider::new(rec.exchanges());
        let err = replay
            .decide(json!("s1"), q("a different question?"))
            .await
            .unwrap_err();
        assert!(matches!(err, JevError::ReplayMismatch(_)));

        let replay = ReplayProvider::new(rec.exchanges());
        replay.decide(json!("s1"), q("first?")).await.unwrap();
        assert!(matches!(
            replay.decide(json!("s1"), q("first?")).await,
            Err(JevError::ReplayMismatch(_))
        ));
    }

    #[tokio::test]
    async fn failed_calls_are_not_recorded() {
        let server =
            MockServer::start(vec![Reply::json(401, r#"{"error":{"message":"no"}}"#)]).await;
        let rec = Recorder::new(client(&server));
        assert!(rec.decide(json!("s"), q("x?")).await.is_err());
        assert!(rec.exchanges().is_empty());
    }

    #[test]
    fn exchanges_survive_a_json_roundtrip() {
        let ex = Exchange {
            request: request_digest(&json!("s"), &q("x?")),
            response: serde_json::from_str(&body(0.5)).unwrap(),
        };
        let back: Exchange = serde_json::from_str(&serde_json::to_string(&ex).unwrap()).unwrap();
        assert_eq!(back, ex);
    }
}
