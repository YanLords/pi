//! Jeux de données partagés par les tests des autres crates (feature `test-util`).

use crate::{Check, CheckCatalog, Form, Lockfile};
use std::collections::{BTreeMap, BTreeSet};

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

pub fn build_check() -> Check {
    Check {
        id: "verify.build".into(),
        command: "cargo build".into(),
        dependencies: vec!["cargo".into()],
        success: "exit_code == 0".into(),
        outputs: vec![],
        version: 1,
    }
}

pub fn integration_check() -> Check {
    Check {
        id: "verify.integration".into(),
        command: "cargo test --test integration".into(),
        dependencies: vec!["cargo".into(), "docker".into(), "postgres".into()],
        success: "exit_code == 0".into(),
        outputs: vec![],
        version: 1,
    }
}

pub fn catalog() -> CheckCatalog {
    [build_check(), integration_check()]
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect()
}

fn lock() -> Lockfile {
    Lockfile {
        agon: "0.1.0".into(),
        morph_schema: "1".into(),
        modules: BTreeMap::new(),
        tools: BTreeMap::new(),
        model: None,
        decision: None,
        checks: BTreeMap::new(),
    }
}

/// Form de départ : Rust + cargo, aucune capacité, seul `verify.build` actif.
pub fn base_form() -> Form {
    Form {
        configuration: BTreeMap::new(),
        modules: set(&["rust"]),
        tools: set(&["cargo"]),
        capabilities: BTreeSet::new(),
        model: "medium".into(),
        context: BTreeSet::new(),
        checks: BTreeMap::from([(build_check().id.clone(), build_check().definition_hash())]),
        policies: crate::Digest::of_bytes(b"policies-v1"),
        lock: lock(),
    }
}

/// Form avec docker + postgres actifs et `verify.integration` en plus.
pub fn integration_form() -> Form {
    let mut f = base_form();
    f.capabilities = set(&["docker", "postgres"]);
    f.checks.insert(
        integration_check().id.clone(),
        integration_check().definition_hash(),
    );
    f
}
