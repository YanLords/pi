//! Confidence gating (§6.4) : la réponse dit *quoi*, la confiance dit *si l'on peut agir*.
//! Fonctions pures, seuils configurables.

use super::JevError;
use super::types::Answer;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// Un Noul vaut « oui » à partir de ce seuil (inclus).
    pub noul_yes: f64,
    pub choice_min_confidence: f64,
    pub mutation_min_confidence: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            noul_yes: 0.5,
            choice_min_confidence: 0.6,
            mutation_min_confidence: 0.75,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Gate<T> {
    Pass(T),
    /// Confiance sous le seuil : escalade (LLM génératif si autorisé, sinon `HUMAN_REQUIRED`).
    Escalate {
        value: T,
        confidence: f64,
        required: f64,
    },
}

fn wrong_kind(expected: &str) -> JevError {
    JevError::InvalidResponse(format!("expected a {expected} answer"))
}

pub fn noul_is_yes(answer: &Answer, threshold: f64) -> Result<bool, JevError> {
    match answer {
        Answer::Noul { noul } => Ok(*noul >= threshold),
        _ => Err(wrong_kind("noul")),
    }
}

/// Applique le seuil de confiance à un Choice.
pub fn gate_choice(answer: &Answer, required: f64) -> Result<Gate<String>, JevError> {
    match answer {
        Answer::Choice {
            choice, confidence, ..
        } if *confidence >= required => Ok(Gate::Pass(choice.clone())),
        Answer::Choice {
            choice, confidence, ..
        } => Ok(Gate::Escalate {
            value: choice.clone(),
            confidence: *confidence,
            required,
        }),
        _ => Err(wrong_kind("choice")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    pub(crate) fn choice(c: &str, confidence: f64) -> Answer {
        Answer::Choice {
            choice: c.into(),
            probabilities: BTreeMap::from([(c.to_string(), 1.0)]),
            confidence,
        }
    }

    #[test]
    fn noul_threshold_is_inclusive() {
        assert!(noul_is_yes(&Answer::Noul { noul: 0.5 }, 0.5).unwrap());
        assert!(!noul_is_yes(&Answer::Noul { noul: 0.49 }, 0.5).unwrap());
    }

    #[test]
    fn choice_passes_at_or_above_the_required_confidence() {
        assert_eq!(
            gate_choice(&choice("a", 0.75), 0.75).unwrap(),
            Gate::Pass("a".into())
        );
        assert_eq!(
            gate_choice(&choice("a", 0.9), 0.75).unwrap(),
            Gate::Pass("a".into())
        );
    }

    #[test]
    fn choice_below_the_threshold_escalates_with_details() {
        assert_eq!(
            gate_choice(&choice("a", 0.7), 0.75).unwrap(),
            Gate::Escalate {
                value: "a".into(),
                confidence: 0.7,
                required: 0.75
            }
        );
    }

    #[test]
    fn wrong_answer_kinds_are_errors() {
        assert!(gate_choice(&Answer::Noul { noul: 1.0 }, 0.5).is_err());
        assert!(noul_is_yes(&choice("a", 1.0), 0.5).is_err());
    }
}
