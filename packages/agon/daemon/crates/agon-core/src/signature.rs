//! Signature : forme normalisée d'un échec (§18).

use crate::canonical::{Digest, sha256_of};
use crate::check::CheckId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const UNKNOWN_CLASS: &str = "unknown";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SignatureId(pub Digest);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub check: CheckId,
    pub class: String,
    /// Champs normalisés (service, port…), triés pour un hash stable.
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl Signature {
    pub fn unknown(check: CheckId) -> Self {
        Signature {
            check,
            class: UNKNOWN_CLASS.into(),
            fields: BTreeMap::new(),
        }
    }

    pub fn is_unknown(&self) -> bool {
        self.class == UNKNOWN_CLASS
    }

    pub fn id(&self) -> SignatureId {
        SignatureId(sha256_of(self).expect("signature is always serializable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pg() -> Signature {
        Signature {
            check: "verify.integration".into(),
            class: "connection_refused".into(),
            fields: BTreeMap::from([
                ("service".into(), json!("postgres")),
                ("port".into(), json!(5432)),
            ]),
        }
    }

    #[test]
    fn same_signature_same_id() {
        assert_eq!(pg().id(), pg().id());
    }

    #[test]
    fn port_change_changes_id() {
        let mut other = pg();
        other.fields.insert("port".into(), json!(6379));
        assert_ne!(pg().id(), other.id());
    }

    #[test]
    fn unknown_is_flagged() {
        assert!(Signature::unknown("verify.build".into()).is_unknown());
        assert!(!pg().is_unknown());
    }
}
