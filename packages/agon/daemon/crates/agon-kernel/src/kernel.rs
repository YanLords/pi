//! Boucle de réparation d'un check (§5, §16, §19, §44) :
//!
//! ```text
//! VERIFY → RECHECK → SIGNATURE → registre ─┬─ connue ─────────────────────┐
//!                                          └─ inconnue → candidats → Policy → Jev
//!                                                          → Policy → Morph → activation
//!                                                          → VERIFY → causalité → registre / rollback
//! ```
//! Toute sortie « pas de solution » est un `HUMAN_REQUIRED` explicite : Agon ne boucle jamais (§23).

mod agent;

use crate::activation::{ActivationError, Activator};
use crate::candidates::CandidateCatalog;
use crate::events::{EventKind, Phase, Session, SessionError};
use crate::state::{decision_state, model_matches};
use agon_core::mutation::{Op, Origin, Scope, Trigger};
use agon_core::{
    Check, CheckCatalog, CheckId, Digest, Form, FormId, Mutation, MutationId, Signature, sha256_of,
};
use agon_model::jev::JevError;
use agon_model::jev::gating::Thresholds;
use agon_model::jev::questions::{self as q, Candidate, MutationChoice};
use agon_model::{DecisionProvider, Exchange, request_digest};
use agon_morph::{History, MorphError, Registry, Status, apply};
use agon_policy::{Budget, BudgetState, Equivalence, Evaluation, Verdict, evaluate};
use agon_verify::{
    Causality, CheckRunner, Evidence, ExtractorSet, Reproduction, VerifyError, causality,
    reproduction, run_check,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use agent::looks_like_plan;
pub use agent::{
    Agent, AgentConfig, CancelToken, ChatOutcome, Conversation, Mode, TaskOutcome, TaskReason,
    Tier, Tiers,
};

/// Pourquoi Agon s'arrête et rend la main (`HUMAN_REQUIRED`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum HumanReason {
    BudgetExhausted {
        ceiling: String,
    },
    /// Aucun candidat n'a survécu à Policy (ou il n'y en a pas) : Jev n'est pas appelé (§9.2).
    NoCandidate,
    JevRequestedHuman,
    LowConfidence {
        candidate: String,
        confidence: f64,
        required: f64,
    },
    /// Jev injoignable après retries (§6.6) et aucune mutation connue applicable.
    JevUnavailable {
        detail: String,
    },
    JevError {
        detail: String,
    },
    ActivationFailed {
        detail: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proof {
    /// Le parent échoue toujours : la mutation est bien la cause (§16.1). Inscrite au registre.
    Validated,
    /// Le parent repasse aussi : l'échec a disparu sans la mutation. Non apprise.
    Unproven,
    /// Mutation déjà validée, rejouée depuis le registre sans Jev (scénario 5).
    Reused,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Le check passait déjà.
    Passed,
    /// FAIL puis PASS sur la même Form : transitoire, aucune mutation (§16).
    Flaky,
    /// Jev estime qu'aucun changement n'est approprié.
    NoChange,
    Fixed {
        mutation: MutationId,
        proof: Proof,
        form: FormId,
    },
    HumanRequired {
        why: HumanReason,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Verify(#[from] VerifyError),
    #[error(transparent)]
    Morph(#[from] MorphError),
    #[error(transparent)]
    Activation(#[from] ActivationError),
    #[error("unknown check `{0}`")]
    UnknownCheck(CheckId),
}

#[derive(Clone, Debug)]
pub struct KernelConfig {
    pub budget: Budget,
    pub thresholds: Thresholds,
    /// Identifiant Jev du lockfile, pour détecter une dérive de version (§11.2).
    pub jev_model: String,
    pub equivalences: Vec<Equivalence>,
}

impl Default for KernelConfig {
    fn default() -> Self {
        KernelConfig {
            budget: Budget::default(),
            thresholds: Thresholds::default(),
            jev_model: "jev-1.13.0".into(),
            equivalences: vec![],
        }
    }
}

pub struct KernelParts<D, R, A> {
    pub decisions: D,
    pub runner: R,
    pub activator: A,
    pub config: KernelConfig,
    pub catalog: CheckCatalog,
    pub extractors: ExtractorSet,
    pub candidates: CandidateCatalog,
    pub registry: Registry,
    pub form: Form,
    pub session: Session,
}

type DeltaSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub struct Kernel<D, R, A> {
    decisions: D,
    runner: R,
    activator: A,
    cfg: KernelConfig,
    catalog: CheckCatalog,
    extractors: ExtractorSet,
    candidates: CandidateCatalog,
    registry: Registry,
    history: History,
    session: Session,
    state: BudgetState,
    mutation_no: u32,
    decision_no: u32,
    /// Fragments de texte du modèle, pour l'affichage en direct. Jamais journalisés : le message
    /// complet l'est (`agent.message`).
    delta: Option<DeltaSink>,
    cancel: CancelToken,
}

enum Attempt {
    Done(Outcome),
    /// La mutation n'a pas abouti (refusée, ou candidate en échec puis rollback) : on peut réessayer.
    Failed,
}

fn human(why: HumanReason) -> Outcome {
    Outcome::HumanRequired { why }
}

fn ops_digest(ops: &[Op]) -> Digest {
    sha256_of(&ops).expect("ops are serializable")
}

impl<D: DecisionProvider, R: CheckRunner, A: Activator> Kernel<D, R, A> {
    /// Jeton pour interrompre le tour en cours depuis un autre thread.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Reçoit le texte du modèle au fil de l'eau (affichage seulement, non journalisé).
    pub fn set_delta_sink(&mut self, sink: impl Fn(&str) + Send + Sync + 'static) {
        self.delta = Some(std::sync::Arc::new(sink));
    }

    pub fn new(parts: KernelParts<D, R, A>) -> Result<Self, KernelError> {
        let KernelParts {
            decisions,
            runner,
            activator,
            config,
            catalog,
            extractors,
            candidates,
            registry,
            form,
            mut session,
        } = parts;
        let history = History::new(form.clone());
        let id = history.current_id().clone();
        session.record(
            None,
            EventKind::SessionStarted {
                agon: env!("CARGO_PKG_VERSION").into(),
            },
        )?;
        session.record(
            Some(id),
            EventKind::FormCommitted {
                snapshot: Box::new(form),
                parent: None,
            },
        )?;
        Ok(Kernel {
            decisions,
            runner,
            activator,
            cfg: config,
            catalog,
            extractors,
            candidates,
            registry,
            history,
            session,
            state: BudgetState::default(),
            mutation_no: 0,
            decision_no: 0,
            delta: None,
            cancel: CancelToken::default(),
        })
    }

    pub fn history(&self) -> &History {
        &self.history
    }
    pub fn session(&self) -> &Session {
        &self.session
    }
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub fn budget_state(&self) -> &BudgetState {
        &self.state
    }
    pub fn into_registry(self) -> Registry {
        self.registry
    }

    /// Tente de faire passer `check_id`, en adaptant la Form si nécessaire.
    pub async fn repair(&mut self, check_id: &CheckId) -> Result<Outcome, KernelError> {
        let check = self
            .catalog
            .get(check_id)
            .cloned()
            .ok_or_else(|| KernelError::UnknownCheck(check_id.clone()))?;
        let outcome = self.repair_check(&check).await?;
        self.emit(EventKind::TaskCompleted {
            outcome: outcome.clone(),
        })?;
        Ok(outcome)
    }

    async fn repair_check(&mut self, check: &Check) -> Result<Outcome, KernelError> {
        if let Err(e) = self.activator.activate(self.history.current()) {
            return Ok(human(HumanReason::ActivationFailed {
                detail: e.to_string(),
            }));
        }
        let form = self.history.current_id().clone();

        if self.verify_at(check, Phase::Initial, form.clone())?.passed {
            return Ok(Outcome::Passed);
        }
        // §16 : un échec doit être reproductible avant toute mutation. Le compteur de rechecks est propre
        // à chaque réparation : plusieurs checks vérifiés à la suite ne se comptent pas entre eux.
        self.state.same_form_retries = 1;
        let recheck = self.verify_at(check, Phase::Recheck, form)?;
        if reproduction(&recheck) == Reproduction::Flaky {
            return Ok(Outcome::Flaky);
        }

        let sig = self.extractors.normalize(&recheck.observation);
        self.emit(EventKind::SignatureDetected {
            id: sig.id(),
            signature: sig.clone(),
        })?;

        let mut tried: BTreeSet<Digest> = BTreeSet::new();
        loop {
            if let Some(ceiling) = self.state.exhausted(&self.cfg.budget) {
                return Ok(human(HumanReason::BudgetExhausted {
                    ceiling: ceiling.into(),
                }));
            }
            // Scénario 5 : signature connue → mutation validée, sans Jev.
            if let Some(m) = self.known_mutation(&sig, &tried) {
                tried.insert(ops_digest(&m.ops));
                if let Attempt::Done(o) = self.try_mutation(m, check, &sig, true)? {
                    return Ok(o);
                }
                continue;
            }
            // Signature inconnue du registre : candidats → Policy → Jev.
            if let Some(o) = self.consult_jev(check, &sig, &mut tried).await? {
                return Ok(o);
            }
        }
    }

    fn known_mutation(&mut self, sig: &Signature, tried: &BTreeSet<Digest>) -> Option<Mutation> {
        let id = self.next_mutation_id();
        let parent = self.history.current_id().clone();
        let m = self.registry.reuse(sig, parent, id)?;
        (!tried.contains(&ops_digest(&m.ops))).then_some(m)
    }

    /// Génère les candidats, les filtre par Policy (§7.3, amont), interroge Jev.
    /// `None` : la mutation choisie a échoué, la boucle peut continuer. `Some` : fin de tâche.
    async fn consult_jev(
        &mut self,
        check: &Check,
        sig: &Signature,
        tried: &mut BTreeSet<Digest>,
    ) -> Result<Option<Outcome>, KernelError> {
        let parent = self.history.current().clone();
        let mut cands: Vec<Mutation> = Vec::new();
        for ops in self.candidates.generate(&parent, check, sig) {
            let digest = ops_digest(&ops);
            if tried.contains(&digest) {
                continue;
            }
            let m = self.build_mutation(&parent, ops, sig, check, Origin::Catalog);
            self.emit(EventKind::MutationProposed {
                mutation: Box::new(m.clone()),
            })?;
            let denial: Option<Vec<String>> = match apply(&parent, &m, &self.catalog) {
                Ok(child) => match self.policy(&parent, &child, &m) {
                    Verdict::Allow => None,
                    Verdict::Deny(v) => Some(v.iter().map(|x| format!("{x:?}")).collect()),
                },
                Err(e) => Some(vec![e.to_string()]),
            };
            match denial {
                None => cands.push(m),
                Some(reasons) => {
                    tried.insert(digest);
                    self.emit(EventKind::MutationRejected {
                        mutation: m.id,
                        reasons,
                    })?;
                }
            }
        }
        if cands.is_empty() {
            return Ok(Some(human(HumanReason::NoCandidate)));
        }

        let options: Vec<Candidate> = cands.iter().map(Candidate::from_mutation).collect();
        let question = match q::mutation_question(&options) {
            Ok(question) => question,
            Err(e) => {
                return Ok(Some(human(HumanReason::JevError {
                    detail: e.to_string(),
                })));
            }
        };
        let questions = BTreeMap::from([(q::MUTATION_KEY.to_string(), question)]);
        let state = decision_state(check, sig, &parent);

        self.decision_no += 1;
        let decision_id = format!("d-{:04}", self.decision_no);
        let request = request_digest(&state, &questions);
        self.emit(EventKind::DecisionRequested {
            decision_id: decision_id.clone(),
            template: q::template_hash(&questions, &[q::MUTATION_KEY]),
            request: request.clone(),
            candidates: options.iter().map(|c| c.id.clone()).collect(),
        })?;

        let response = match self.decisions.decide(state, questions).await {
            Ok(r) => r,
            Err(e) => {
                let detail = e.to_string();
                self.emit(EventKind::DecisionUnavailable {
                    decision_id,
                    detail: detail.clone(),
                })?;
                return Ok(Some(match e {
                    JevError::Unavailable { .. } => human(HumanReason::JevUnavailable { detail }),
                    _ => human(HumanReason::JevError { detail }),
                }));
            }
        };

        let choice = response
            .answers
            .get(q::MUTATION_KEY)
            .ok_or_else(|| JevError::InvalidResponse("no answer for `mutation`".into()))
            .and_then(|a| q::decide_mutation(a, &self.cfg.thresholds));
        let requested = self.cfg.jev_model.clone();
        let answered = response.model.clone();
        self.emit(EventKind::DecisionCompleted {
            decision_id: decision_id.clone(),
            model_drift: !model_matches(&requested, &answered),
            jev_model_requested: requested,
            jev_model_answered: answered,
            exchange: Box::new(Exchange { request, response }),
            outcome: match &choice {
                Ok(MutationChoice::Apply {
                    candidate,
                    confidence,
                }) => format!("chose {candidate} (confidence {confidence:.2})"),
                Ok(MutationChoice::NoChange) => "no change is appropriate".into(),
                Ok(MutationChoice::Human) => "asked for a human decision".into(),
                Ok(MutationChoice::LowConfidence {
                    candidate,
                    confidence,
                    required,
                }) => {
                    format!("preferred {candidate} but confidence {confidence:.2} < {required:.2}")
                }
                Err(e) => format!("error: {e}"),
            },
        })?;

        Ok(match choice {
            Err(e) => Some(human(HumanReason::JevError {
                detail: e.to_string(),
            })),
            Ok(MutationChoice::NoChange) => Some(Outcome::NoChange),
            Ok(MutationChoice::Human) => Some(human(HumanReason::JevRequestedHuman)),
            Ok(MutationChoice::LowConfidence {
                candidate,
                confidence,
                required,
            }) => {
                self.emit(EventKind::DecisionEscalated {
                    decision_id,
                    detail: format!("`{candidate}` at confidence {confidence} < {required}"),
                })?;
                Some(human(HumanReason::LowConfidence {
                    candidate,
                    confidence,
                    required,
                }))
            }
            Ok(MutationChoice::Apply { candidate, .. }) => {
                match cands.into_iter().find(|m| m.id.0 == candidate) {
                    None => Some(human(HumanReason::JevError {
                        detail: format!("unknown candidate `{candidate}`"),
                    })),
                    Some(m) => {
                        tried.insert(ops_digest(&m.ops));
                        match self.try_mutation(m, check, sig, false)? {
                            Attempt::Done(o) => Some(o),
                            Attempt::Failed => None,
                        }
                    }
                }
            }
        })
    }

    /// Applique une mutation : Morph → Policy (aval) → activation → vérification → causalité.
    fn try_mutation(
        &mut self,
        m: Mutation,
        check: &Check,
        sig: &Signature,
        reused: bool,
    ) -> Result<Attempt, KernelError> {
        let parent = self.history.current().clone();
        let parent_id = parent.id();
        if reused {
            self.emit(EventKind::MutationProposed {
                mutation: Box::new(m.clone()),
            })?;
        }

        let child = match apply(&parent, &m, &self.catalog) {
            Ok(c) => c,
            Err(e) => {
                self.emit(EventKind::MutationRejected {
                    mutation: m.id,
                    reasons: vec![e.to_string()],
                })?;
                return Ok(Attempt::Failed);
            }
        };
        if let Verdict::Deny(v) = self.policy(&parent, &child, &m) {
            self.emit(EventKind::MutationRejected {
                mutation: m.id,
                reasons: v.iter().map(|x| format!("{x:?}")).collect(),
            })?;
            return Ok(Attempt::Failed);
        }
        self.emit(EventKind::MutationApproved {
            mutation: m.id.clone(),
        })?;

        self.state.mutations_applied += 1;
        self.state.same_form_retries = 0;
        let child_id = self.history.commit(child.clone());
        self.emit(EventKind::FormCommitted {
            snapshot: Box::new(child.clone()),
            parent: Some(parent_id.clone()),
        })?;
        self.emit(EventKind::MutationApplied {
            mutation: m.id.clone(),
            parent: parent_id.clone(),
            child: child_id.clone(),
        })?;

        if let Err(e) = self.activator.activate(&child) {
            self.rollback(&m.id, &parent_id)?;
            return Ok(Attempt::Done(human(HumanReason::ActivationFailed {
                detail: e.to_string(),
            })));
        }

        if !self
            .verify_at(check, Phase::Candidate, child_id.clone())?
            .passed
        {
            self.rollback(&m.id, &parent_id)?;
            self.state.consecutive_failures += 1;
            return Ok(Attempt::Failed);
        }
        self.state.consecutive_failures = 0;

        if reused {
            self.emit(EventKind::MutationValidated {
                mutation: m.id.clone(),
            })?;
            return Ok(Attempt::Done(Outcome::Fixed {
                mutation: m.id,
                proof: Proof::Reused,
                form: child_id,
            }));
        }

        // §16.1 : le parent échoue-t-il toujours ? Sinon la mutation n'est pas prouvée.
        self.activator.activate(&parent)?;
        let parent_recheck = self.verify_at(check, Phase::ParentRecheck, parent_id)?;
        self.activator.activate(&child)?;

        let proof = match causality(&parent_recheck) {
            Causality::Unproven => {
                self.emit(EventKind::MutationUnproven {
                    mutation: m.id.clone(),
                })?;
                Proof::Unproven
            }
            Causality::Proven => {
                self.emit(EventKind::MutationValidated {
                    mutation: m.id.clone(),
                })?;
                if self
                    .registry
                    .insert(sig, m.clone(), Status::Validated)
                    .is_ok()
                {
                    self.emit(EventKind::MemoryCreated {
                        signature: sig.id(),
                        mutation: m.id.clone(),
                    })?;
                }
                Proof::Validated
            }
        };
        Ok(Attempt::Done(Outcome::Fixed {
            mutation: m.id,
            proof,
            form: child_id,
        }))
    }

    fn rollback(&mut self, mutation: &MutationId, to: &FormId) -> Result<(), KernelError> {
        let from = self.history.current_id().clone();
        self.history.rollback(to)?;
        self.emit(EventKind::MutationRollback {
            mutation: mutation.clone(),
            from,
            to: to.clone(),
        })?;
        self.activator.activate(self.history.current())?;
        Ok(())
    }

    fn policy(&self, parent: &Form, candidate: &Form, mutation: &Mutation) -> Verdict {
        evaluate(&Evaluation {
            parent,
            candidate,
            mutation,
            catalog: &self.catalog,
            equivalences: &self.cfg.equivalences,
            budget: &self.cfg.budget,
            state: &self.state,
        })
    }

    fn build_mutation(
        &mut self,
        parent: &Form,
        ops: Vec<Op>,
        sig: &Signature,
        check: &Check,
        origin: Origin,
    ) -> Mutation {
        Mutation {
            id: self.next_mutation_id(),
            parent_form: parent.id(),
            scope: Scope::Task,
            origin,
            trigger: Trigger {
                check: sig.check.clone(),
                signature: sig.id(),
            },
            success_check: check.id.clone(),
            ops,
        }
    }

    fn next_mutation_id(&mut self) -> MutationId {
        self.mutation_no += 1;
        MutationId(format!("m-{:03}", self.mutation_no))
    }

    fn verify_at(
        &mut self,
        check: &Check,
        phase: Phase,
        form: FormId,
    ) -> Result<Evidence, KernelError> {
        self.emit_for(
            form.clone(),
            EventKind::VerificationStarted {
                check: check.id.clone(),
                phase,
            },
        )?;
        let evidence = run_check(&self.runner, check)?;
        let kind = if evidence.passed {
            EventKind::VerificationPassed {
                phase,
                evidence: evidence.clone(),
            }
        } else {
            EventKind::VerificationFailed {
                phase,
                evidence: evidence.clone(),
            }
        };
        self.emit_for(form, kind)?;
        Ok(evidence)
    }

    fn emit(&mut self, kind: EventKind) -> Result<(), KernelError> {
        let form = self.history.current_id().clone();
        self.emit_for(form, kind)
    }

    fn emit_for(&mut self, form: FormId, kind: EventKind) -> Result<(), KernelError> {
        self.session.record(Some(form), kind)?;
        Ok(())
    }
}
