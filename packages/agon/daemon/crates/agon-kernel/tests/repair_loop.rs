//! Boucle de réparation de bout en bout (§44), avec un faux Jev et un runner scripté.

use agon_core::fixtures::{base_form, catalog, integration_check};
use agon_core::mutation::{Op, OpKind, Target};
use agon_core::{Check, CheckId, Form};
use agon_kernel::{
    CandidateCatalog, CandidateRule, EventKind, HumanReason, Kernel, KernelConfig, KernelParts,
    NoopActivator, Outcome, Proof, Session,
};
use agon_model::jev::{Answer, JevError, Question, Response, Usage};
use agon_model::{DecisionProvider, ReplayProvider};
use agon_morph::Registry;
use agon_policy::Budget;
use agon_verify::{CheckRunner, ExtractorSet, Observation};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// ── doubles de test ────────────────────────────────────────────────────────────────────────

/// Rejoue des codes de sortie ; le dernier se répète. Un échec écrit l'erreur Postgres sur stderr.
struct Script {
    codes: Vec<i32>,
    next: Mutex<usize>,
}

fn script(codes: &[i32]) -> Script {
    Script {
        codes: codes.to_vec(),
        next: Mutex::new(0),
    }
}

impl CheckRunner for Script {
    fn run(&self, check: &Check) -> Observation {
        let mut i = self.next.lock().unwrap();
        let code = self.codes[(*i).min(self.codes.len() - 1)];
        *i += 1;
        Observation {
            check: check.id.clone(),
            exit_code: Some(code),
            stdout: String::new(),
            stderr: if code != 0 {
                "ECONNREFUSED 127.0.0.1:5432".into()
            } else {
                String::new()
            },
            timed_out: false,
        }
    }
}

#[derive(Clone)]
enum Mode {
    Pick(&'static str),
    Fail,
}

/// Faux Jev : choisit l'option dont la description contient `pick` (ou `none` / `human`).
#[derive(Clone)]
struct FakeJev {
    mode: Mode,
    confidence: f64,
    model: String,
    calls: Arc<AtomicU32>,
}

impl FakeJev {
    fn picking(pick: &'static str) -> Self {
        FakeJev {
            mode: Mode::Pick(pick),
            confidence: 0.9,
            model: "jev-1.13.0".into(),
            calls: Arc::new(AtomicU32::new(0)),
        }
    }
    fn failing() -> Self {
        FakeJev {
            mode: Mode::Fail,
            ..Self::picking("")
        }
    }
    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
    fn respond(&self, questions: &BTreeMap<String, Question>) -> Result<Response, JevError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let Mode::Pick(pick) = &self.mode else {
            return Err(JevError::Unavailable {
                attempts: 4,
                last: "http 503".into(),
            });
        };
        let Question::Choice { criteria, .. } = &questions["mutation"] else {
            panic!("mutation must be a choice")
        };
        let choice = if matches!(*pick, "none" | "human") {
            pick.to_string()
        } else {
            criteria
                .iter()
                .find(|(_, d)| {
                    d.as_ref()
                        .and_then(|v| v.as_str())
                        .is_some_and(|t| t.contains(pick))
                })
                .map(|(k, _)| k.clone())
                .unwrap_or_else(|| panic!("no candidate mentions `{pick}`: {criteria:?}"))
        };
        Ok(Response {
            id: None,
            model: self.model.clone(),
            provider: None,
            answers: BTreeMap::from([(
                "mutation".to_string(),
                Answer::Choice {
                    choice: choice.clone(),
                    probabilities: BTreeMap::from([(choice, 1.0)]),
                    confidence: self.confidence,
                },
            )]),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cost: None,
            },
        })
    }
}

