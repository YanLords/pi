use agon_core::mutation::Origin;
use agon_core::{FormId, Mutation, MutationId, Signature, SignatureId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cycle de vie d'une mutation (§20). Seuls les états V0 sont implémentés.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Proposed,
    Denied,
    Approved,
    Trial,
    Validated,
    Unproven,
    Rejected,
    Expired,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("only VALIDATED mutations enter the registry (got {0:?})")]
    NotValidated(Status),
    #[error("an unknown signature is never registered")]
    UnknownSignature,
    #[error("the mutation trigger does not match the signature")]
    TriggerMismatch,
}

/// Registry V0 (§19.1) : `signature_id → mutations validées`, correspondance exacte uniquement.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    entries: BTreeMap<SignatureId, Vec<Mutation>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        signature: &Signature,
        mutation: Mutation,
        status: Status,
    ) -> Result<(), RegistryError> {
        if status != Status::Validated {
            return Err(RegistryError::NotValidated(status));
        }
        if signature.is_unknown() {
            return Err(RegistryError::UnknownSignature);
        }
        if mutation.trigger.signature != signature.id() {
            return Err(RegistryError::TriggerMismatch);
        }
        self.entries
            .entry(signature.id())
            .or_default()
            .push(mutation);
        Ok(())
    }

    /// Mutation la plus récemment validée pour cette signature. Une signature `unknown` ne
    /// correspond jamais (§19.1).
    pub fn lookup(&self, signature: &Signature) -> Option<&Mutation> {
        if signature.is_unknown() {
            return None;
        }
        self.entries.get(&signature.id())?.last()
    }

    /// Mutation réutilisable : la mutation enregistrée, rebasée sur la Form courante.
    pub fn reuse(
        &self,
        signature: &Signature,
        parent: FormId,
        new_id: MutationId,
    ) -> Option<Mutation> {
        let mut m = self.lookup(signature)?.clone();
        m.id = new_id;
        m.parent_form = parent;
        m.origin = Origin::Registry;
        Some(m)
    }

    /// Toutes les mutations enregistrées, groupées par signature, dans l'ordre de validation.
    pub fn iter(&self) -> impl Iterator<Item = (&SignatureId, &Mutation)> {
        self.entries
            .iter()
            .flat_map(|(s, ms)| ms.iter().map(move |m| (s, m)))
    }

    /// Persistance projet : `.agon/mutations/registry.toml` (§19.1, §24).
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    pub fn from_toml(src: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(src)
    }

    pub fn len(&self) -> usize {
        self.entries.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::tests::{mutation, op};
    use agon_core::fixtures::*;
    use agon_core::mutation::{OpKind, Target};
    use serde_json::json;

    fn pg_signature() -> Signature {
        Signature {
            check: "verify.integration".into(),
            class: "connection_refused".into(),
            fields: BTreeMap::from([("port".into(), json!(5432))]),
        }
    }

    fn pg_mutation() -> Mutation {
        let mut m = mutation(
            &base_form(),
            vec![op(
                OpKind::Add,
                Target::Capabilities,
                &["docker", "postgres"],
            )],
        );
        m.trigger.signature = pg_signature().id();
        m
    }

    #[test]
    fn scenario_5_known_signature_yields_rebased_mutation() {
        let mut reg = Registry::new();
        reg.insert(&pg_signature(), pg_mutation(), Status::Validated)
            .unwrap();
        let other_parent = integration_form().id();
        let reused = reg
            .reuse(
                &pg_signature(),
                other_parent.clone(),
                MutationId("m-042".into()),
            )
            .unwrap();
        assert_eq!(reused.parent_form, other_parent);
        assert_eq!(reused.origin, Origin::Registry);
        assert_eq!(reused.ops, pg_mutation().ops);
    }

    #[test]
    fn only_validated_mutations_enter() {
        let mut reg = Registry::new();
        for s in [
            Status::Unproven,
            Status::Rejected,
            Status::Trial,
            Status::Denied,
        ] {
            assert_eq!(
                reg.insert(&pg_signature(), pg_mutation(), s),
                Err(RegistryError::NotValidated(s))
            );
        }
        assert!(reg.is_empty());
    }

    #[test]
    fn unknown_signature_is_never_registered_or_matched() {
        let mut reg = Registry::new();
        let unknown = Signature::unknown("verify.integration".into());
        let mut m = pg_mutation();
        m.trigger.signature = unknown.id();
        assert_eq!(
            reg.insert(&unknown, m, Status::Validated),
            Err(RegistryError::UnknownSignature)
        );
        assert!(reg.lookup(&unknown).is_none());
    }

    #[test]
    fn exact_match_only_and_latest_wins() {
        let mut reg = Registry::new();
        reg.insert(&pg_signature(), pg_mutation(), Status::Validated)
            .unwrap();
        let mut newer = pg_mutation();
        newer.id = MutationId("m-002".into());
        reg.insert(&pg_signature(), newer, Status::Validated)
            .unwrap();
        assert_eq!(
            reg.lookup(&pg_signature()).unwrap().id,
            MutationId("m-002".into())
        );

        let mut other = pg_signature();
        other.fields.insert("port".into(), json!(6379));
        assert!(reg.lookup(&other).is_none());
    }

    #[test]
    fn registry_survives_a_toml_roundtrip() {
        let mut reg = Registry::new();
        reg.insert(&pg_signature(), pg_mutation(), Status::Validated)
            .unwrap();
        let back = Registry::from_toml(&reg.to_toml().unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.lookup(&pg_signature()), reg.lookup(&pg_signature()));
        assert!(Registry::from_toml("").unwrap().is_empty());
    }

    #[test]
    fn trigger_must_match_signature() {
        let mut reg = Registry::new();
        let mut m = pg_mutation();
        m.trigger.signature = Signature::unknown("verify.build".into()).id();
        assert_eq!(
            reg.insert(&pg_signature(), m, Status::Validated),
            Err(RegistryError::TriggerMismatch)
        );
    }
}
