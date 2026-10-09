//! Génération déterministe des mutations candidates (§9.2, source 1) : règles fournies par les
//! plugins (`[[candidate_rules]]`, §29.1) et dépendances de check non satisfaites (§14).

use agon_core::mutation::{Op, OpKind, Target};
use agon_core::{Check, Digest, Form, Signature, sha256_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// `[[candidate_rules]]` : si la signature a cette classe et ces champs, proposer ce diff.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateRule {
    pub signature_class: String,
    /// Champs de signature qui doivent être égaux (ex. `{ service = "postgres" }`).
    #[serde(default)]
    pub when: BTreeMap<String, Value>,
    pub propose: Op,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    candidate_rules: Vec<CandidateRule>,
}

#[derive(Clone, Debug, Default)]
pub struct CandidateCatalog {
    rules: Vec<CandidateRule>,
    /// Capacités connues (fournies par les plugins) : seules celles-ci peuvent être proposées
    /// pour combler une dépendance manquante.
    known_capabilities: BTreeSet<String>,
}

impl CandidateCatalog {
    pub fn new(
        rules: Vec<CandidateRule>,
        known_capabilities: impl IntoIterator<Item = String>,
    ) -> Self {
        CandidateCatalog {
            rules,
            known_capabilities: known_capabilities.into_iter().collect(),
        }
    }

    pub fn from_toml(
        src: &str,
        known_capabilities: impl IntoIterator<Item = String>,
    ) -> Result<Self, toml::de::Error> {
        let file: File = toml::from_str(src)?;
        Ok(Self::new(file.candidate_rules, known_capabilities))
    }

    /// Listes d'opérations candidates pour `check` en échec avec la signature `sig` sur `form`.
    /// Résultat trié et dédupliqué : même entrée, même sortie.
    pub fn generate(&self, form: &Form, check: &Check, sig: &Signature) -> Vec<Vec<Op>> {
        let mut out: BTreeMap<Digest, Vec<Op>> = BTreeMap::new();
        let mut push = |ops: Vec<Op>| {
            out.entry(sha256_of(&ops).expect("ops are serializable"))
                .or_insert(ops);
        };

        for r in &self.rules {
            if r.signature_class == sig.class
                && r.when.iter().all(|(k, v)| sig.fields.get(k) == Some(v))
            {
                push(vec![r.propose.clone()]);
            }
        }

        let provided: BTreeSet<&String> = form
            .capabilities
            .iter()
            .chain(&form.tools)
            .chain(&form.modules)
            .collect();
        let missing: Vec<String> = check
            .dependencies
            .iter()
            .filter(|d| !provided.contains(d) && self.known_capabilities.contains(*d))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !missing.is_empty() {
            push(vec![Op {
                op: OpKind::Add,
                target: Target::Capabilities,
                value: missing,
            }]);
        }
        out.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agon_core::fixtures::{base_form, integration_check, integration_form};
    use serde_json::json;

    const RULES: &str = r#"
[[candidate_rules]]
signature_class = "connection_refused"
when = { service = "postgres" }
propose = { op = "add", target = "capabilities", value = ["docker", "postgres"] }

[[candidate_rules]]
signature_class = "connection_refused"
when = { service = "redis" }
propose = { op = "add", target = "capabilities", value = ["redis"] }
"#;

    fn caps(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn sig(service: &str) -> Signature {
        Signature {
            check: "verify.integration".into(),
            class: "connection_refused".into(),
            fields: BTreeMap::from([("service".into(), json!(service))]),
        }
    }

    #[test]
    fn plugin_rules_match_on_class_and_fields() {
        let cat = CandidateCatalog::from_toml(RULES, caps(&[])).unwrap();
        let got = cat.generate(&base_form(), &integration_check(), &sig("postgres"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0][0].value, ["docker", "postgres"]);
        assert!(
            cat.generate(&base_form(), &integration_check(), &sig("mysql"))
                .is_empty()
        );
    }

    #[test]
    fn missing_dependencies_become_a_candidate_when_the_capability_is_known() {
        let cat = CandidateCatalog::new(vec![], caps(&["docker", "postgres"]));
        let got = cat.generate(
            &base_form(),
            &integration_check(),
            &Signature::unknown("verify.integration".into()),
        );
        assert_eq!(
            got,
            vec![vec![Op {
                op: OpKind::Add,
                target: Target::Capabilities,
                value: caps(&["docker", "postgres"])
            }]]
        );
    }

    #[test]
    fn unknown_capabilities_and_satisfied_dependencies_yield_nothing() {
        let unknown = Signature::unknown("verify.integration".into());
        let cat = CandidateCatalog::new(vec![], caps(&["docker"])); // postgres n'est pas connue
        let got = cat.generate(&base_form(), &integration_check(), &unknown);
        assert_eq!(
            got[0][0].value,
            ["docker"],
            "only the known missing capability is proposed"
        );

        let cat = CandidateCatalog::new(vec![], caps(&["docker", "postgres"]));
        assert!(
            cat.generate(&integration_form(), &integration_check(), &unknown)
                .is_empty()
        );
    }

    #[test]
    fn identical_proposals_are_deduplicated() {
        let cat = CandidateCatalog::from_toml(RULES, caps(&["docker", "postgres"])).unwrap();
        // La règle Postgres et la dépendance manquante proposent le même diff : un seul candidat.
        let got = cat.generate(&base_form(), &integration_check(), &sig("postgres"));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn generation_is_deterministic() {
        let cat =
            CandidateCatalog::from_toml(RULES, caps(&["docker", "postgres", "redis"])).unwrap();
        let a = cat.generate(&base_form(), &integration_check(), &sig("postgres"));
        let b = cat.generate(&base_form(), &integration_check(), &sig("postgres"));
        assert_eq!(a, b);
    }

    #[test]
    fn a_rule_naming_a_forbidden_target_does_not_load() {
        let bad = RULES.replace("\"capabilities\"", "\"kernel\"");
        assert!(CandidateCatalog::from_toml(&bad, caps(&[])).is_err());
    }
}
