//! Form : état opérationnel complet et résolu d'Agon (§10).

use crate::canonical::{Digest, sha256_of};
use crate::check::CheckId;
use crate::lock::Lockfile;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// `Form ID = hash(resolved form + lockfile)` ; le lockfile fait partie de la Form.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FormId(pub Digest);

impl FormId {
    /// Forme d'affichage `f-7a3c91be`.
    pub fn display(&self) -> String {
        format!("f-{}", self.0.short())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Form {
    #[serde(default)]
    pub configuration: BTreeMap<String, String>,
    pub modules: BTreeSet<String>,
    pub tools: BTreeSet<String>,
    pub capabilities: BTreeSet<String>,
    /// Niveau de modèle génératif (`small` / `medium` / `large`).
    pub model: String,
    pub context: BTreeSet<String>,
    /// Références (par hash de définition) aux checks actifs : instantané en lecture seule.
    pub checks: BTreeMap<CheckId, Digest>,
    /// Hash de l'instantané des policies : non modifiable par mutation.
    pub policies: Digest,
    pub lock: Lockfile,
}

impl Form {
    pub fn id(&self) -> FormId {
        FormId(sha256_of(self).expect("form is always serializable"))
    }

    pub fn check_ids(&self) -> BTreeSet<&CheckId> {
        self.checks.keys().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Precision;

    pub(crate) fn sample() -> Form {
        Form {
            configuration: BTreeMap::new(),
            modules: BTreeSet::from(["rust".to_string()]),
            tools: BTreeSet::from(["cargo".to_string()]),
            capabilities: BTreeSet::new(),
            model: "medium".into(),
            context: BTreeSet::new(),
            checks: BTreeMap::from([("verify.build".into(), Digest::of_bytes(b"build"))]),
            policies: Digest::of_bytes(b"policies-v1"),
            lock: Lockfile {
                agon: "0.1.0".into(),
                morph_schema: "1".into(),
                modules: BTreeMap::new(),
                tools: BTreeMap::new(),
                model: None,
                decision: None,
                checks: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn id_is_deterministic() {
        assert_eq!(sample().id(), sample().id());
        assert!(sample().id().display().starts_with("f-"));
    }

    #[test]
    fn any_change_changes_id() {
        let mut f = sample();
        f.capabilities.insert("docker".into());
        assert_ne!(f.id(), sample().id());
    }

    #[test]
    fn lockfile_is_part_of_the_id() {
        let mut f = sample();
        f.lock.agon = "0.2.0".into();
        assert_ne!(f.id(), sample().id());
        let _ = Precision::Exact;
    }
}
