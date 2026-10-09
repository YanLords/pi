//! Lockfile de Form (§11) : versions résolues et précision de chaque entrée.

use crate::canonical::Digest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `Exact` : version garantie identique. `BestEffort` : seul l'identifiant déclaré l'est (§11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    Exact,
    BestEffort,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    pub version: String,
    pub precision: Precision,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLock {
    pub provider: String,
    /// Identifiant versionné (`jev-1.13.0`), jamais un alias mobile.
    pub model: String,
    pub precision: Precision,
    pub question_sets: BTreeMap<String, Digest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub agon: String,
    pub morph_schema: String,
    #[serde(default)]
    pub modules: BTreeMap<String, LockEntry>,
    #[serde(default)]
    pub tools: BTreeMap<String, LockEntry>,
    pub model: Option<LockEntry>,
    pub decision: Option<DecisionLock>,
    #[serde(default)]
    pub checks: BTreeMap<String, Digest>,
}

/// Une divergence entre deux lockfiles (§11.2).
#[derive(Debug, PartialEq, Eq)]
pub struct Divergence {
    pub path: String,
    pub recorded: String,
    pub resolved: String,
}

impl Lockfile {
    /// Compare un lockfile enregistré à un lockfile résolu ; toute différence est signalée,
    /// y compris sur les entrées `best_effort`.
    pub fn diverges_from(&self, resolved: &Lockfile) -> Vec<Divergence> {
        let mut out = Vec::new();
        let mut push = |path: String, a: String, b: String| {
            if a != b {
                out.push(Divergence {
                    path,
                    recorded: a,
                    resolved: b,
                });
            }
        };
        push("agon".into(), self.agon.clone(), resolved.agon.clone());
        push(
            "morph_schema".into(),
            self.morph_schema.clone(),
            resolved.morph_schema.clone(),
        );
        for (section, a, b) in [
            ("modules", &self.modules, &resolved.modules),
            ("tools", &self.tools, &resolved.tools),
        ] {
            for k in a
                .keys()
                .chain(b.keys())
                .collect::<std::collections::BTreeSet<_>>()
            {
                push(
                    format!("{section}.{k}"),
                    a.get(k).map(|e| e.version.clone()).unwrap_or_default(),
                    b.get(k).map(|e| e.version.clone()).unwrap_or_default(),
                );
            }
        }
        push(
            "decision.model".into(),
            self.decision
                .as_ref()
                .map(|d| d.model.clone())
                .unwrap_or_default(),
            resolved
                .decision
                .as_ref()
                .map(|d| d.model.clone())
                .unwrap_or_default(),
        );
        for k in self
            .checks
            .keys()
            .chain(resolved.checks.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            push(
                format!("checks.{k}"),
                self.checks
                    .get(k)
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
                resolved
                    .checks
                    .get(k)
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Lockfile {
        Lockfile {
            agon: "0.1.0".into(),
            morph_schema: "1".into(),
            modules: BTreeMap::new(),
            tools: BTreeMap::from([(
                "cargo".to_string(),
                LockEntry {
                    version: "1.93.0".into(),
                    precision: Precision::Exact,
                },
            )]),
            model: None,
            decision: Some(DecisionLock {
                provider: "typesafe".into(),
                model: "jev-1.13.0".into(),
                precision: Precision::Exact,
                question_sets: BTreeMap::new(),
            }),
            checks: BTreeMap::new(),
        }
    }

    #[test]
    fn identical_lockfiles_do_not_diverge() {
        assert!(sample().diverges_from(&sample()).is_empty());
    }

    #[test]
    fn tool_and_jev_version_changes_are_reported() {
        let mut other = sample();
        other.tools.get_mut("cargo").unwrap().version = "1.94.0".into();
        other.decision.as_mut().unwrap().model = "jev-1.14.0".into();
        let d = sample().diverges_from(&other);
        let paths: Vec<_> = d.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, ["tools.cargo", "decision.model"]);
    }
}
