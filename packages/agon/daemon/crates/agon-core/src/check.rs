//! Checks : unités fondamentales de Verification (§12).

use crate::canonical::{Digest, sha256_of};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Catalogue des checks définis (projet + plugins), indexé par identifiant.
pub type CheckCatalog = BTreeMap<CheckId, Check>;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckId(pub String);

impl std::fmt::Display for CheckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for CheckId {
    fn from(s: &str) -> Self {
        CheckId(s.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub id: CheckId,
    pub command: String,
    /// Capacités/outils dont dépend le check (graphe analysé par Policy, §14).
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Condition de succès, ex. `exit_code == 0`.
    pub success: String,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default = "one")]
    pub version: u32,
}

fn one() -> u32 {
    1
}

impl Check {
    /// Hash de la définition canonique : c'est ce hash qui est référencé par la Form.
    pub fn definition_hash(&self) -> Digest {
        let mut c = self.clone();
        c.dependencies.sort();
        c.outputs.sort();
        sha256_of(&c).expect("check is always serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(deps: &[&str]) -> Check {
        Check {
            id: "verify.integration".into(),
            command: "cargo test --test integration".into(),
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            success: "exit_code == 0".into(),
            outputs: vec![],
            version: 1,
        }
    }

    #[test]
    fn dependency_order_is_irrelevant() {
        assert_eq!(
            check(&["cargo", "docker"]).definition_hash(),
            check(&["docker", "cargo"]).definition_hash()
        );
    }

    #[test]
    fn command_change_changes_hash() {
        let mut b = check(&["cargo"]);
        b.command.push_str(" -- --ignored");
        assert_ne!(check(&["cargo"]).definition_hash(), b.definition_hash());
    }
}
