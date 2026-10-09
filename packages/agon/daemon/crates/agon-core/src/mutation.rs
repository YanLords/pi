//! Mutation : diff déclaratif appliqué à une Form (§9).
//!
//! `Target` ne contient que les zones mutables (§8.2). Les zones intouchables
//! (kernel, policy, verification, règles de Morph) ne sont pas représentables :
//! une mutation qui les vise échoue dès la désérialisation.

use crate::check::CheckId;
use crate::form::FormId;
use crate::signature::SignatureId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MutationId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Add,
    Remove,
    Set,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Capabilities,
    Tools,
    Modules,
    Context,
    /// Ajout de références à des checks existants uniquement (§8.2, §13).
    Checks,
    /// Niveau ou identifiant de modèle ; seul `Set` est valide.
    Model,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub op: OpKind,
    pub target: Target,
    pub value: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Task,
    Project,
}

/// Provenance d'un candidat (§9.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Catalog,
    Llm,
    Registry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    pub check: CheckId,
    pub signature: SignatureId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mutation {
    pub id: MutationId,
    pub parent_form: FormId,
    pub scope: Scope,
    pub origin: Origin,
    pub trigger: Trigger,
    /// Check qui doit passer pour valider la mutation.
    pub success_check: CheckId,
    pub ops: Vec<Op>,
}

impl Op {
    /// Cohérence interne : `Model` n'accepte que `Set` avec une valeur unique.
    pub fn is_well_formed(&self) -> bool {
        match self.target {
            Target::Model => self.op == OpKind::Set && self.value.len() == 1,
            _ => !self.value.is_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_targets_do_not_deserialize() {
        for t in [
            "kernel",
            "policy",
            "policies",
            "verification",
            "morph_rules",
            "event_bus",
        ] {
            let json = format!(r#"{{"op":"add","target":"{t}","value":["x"]}}"#);
            assert!(
                serde_json::from_str::<Op>(&json).is_err(),
                "{t} must be rejected"
            );
        }
    }

    #[test]
    fn mutable_target_deserializes() {
        let op: Op = serde_json::from_str(
            r#"{"op":"add","target":"capabilities","value":["docker","postgres"]}"#,
        )
        .unwrap();
        assert!(op.is_well_formed());
    }

    #[test]
    fn model_requires_single_set() {
        let bad = Op {
            op: OpKind::Add,
            target: Target::Model,
            value: vec!["large".into()],
        };
        let good = Op {
            op: OpKind::Set,
            target: Target::Model,
            value: vec!["large".into()],
        };
        assert!(!bad.is_well_formed());
        assert!(good.is_well_formed());
    }
}