impl DecisionProvider for FakeJev {
    fn decide(
        &self,
        _state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> impl Future<Output = Result<Response, JevError>> + Send {
        let r = self.respond(&questions);
        async move { r }
    }
}

// ── montage ────────────────────────────────────────────────────────────────────────────────

const PG_EXTRACTOR: &str = r#"
[extractor.pg]
check = "verify.integration"
class = "connection_refused"
priority = 100
[[extractor.pg.match]]
source = "exit_code"
operator = "not_equals"
value = 0
[[extractor.pg.match]]
source = "stderr"
regex = 'ECONNREFUSED [0-9.]+:(?P<port>\d+)'
[extractor.pg.fields]
port = { from = "port", type = "integer" }
service = { lookup = "port", table = { "5432" = "postgres" } }
"#;

/// Form de départ : `verify.integration` actif mais ni docker ni postgres.
fn failing_form() -> Form {
    let mut f = base_form();
    f.checks.insert(
        integration_check().id,
        integration_check().definition_hash(),
    );
    f
}

fn dep_candidates() -> CandidateCatalog {
    CandidateCatalog::new(vec![], ["docker".to_string(), "postgres".to_string()])
}

type TestKernel<D> = Kernel<D, Script, NoopActivator>;

fn kernel_with<D: DecisionProvider>(
    decisions: D,
    codes: &[i32],
    form: Form,
    registry: Registry,
    candidates: CandidateCatalog,
    config: KernelConfig,
    session: Session,
) -> TestKernel<D> {
    Kernel::new(KernelParts {
        decisions,
        runner: script(codes),
        activator: NoopActivator,
        config,
        catalog: catalog(),
        extractors: ExtractorSet::from_toml(PG_EXTRACTOR).unwrap(),
        candidates,
        registry,
        form,
        session,
    })
    .unwrap()
}

fn kernel<D: DecisionProvider>(decisions: D, codes: &[i32]) -> TestKernel<D> {
    kernel_with(
        decisions,
        codes,
        failing_form(),
        Registry::new(),
        dep_candidates(),
        KernelConfig::default(),
        Session::in_memory("t"),
    )
}

fn integration() -> CheckId {
    "verify.integration".into()
}

fn assert_in_order(kinds: &[String], expected: &[&str]) {
    let mut it = kinds.iter();
    for e in expected {
        assert!(
            it.any(|k| k == e),
            "expected `{e}` (in this order) in {kinds:?}"
        );
    }
}

fn count(kinds: &[String], kind: &str) -> usize {
    kinds.iter().filter(|k| *k == kind).count()
}

/// Exécute une tâche qui aboutit (Validated) et renvoie le registre appris.
async fn learn() -> Registry {
    let mut k = kernel(FakeJev::picking("docker"), &[1, 1, 0, 1]);
    let out = k.repair(&integration()).await.unwrap();
    assert!(
        matches!(
            out,
            Outcome::Fixed {
                proof: Proof::Validated,
                ..
            }
        ),
        "{out:?}"
    );
    k.into_registry()
}

// ── scénarios ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scenario_2_fail_recheck_jev_mutation_pass_validated() {
    let jev = FakeJev::picking("docker");
    let mut k = kernel(jev.clone(), &[1, 1, 0, 1]);
    let root = k.history().current_id().clone();

    let out = k.repair(&integration()).await.unwrap();
    let Outcome::Fixed {
        proof: Proof::Validated,
        form,
        ..
    } = out
    else {
        panic!("{out:?}")
    };

    assert_eq!(jev.calls(), 1);
    assert_eq!(k.registry().len(), 1, "a validated mutation is remembered");
    assert_ne!(form, root);
    assert_eq!(
        k.history().current_id(),
        &form,
        "the candidate stays active"
    );
    assert!(k.history().current().capabilities.contains("postgres"));

    let kinds = k.session().kinds();
    assert_in_order(
        &kinds,
        &[
            "session.started",
            "form.committed",
            "verification.failed", // initial
            "verification.failed", // recheck reproductible
            "signature.detected",
            "mutation.proposed",
            "decision.requested",
            "decision.completed",
            "mutation.approved",
            "form.committed",
            "mutation.applied",
            "verification.passed", // candidate
            "verification.failed", // parent recheck : toujours en échec
            "mutation.validated",
            "memory.created",
            "task.completed",
        ],
    );

    // Chaque événement de vérification est rattaché à la Form sur laquelle il a réellement couru.
    let parent_recheck = k.session().events().iter().rev().find(|e| matches!(&e.kind, EventKind::VerificationFailed { phase, .. } if *phase == agon_kernel::Phase::ParentRecheck)).unwrap();
    assert_eq!(parent_recheck.form.as_ref(), Some(&root));
}

