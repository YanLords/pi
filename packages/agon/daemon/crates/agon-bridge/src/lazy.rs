//! Client Jev paresseux : initialise la connexion uniquement au premier appel.
//! Si la variable d'environnement est absente, l'erreur n'est levée qu'au moment
//! où une décision est réellement requise, permettant à `status`, `verify` et aux
//! mutations déjà apprises de fonctionner sans clé.

use agon_model::jev::{JevError, Question, Response};
use agon_model::{DecisionProvider, JevClient, JevConfig};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::OnceLock;

pub struct LazyJev {
    cfg: JevConfig,
    key_env: String,
    client: OnceLock<Result<JevClient, String>>,
}

impl LazyJev {
    pub fn new(cfg: JevConfig, key_env: impl Into<String>) -> Self {
        LazyJev {
            cfg,
            key_env: key_env.into(),
            client: OnceLock::new(),
        }
    }

    fn client(&self) -> Result<&JevClient, JevError> {
        self.client
            .get_or_init(|| {
                JevClient::from_env(self.cfg.clone(), &self.key_env).map_err(|e| match e {
                    JevError::NotConfigured(m) | JevError::InvalidRequest(m) => m,
                    other => other.to_string(),
                })
            })
            .as_ref()
            .map_err(|msg| JevError::NotConfigured(msg.clone()))
    }
}

impl DecisionProvider for LazyJev {
    fn decide(
        &self,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> impl Future<Output = Result<Response, JevError>> + Send {
        let client = self.client();
        async move { client?.evaluate(state, questions).await }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unset_key_fails_at_decide_time() {
        let jev = LazyJev::new(JevConfig::default(), "AGON_TEST_NONEXISTENT_KEY_XYZ");
        let q = BTreeMap::from([("q".to_string(), Question::noul("test?"))]);
        let err = jev.decide(serde_json::json!({}), q).await.unwrap_err();
        assert!(matches!(err, JevError::NotConfigured(_)));
        assert!(err.to_string().contains("AGON_TEST_NONEXISTENT_KEY_XYZ"));
    }
}
