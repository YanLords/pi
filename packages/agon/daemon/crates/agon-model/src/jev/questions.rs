//! Construction des questions posées à Jev (§6.3) et lecture des réponses en décisions typées.
//! Jev choisit dans des ensembles fermés construits par Agon ; il ne propose jamais d'option.

use super::JevError;
use super::gating::{Gate, Thresholds, gate_choice};
use super::types::{Answer, Question};
use super::validate::MAX_CHOICE_OPTIONS;
use agon_core::mutation::{OpKind, Target};
use agon_core::{Digest, Mutation, sha256_of};
use std::collections::BTreeMap;

pub const NONE: &str = "none";
pub const HUMAN: &str = "human";
pub const MUTATION_KEY: &str = "mutation";
pub const MODEL_TIER_KEY: &str = "model_tier";

/// `none` et `human` occupent deux des 255 options d'un Choice.
pub const MAX_CANDIDATES: usize = MAX_CHOICE_OPTIONS - 2;

/// Mutation candidate présentée à Jev (déjà filtrée par Policy en amont, §7.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Identifiant de la mutation, utilisé comme clé d'option.
    pub id: String,
    pub description: String,
}

impl Candidate {
    pub fn from_mutation(m: &Mutation) -> Self {
        Candidate {
            id: m.id.0.clone(),
            description: describe(m),
        }
    }
}

