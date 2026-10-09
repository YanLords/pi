//! Test réel contre l'API (payant, quelques millièmes de centime). Ignoré par défaut :
//!
//! ```text
//! TYPESAFE_API_KEY=... TYPESAFE_BASE_URL=https://openrouter.ai/api JEV_MODEL='~typesafe/jev-latest' \
//!   cargo test -p agon-model --test live -- --ignored --nocapture
//! ```
//! La clé vient uniquement de l'environnement (§39.2).

use agon_model::jev::{Answer, Question};
use agon_model::{JevClient, JevConfig};
use serde_json::json;
use std::collections::BTreeMap;

#[tokio::test]
#[ignore = "calls the real Jev API"]
async fn jev_picks_the_docker_postgres_mutation_for_connection_refused() {
    let cfg = JevConfig {
        base_url: std::env::var("TYPESAFE_BASE_URL")
            .unwrap_or_else(|_| "https://api.typesafe.ai".into()),
        model: std::env::var("JEV_MODEL").unwrap_or_else(|_| "jev-1.13.0".into()),
        ..JevConfig::default()
    };
    let client = JevClient::from_env(cfg, "TYPESAFE_API_KEY").unwrap();

    let state = json!({
        "task": "Make the integration test suite pass",
        "signature": {"check": "verify.integration", "class": "connection_refused", "service": "postgres", "port": 5432},
        "form_capabilities": ["rust", "cargo"]
    });
    let questions = BTreeMap::from([
        (
            "mutation".to_string(),
            Question::choice(
                "Which candidate change to the runtime configuration best addresses `signature`?",
                [
                    (
                        "m_docker_postgres",
                        "Enable capabilities docker and postgres",
                    ),
                    ("m_sqlite", "Enable capability sqlite"),
                    ("none", "No configuration change is appropriate"),
                    ("human", "The situation requires a human decision"),
                ],
            ),
        ),
        (
            "needs_cargo".to_string(),
            Question::noul("Is running `cargo test` needed to make progress on `task`?"),
        ),
        (
            "risk".to_string(),
            Question::score(
                "How risky is enabling docker for `task`?",
                ["negligible", "moderate", "high"],
            ),
        ),
    ]);

    let resp = client.evaluate(state, questions).await.expect("live call");
    println!("model answered: {} | usage: {:?}", resp.model, resp.usage);
    println!("{}", serde_json::to_string_pretty(&resp.answers).unwrap());

    match &resp.answers["mutation"] {
        Answer::Choice {
            choice, confidence, ..
        } => {
            assert_eq!(choice, "m_docker_postgres", "confidence {confidence}");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(&resp.answers["needs_cargo"], Answer::Noul { noul } if *noul > 0.5));
}
