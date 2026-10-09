//! Scénarios d'acceptation V0 (§44) exécutés avec de vrais processus.
//! L'activation d'une Form (démarrer Postgres) est simulée par un fichier marqueur, en attendant
//! les capability providers ; tout le reste (checks, extracteurs, Morph, Policy, registre) est réel.

use agon_core::fixtures::{base_form, catalog};
use agon_core::mutation::{Op, OpKind, Origin, Scope, Target, Trigger};
use agon_core::{CheckCatalog, CheckId, Form, Mutation, MutationId, Signature};
use agon_kernel::ProcessRunner;
use agon_morph::{Registry, Status, apply};
use agon_policy::{Budget, BudgetState, Evaluation, Verdict, evaluate};
use agon_verify::{
    Causality, Evidence, ExtractorSet, Reproduction, causality, reproduction, run_check,
};
use std::path::{Path, PathBuf};

const EXTRACTORS: &str = r#"
[extractor.pg_connection_refused]
check = "verify.integration"
class = "connection_refused"
priority = 100
[[extractor.pg_connection_refused.match]]
source = "exit_code"
operator = "not_equals"
value = 0
[[extractor.pg_connection_refused.match]]
source = "stderr"
regex = 'ECONNREFUSED [0-9.]+:(?P<port>\d+)'
[extractor.pg_connection_refused.fields]
port = { from = "port", type = "integer" }
service = { lookup = "port", table = { "5432" = "postgres" } }
"#;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agon-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Simule l'activation : Postgres « tourne » si et seulement si la Form active la capacité.
fn activate(form: &Form, dir: &Path) {
    let marker = dir.join("pg.up");
    if form.capabilities.contains("postgres") {
        std::fs::write(marker, "").unwrap();
    } else {
        let _ = std::fs::remove_file(marker);
    }
}

fn setup(dir: &Path) -> (CheckCatalog, Form) {
    let mut cat = catalog();
    let integ = cat.get_mut(&CheckId::from("verify.integration")).unwrap();
    integ.command = format!(
        "test -f {}/pg.up || {{ echo 'ECONNREFUSED 127.0.0.1:5432' >&2; exit 1; }}",
        dir.display()
    );
    let mut parent = base_form();
    parent
        .checks
        .insert(integ.id.clone(), integ.definition_hash());
    (cat, parent)
}

fn candidate_mutation(parent: &Form, sig: &Signature, id: &str, origin: Origin) -> Mutation {
    Mutation {
        id: MutationId(id.into()),
        parent_form: parent.id(),
        scope: Scope::Task,
        origin,
        trigger: Trigger {
            check: sig.check.clone(),
            signature: sig.id(),
        },
        success_check: "verify.integration".into(),
        ops: vec![Op {
            op: OpKind::Add,
            target: Target::Capabilities,
            value: vec!["docker".into(), "postgres".into()],
        }],
    }
}

fn run(runner: &ProcessRunner, cat: &CheckCatalog) -> Evidence {
    run_check(runner, &cat[&CheckId::from("verify.integration")]).unwrap()
}

#[test]
fn learn_a_mutation_then_reuse_it_without_jev() {
    let dir = scratch("learn");
    let (cat, parent) = setup(&dir);
    let runner = ProcessRunner::new(&dir);
    let extractors = ExtractorSet::from_toml(EXTRACTORS).unwrap();
    let mut registry = Registry::new();
    let (budget, state) = (Budget::default(), BudgetState::default());

    // ── Scénario 2 : FAIL → RECHECK → FAIL reproductible ────────────────────────────────────
    activate(&parent, &dir);
    let first = run(&runner, &cat);
    assert!(!first.passed);
    let second = run(&runner, &cat);
    assert_eq!(reproduction(&second), Reproduction::Reproducible);

    // ── Observation → signature (déterministe) ; inconnue du registre → il faudrait Jev ─────
    let sig = extractors.normalize(&second.observation);
    assert_eq!(sig.class, "connection_refused");
    assert!(registry.lookup(&sig).is_none());

    // ── Candidat (choisi par Jev en vrai ; ici imposé) → Morph → Policy ─────────────────────
    let m = candidate_mutation(&parent, &sig, "m-001", Origin::Catalog);
    let child = apply(&parent, &m, &cat).unwrap();
    let verdict = evaluate(&Evaluation {
        parent: &parent,
        candidate: &child,
        mutation: &m,
        catalog: &cat,
        equivalences: &[],
        budget: &budget,
        state: &state,
    });
    assert_eq!(verdict, Verdict::Allow);

    // ── Candidate PASS ──────────────────────────────────────────────────────────────────────
    activate(&child, &dir);
    assert!(run(&runner, &cat).passed);

    // ── Recheck du parent : toujours FAIL → causalité prouvée → VALIDATED → registre ────────
    activate(&parent, &dir);
    let parent_recheck = run(&runner, &cat);
    assert_eq!(causality(&parent_recheck), Causality::Proven);
    registry.insert(&sig, m.clone(), Status::Validated).unwrap();

    // ── Scénario 5 : même signature plus tard → mutation connue, sans Jev ───────────────────
    activate(&parent, &dir);
    let again = run(&runner, &cat);
    assert!(!again.passed);
    let sig2 = extractors.normalize(&again.observation);
    assert_eq!(sig2.id(), sig.id());
    let reused = registry
        .reuse(&sig2, parent.id(), MutationId("m-002".into()))
        .expect("known mutation");
    assert_eq!(reused.origin, Origin::Registry);
    let child2 = apply(&parent, &reused, &cat).unwrap();
    assert_eq!(
        child2.id(),
        child.id(),
        "same mutation, same parent ⇒ same Form"
    );
    activate(&child2, &dir);
    assert!(run(&runner, &cat).passed);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scenario_4_failure_that_fixes_itself_is_flaky_or_unproven() {
    let dir = scratch("unproven");
    let (cat, parent) = setup(&dir);
    let runner = ProcessRunner::new(&dir);

    // FAIL puis PASS sur la même Form : flaky, aucune mutation ne doit être proposée.
    activate(&parent, &dir);
    assert!(!run(&runner, &cat).passed);
    std::fs::write(dir.join("pg.up"), "").unwrap(); // l'environnement se répare tout seul
    assert_eq!(reproduction(&run(&runner, &cat)), Reproduction::Flaky);

    // Candidate PASS, mais le parent repasse aussi : mutation non prouvée.
    assert_eq!(causality(&run(&runner, &cat)), Causality::Unproven);

    let _ = std::fs::remove_dir_all(&dir);
}