/// Description déterministe d'un diff, ex. `add capabilities: docker, postgres`.
pub fn describe(m: &Mutation) -> String {
    let verb = |k: OpKind| match k {
        OpKind::Add => "enable",
        OpKind::Remove => "disable",
        OpKind::Set => "set",
    };
    let target = |t: Target| match t {
        Target::Capabilities => "capabilities",
        Target::Tools => "tools",
        Target::Modules => "modules",
        Target::Context => "context",
        Target::Checks => "checks",
        Target::Model => "model",
    };
    m.ops
        .iter()
        .map(|o| {
            format!(
                "{} {}: {}",
                verb(o.op),
                target(o.target),
                o.value.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Question « quelle mutation ? » : candidats + `none` + `human` (§6.3).
/// Sans candidat, Agon ne doit pas appeler Jev mais passer en `HUMAN_REQUIRED` (§9.2).
pub fn mutation_question(candidates: &[Candidate]) -> Result<Question, JevError> {
    if candidates.is_empty() {
        return Err(JevError::InvalidRequest(
            "no candidate: go to HUMAN_REQUIRED without calling Jev".into(),
        ));
    }
    if candidates.len() > MAX_CANDIDATES {
        return Err(JevError::InvalidRequest(format!(
            "{} candidates, at most {MAX_CANDIDATES}",
            candidates.len()
        )));
    }
    let mut options = BTreeMap::new();
    for c in candidates {
        if c.id == NONE || c.id == HUMAN {
            return Err(JevError::InvalidRequest(format!(
                "candidate id `{}` is reserved",
                c.id
            )));
        }
        if options
            .insert(c.id.clone(), c.description.clone())
            .is_some()
        {
            return Err(JevError::InvalidRequest(format!(
                "duplicate candidate id `{}`",
                c.id
            )));
        }
    }
    options.insert(NONE.into(), "No configuration change is appropriate".into());
    options.insert(
        HUMAN.into(),
        "The situation requires a human decision".into(),
    );
    Ok(Question::choice(
        "Which candidate change to the runtime configuration best addresses `signature`?",
        options,
    ))
}

#[derive(Clone, Debug, PartialEq)]
pub enum MutationChoice {
    Apply {
        candidate: String,
        confidence: f64,
    },
    /// Jev estime qu'aucun changement n'est approprié.
    NoChange,
    /// Jev demande une décision humaine → `HUMAN_REQUIRED`.
    Human,
    /// Candidat choisi mais confiance insuffisante → `HUMAN_REQUIRED` par défaut (§6.4).
    LowConfidence {
        candidate: String,
        confidence: f64,
        required: f64,
    },
}

pub fn decide_mutation(
    answer: &Answer,
    thresholds: &Thresholds,
) -> Result<MutationChoice, JevError> {
    let Answer::Choice { confidence, .. } = answer else {
        return Err(JevError::InvalidResponse("expected a choice answer".into()));
    };
    Ok(
        match gate_choice(answer, thresholds.mutation_min_confidence)? {
            Gate::Pass(c) | Gate::Escalate { value: c, .. } if c == NONE => {
                MutationChoice::NoChange
            }
            Gate::Pass(c) | Gate::Escalate { value: c, .. } if c == HUMAN => MutationChoice::Human,
            Gate::Pass(candidate) => MutationChoice::Apply {
                candidate,
                confidence: *confidence,
            },
            Gate::Escalate {
                value,
                confidence,
                required,
            } => MutationChoice::LowConfidence {
                candidate: value,
                confidence,
                required,
            },
        },
    )
}

/// Niveau de modèle génératif : Choice `small` / `medium` / `large` (§6.3, §31).
pub fn model_tier_question() -> Question {
    Question::choice(
        "Which model tier is appropriate for `task`?",
        [
            (
                "small",
                "Mechanical change, single file, no design decision",
            ),
            (
                "medium",
                "Multi-file change or debugging with clear evidence",
            ),
            (
                "large",
                "Architecture, ambiguous failure, or cross-cutting change",
            ),
        ],
    )
}

/// Un Noul par outil candidat (§6.3), clé `tool.<id>`.
pub fn tool_question(tool_id: &str, description: &str) -> (String, Question) {
    (
        format!("tool.{tool_id}"),
        Question::noul(format!(
            "Is using {tool_id} ({description}) needed to make progress on `task`?"
        )),
    )
}

/// Empreinte des gabarits de questions, pour le lockfile (§11) et les événements (§34).
/// Les clés `dynamic` (dont les options dépendent des candidats) ne comptent que par leurs instructions.
pub fn template_hash(questions: &BTreeMap<String, Question>, dynamic: &[&str]) -> Digest {
    let view: BTreeMap<&String, serde_json::Value> = questions
        .iter()
        .map(|(k, q)| {
            let mut v = serde_json::to_value(q).expect("question is serializable");
            if dynamic.contains(&k.as_str()) {
                v["criteria"] = serde_json::Value::Null;
            }
            (k, v)
        })
        .collect();
    sha256_of(&view).expect("template is serializable")
}

/// Empreinte du gabarit de la question de mutation, indépendante des candidats : c'est la valeur
/// consignée dans le lockfile (§11) et recalculée à chaque décision (§34).
pub fn mutation_template_hash() -> Digest {
    let placeholder = |id: &str| Candidate {
        id: id.into(),
        description: String::new(),
    };
    let question = mutation_question(&[placeholder("a"), placeholder("b")])
        .expect("placeholder candidates are valid");
    template_hash(
        &BTreeMap::from([(MUTATION_KEY.to_string(), question)]),
        &[MUTATION_KEY],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agon_core::mutation::{Op, Origin, Scope, Trigger};
    use agon_core::{MutationId, Signature};

    fn cand(id: &str) -> Candidate {
        Candidate {
            id: id.into(),
            description: format!("desc {id}"),
        }
    }

    fn pick(c: &str, confidence: f64) -> Answer {
        Answer::Choice {
            choice: c.into(),
            probabilities: BTreeMap::from([(c.to_string(), 1.0)]),
            confidence,
        }
    }

    #[test]
    fn mutation_question_lists_candidates_plus_none_and_human() {
        let Question::Choice { criteria, .. } =
            mutation_question(&[cand("m1"), cand("m2")]).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            criteria.keys().map(String::as_str).collect::<Vec<_>>(),
            ["human", "m1", "m2", "none"]
        );
    }

    #[test]
    fn mutation_question_rejects_bad_candidate_sets() {
        assert!(
            mutation_question(&[]).is_err(),
            "no candidate: Jev must not be called"
        );
        assert!(mutation_question(&[cand("none")]).is_err());
        assert!(mutation_question(&[cand("human")]).is_err());
        assert!(mutation_question(&[cand("m1"), cand("m1")]).is_err());
        let many: Vec<_> = (0..=MAX_CANDIDATES)
            .map(|i| cand(&format!("m{i}")))
            .collect();
        assert!(mutation_question(&many).is_err());
        let max: Vec<_> = (0..MAX_CANDIDATES)
            .map(|i| cand(&format!("m{i}")))
            .collect();
        assert!(mutation_question(&max).is_ok());
    }

    #[test]
    fn decide_mutation_covers_every_outcome() {
        let t = Thresholds::default();
        assert_eq!(
            decide_mutation(&pick("m1", 0.9), &t).unwrap(),
            MutationChoice::Apply {
                candidate: "m1".into(),
                confidence: 0.9
            }
        );
        assert_eq!(
            decide_mutation(&pick("m1", 0.5), &t).unwrap(),
            MutationChoice::LowConfidence {
                candidate: "m1".into(),
                confidence: 0.5,
                required: 0.75
            }
        );
        assert_eq!(
            decide_mutation(&pick(NONE, 0.9), &t).unwrap(),
            MutationChoice::NoChange
        );
        assert_eq!(
            decide_mutation(&pick(HUMAN, 0.9), &t).unwrap(),
            MutationChoice::Human
        );
        // `none` / `human` restent des non-actions même à faible confiance : rien n'est appliqué.
        assert_eq!(
            decide_mutation(&pick(NONE, 0.1), &t).unwrap(),
            MutationChoice::NoChange
        );
        assert_eq!(
            decide_mutation(&pick(HUMAN, 0.1), &t).unwrap(),
            MutationChoice::Human
        );
        assert!(decide_mutation(&Answer::Noul { noul: 1.0 }, &t).is_err());
    }

    #[test]
    fn candidate_description_is_derived_from_the_diff() {
        let m = Mutation {
            id: MutationId("m-001".into()),
            parent_form: agon_core::fixtures::base_form().id(),
            scope: Scope::Task,
            origin: Origin::Catalog,
            trigger: Trigger {
                check: "c".into(),
                signature: Signature::unknown("c".into()).id(),
            },
            success_check: "c".into(),
            ops: vec![
                Op {
                    op: OpKind::Add,
                    target: Target::Capabilities,
                    value: vec!["docker".into(), "postgres".into()],
                },
                Op {
                    op: OpKind::Set,
                    target: Target::Model,
                    value: vec!["large".into()],
                },
            ],
        };
        let c = Candidate::from_mutation(&m);
        assert_eq!(c.id, "m-001");
        assert_eq!(
            c.description,
            "enable capabilities: docker, postgres; set model: large"
        );
    }

    #[test]
    fn template_hash_ignores_dynamic_options_but_not_instructions() {
        let with = |cands: &[Candidate]| {
            BTreeMap::from([
                (MUTATION_KEY.to_string(), mutation_question(cands).unwrap()),
                (MODEL_TIER_KEY.to_string(), model_tier_question()),
            ])
        };
        let a = template_hash(&with(&[cand("m1")]), &[MUTATION_KEY]);
        let b = template_hash(&with(&[cand("m7"), cand("m8")]), &[MUTATION_KEY]);
        assert_eq!(a, b, "different candidates, same template");
        assert_ne!(
            template_hash(&with(&[cand("m1")]), &[]),
            template_hash(&with(&[cand("m7")]), &[])
        );

        let mut edited = with(&[cand("m1")]);
        edited.insert(
            MODEL_TIER_KEY.into(),
            Question::choice("Other wording?", [("small", "s"), ("large", "l")]),
        );
        assert_ne!(a, template_hash(&edited, &[MUTATION_KEY]));
    }

    #[test]
    fn lockfile_template_hash_equals_the_per_decision_hash() {
        let live = BTreeMap::from([(
            MUTATION_KEY.to_string(),
            mutation_question(&[cand("m-007"), cand("m-009")]).unwrap(),
        )]);
        assert_eq!(
            mutation_template_hash(),
            template_hash(&live, &[MUTATION_KEY])
        );
    }

    #[test]
    fn tool_questions_are_keyed_by_tool() {
        let (k, q) = tool_question("cargo.test", "run the test suite");
        assert_eq!(k, "tool.cargo.test");
        assert!(matches!(q, Question::Noul { .. }));
    }
}
