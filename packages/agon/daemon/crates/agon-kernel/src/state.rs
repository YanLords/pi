//! State envoyé à Jev (§6.2, §39.2) : construit et filtré par Agon, jamais un dump brut.

use agon_core::{Check, Form, Signature};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Ne contient que : la tâche, le check, la signature normalisée, le résumé de la Form et les
/// dépendances manquantes. Ni stdout, ni stderr, ni variables d'environnement, ni chemins.
pub fn decision_state(check: &Check, sig: &Signature, form: &Form) -> Value {
    let provided: BTreeSet<&String> = form
        .capabilities
        .iter()
        .chain(&form.tools)
        .chain(&form.modules)
        .collect();
    let missing: Vec<&String> = check
        .dependencies
        .iter()
        .filter(|d| !provided.contains(d))
        .collect();
    json!({
        "task": format!("Make check `{}` pass", check.id),
        "check": check.id,
        "signature": sig,
        "form": {
            "modules": form.modules,
            "tools": form.tools,
            "capabilities": form.capabilities,
        },
        "missing_dependencies": missing,
    })
}

fn strip(id: &str) -> &str {
    id.trim_start_matches('~').trim_start_matches("typesafe/")
}

/// La version qui a répondu correspond-elle à celle demandée ? (§11.2)
/// `jev-1.13` ↔ `typesafe/jev-1.13-20260917` correspond ; un alias `…-latest` accepte toute version
/// (c'est alors la version renvoyée qui fait foi).
pub fn model_matches(requested: &str, answered: &str) -> bool {
    let (req, ans) = (strip(requested), strip(answered));
    req.ends_with("latest")
        || ans == req
        || ans
            .strip_prefix(req)
            .is_some_and(|rest| rest.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agon_core::fixtures::{base_form, integration_check};

    #[test]
    fn state_carries_signature_and_missing_dependencies_but_no_raw_output() {
        let sig = Signature::unknown("verify.integration".into());
        let s = decision_state(&integration_check(), &sig, &base_form());
        assert_eq!(s["check"], "verify.integration");
        assert_eq!(s["signature"]["class"], "unknown");
        assert_eq!(s["missing_dependencies"], json!(["docker", "postgres"]));
        let text = s.to_string();
        for forbidden in ["stdout", "stderr", "env", "/Users"] {
            assert!(
                !text.contains(forbidden),
                "state must not contain `{forbidden}`"
            );
        }
    }

    #[test]
    fn model_matching_handles_dated_versions_prefixes_and_aliases() {
        assert!(model_matches("jev-1.13.0", "jev-1.13.0"));
        assert!(model_matches(
            "typesafe/jev-1.13",
            "typesafe/jev-1.13-20260917"
        ));
        assert!(model_matches("jev-1.13", "typesafe/jev-1.13-20260917"));
        assert!(model_matches(
            "~typesafe/jev-latest",
            "typesafe/jev-1.13-20260917"
        ));
        assert!(!model_matches(
            "typesafe/jev-1.13",
            "typesafe/jev-1.14-20261101"
        ));
        assert!(
            !model_matches("jev-1.1", "jev-1.13.0"),
            "prefix must end at a `-` boundary"
        );
    }
}