#[tokio::test]
async fn scenario_5_known_signature_is_reused_without_jev() {
    let registry = learn().await;
    let jev = FakeJev::picking("docker");
    let mut k = kernel_with(
        jev.clone(),
        &[1, 1, 0],
        failing_form(),
        registry,
        dep_candidates(),
        KernelConfig::default(),
        Session::in_memory("t"),
    );

    let out = k.repair(&integration()).await.unwrap();
    assert!(
        matches!(
            out,
            Outcome::Fixed {
                proof: Proof::Reused,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(jev.calls(), 0, "Jev is not needed for a known signature");
    let kinds = k.session().kinds();
    assert_eq!(count(&kinds, "decision.requested"), 0);
    assert_eq!(
        count(&kinds, "memory.created"),
        0,
        "a reuse teaches nothing new"
    );
}

#[tokio::test]
async fn flaky_failure_never_mutates() {
    let jev = FakeJev::picking("docker");
    let mut k = kernel(jev.clone(), &[1, 0]);
    assert_eq!(k.repair(&integration()).await.unwrap(), Outcome::Flaky);
    assert_eq!(jev.calls(), 0);
    let kinds = k.session().kinds();
    assert_eq!(count(&kinds, "mutation.proposed"), 0);
    assert_eq!(count(&kinds, "signature.detected"), 0);
}

#[tokio::test]
async fn an_already_passing_check_is_left_alone() {
    let mut k = kernel(FakeJev::picking("docker"), &[0]);
    assert_eq!(k.repair(&integration()).await.unwrap(), Outcome::Passed);
}

#[tokio::test]
async fn scenario_3_forbidden_mutation_is_denied_before_jev_sees_it() {
    // Une règle de plugin propose de retirer docker alors que verify.integration en dépend.
    let rule = CandidateRule {
        signature_class: "connection_refused".into(),
        when: BTreeMap::new(),
        propose: Op {
            op: OpKind::Remove,
            target: Target::Capabilities,
            value: vec!["docker".into()],
        },
    };
    let mut form = failing_form();
    form.capabilities
        .extend(["docker".to_string(), "postgres".to_string()]);
    let jev = FakeJev::picking("docker");
    let mut k = kernel_with(
        jev.clone(),
        &[1],
        form,
        Registry::new(),
        CandidateCatalog::new(vec![rule], []),
        KernelConfig::default(),
        Session::in_memory("t"),
    );

    let out = k.repair(&integration()).await.unwrap();
    assert_eq!(
        out,
        Outcome::HumanRequired {
            why: HumanReason::NoCandidate
        }
    );
    assert_eq!(
        jev.calls(),
        0,
        "Jev must not be called when Policy leaves no candidate"
    );
    let rejected = k.session().events().iter().find_map(|e| match &e.kind {
        EventKind::MutationRejected { reasons, .. } => Some(reasons.clone()),
        _ => None,
    });
    assert!(
        rejected
            .unwrap()
            .iter()
            .any(|r| r.contains("BrokenDependency") && r.contains("docker"))
    );
}

#[tokio::test]
async fn scenario_4_a_mutation_that_did_not_help_is_unproven_and_not_learned() {
    let mut k = kernel(FakeJev::picking("docker"), &[1, 1, 0, 0]); // le parent repasse aussi
    let out = k.repair(&integration()).await.unwrap();
    assert!(
        matches!(
            out,
            Outcome::Fixed {
                proof: Proof::Unproven,
                ..
            }
        ),
        "{out:?}"
    );
    assert!(k.registry().is_empty());
    assert_eq!(count(&k.session().kinds(), "mutation.unproven"), 1);
}

#[tokio::test]
async fn scenario_9_low_confidence_stops_without_applying_anything() {
    let mut jev = FakeJev::picking("docker");
    jev.confidence = 0.5;
    let mut k = kernel(jev, &[1]);
    let root = k.history().current_id().clone();

    let out = k.repair(&integration()).await.unwrap();
    assert!(
        matches!(out, Outcome::HumanRequired { why: HumanReason::LowConfidence { required, .. } } if required == 0.75),
        "{out:?}"
    );
    assert_eq!(k.history().current_id(), &root, "no mutation applied");
    let kinds = k.session().kinds();
    assert_eq!(count(&kinds, "decision.escalated"), 1);
    assert_eq!(count(&kinds, "mutation.applied"), 0);
}

#[tokio::test]
async fn jev_choosing_none_or_human_applies_nothing() {
    for (pick, expected) in [
        ("none", Outcome::NoChange),
        (
            "human",
            Outcome::HumanRequired {
                why: HumanReason::JevRequestedHuman,
            },
        ),
    ] {
        let mut k = kernel(FakeJev::picking(pick), &[1]);
        assert_eq!(k.repair(&integration()).await.unwrap(), expected);
        assert_eq!(count(&k.session().kinds(), "mutation.applied"), 0);
    }
}

#[tokio::test]
async fn scenario_10_jev_unavailable_degrades_to_human_required() {
    let mut k = kernel(FakeJev::failing(), &[1]);
    let out = k.repair(&integration()).await.unwrap();
    assert!(
        matches!(
            out,
            Outcome::HumanRequired {
                why: HumanReason::JevUnavailable { .. }
            }
        ),
        "{out:?}"
    );
    assert_eq!(count(&k.session().kinds(), "decision.unavailable"), 1);
}

#[tokio::test]
async fn scenario_10_a_known_mutation_still_works_when_jev_is_down() {
    let registry = learn().await;
    let mut k = kernel_with(
        FakeJev::failing(),
        &[1, 1, 0],
        failing_form(),
        registry,
        dep_candidates(),
        KernelConfig::default(),
        Session::in_memory("t"),
    );
    let out = k.repair(&integration()).await.unwrap();
    assert!(
        matches!(
            out,
            Outcome::Fixed {
                proof: Proof::Reused,
                ..
            }
        ),
        "{out:?}"
    );
}

#[tokio::test]
async fn a_failing_candidate_is_rolled_back_and_not_retried() {
    let mut k = kernel(FakeJev::picking("docker"), &[1]); // tout échoue, y compris la candidate
    let root = k.history().current_id().clone();

    let out = k.repair(&integration()).await.unwrap();
    assert_eq!(
        out,
        Outcome::HumanRequired {
            why: HumanReason::NoCandidate
        },
        "the only candidate was already tried"
    );
    assert_eq!(k.history().current_id(), &root, "rolled back to the parent");
    assert_eq!(k.budget_state().mutations_applied, 1);
    assert_eq!(k.budget_state().consecutive_failures, 1);
    assert_eq!(count(&k.session().kinds(), "mutation.rollback"), 1);
    assert!(k.registry().is_empty());
}

#[tokio::test]
async fn the_mutation_budget_stops_the_loop() {
    // Deux candidats distincts ; le budget n'en autorise qu'un.
    let extra = CandidateRule {
        signature_class: "connection_refused".into(),
        when: BTreeMap::new(),
        propose: Op {
            op: OpKind::Add,
            target: Target::Tools,
            value: vec!["psql".into()],
        },
    };
    let config = KernelConfig {
        budget: Budget {
            max_mutations_per_task: 1,
            ..Budget::default()
        },
        ..KernelConfig::default()
    };
    let mut k = kernel_with(
        FakeJev::picking("docker"),
        &[1],
        failing_form(),
        Registry::new(),
        CandidateCatalog::new(vec![extra], ["docker".to_string(), "postgres".to_string()]),
        config,
        Session::in_memory("t"),
    );
    let out = k.repair(&integration()).await.unwrap();
    assert_eq!(
        out,
        Outcome::HumanRequired {
            why: HumanReason::BudgetExhausted {
                ceiling: "max_mutations_per_task".into()
            }
        }
    );
}

#[tokio::test]
async fn jev_version_drift_is_flagged_in_the_journal() {
    let mut jev = FakeJev::picking("docker");
    jev.model = "jev-1.14.0".into(); // le lockfile attend jev-1.13.0
    let mut k = kernel(jev, &[1, 1, 0, 1]);
    k.repair(&integration()).await.unwrap();
    let drift = k.session().events().iter().find_map(|e| match &e.kind {
        EventKind::DecisionCompleted {
            model_drift,
            jev_model_answered,
            ..
        } => Some((*model_drift, jev_model_answered.clone())),
        _ => None,
    });
    assert_eq!(drift, Some((true, "jev-1.14.0".to_string())));
}

#[tokio::test]
async fn scenario_7_a_session_replays_exactly_from_disk_without_calling_jev() {
    let dir = std::env::temp_dir().join(format!("agon-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // 1. Session réelle, persistée.
    let jev = FakeJev::picking("docker");
    let mut k = kernel_with(
        jev.clone(),
        &[1, 1, 0, 1],
        failing_form(),
        Registry::new(),
        dep_candidates(),
        KernelConfig::default(),
        Session::create(&dir, "s1").unwrap(),
    );
    let original = k.repair(&integration()).await.unwrap();
    let original_form = k.history().current_id().clone();
    let path = k.session().path().unwrap().to_path_buf();
    drop(k);
    assert_eq!(jev.calls(), 1);

    // 2. Relecture : la session seule suffit à reconstruire les Forms et les échanges Jev.
    let recorded = Session::load(&path).unwrap();
    assert!(
        recorded.forms().iter().any(|f| f.id() == original_form),
        "the final Form is reconstructible from the journal"
    );
    let replay = ReplayProvider::new(recorded.exchanges());
    assert_eq!(replay.remaining(), 1);

    // 3. Rejeu : mêmes décisions, même résultat, même Form, aucun appel réseau.
    let mut k2 = kernel(replay, &[1, 1, 0, 1]);
    let replayed = k2.repair(&integration()).await.unwrap();
    assert_eq!(replayed, original);
    assert_eq!(k2.history().current_id(), &original_form);
    assert_eq!(jev.calls(), 1, "the replay must not query Jev");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn unknown_check_is_an_error_not_a_panic() {
    let mut k = kernel(FakeJev::picking("docker"), &[0]);
    assert!(k.repair(&"verify.nope".into()).await.is_err());
}
