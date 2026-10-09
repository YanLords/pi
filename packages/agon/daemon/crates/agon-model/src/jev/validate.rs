//! Validation stricte : une réponse incohérente avec la requête n'est jamais utilisée.

use super::JevError;
use super::types::{Answer, Question, Request, Response};

/// Tolérance sur la somme des probabilités (l'API arrondit à 2 décimales).
const SUM_TOLERANCE: f64 = 0.02;
pub const MAX_CHOICE_OPTIONS: usize = 255;
pub const MAX_SCORE_LEVELS: usize = 10;

fn bad(msg: impl Into<String>) -> JevError {
    JevError::InvalidRequest(msg.into())
}

/// Vérifie la requête avant envoi (limites documentées de l'API).
pub fn request(req: &Request) -> Result<(), JevError> {
    if req.questions.is_empty() {
        return Err(bad("at least one question is required"));
    }
    for (id, q) in &req.questions {
        if is_blank(q.instructions()) {
            return Err(bad(format!("question `{id}`: empty instructions")));
        }
        match q {
            Question::Choice { criteria, .. }
                if criteria.len() < 2 || criteria.len() > MAX_CHOICE_OPTIONS =>
            {
                return Err(bad(format!(
                    "question `{id}`: a choice needs 2..={MAX_CHOICE_OPTIONS} options, got {}",
                    criteria.len()
                )));
            }
            Question::Score { criteria, .. }
                if criteria.len() < 2 || criteria.len() > MAX_SCORE_LEVELS =>
            {
                return Err(bad(format!(
                    "question `{id}`: a score needs 2..={MAX_SCORE_LEVELS} levels, got {}",
                    criteria.len()
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_blank(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

fn invalid(id: &str, msg: impl Into<String>) -> JevError {
    JevError::InvalidResponse(format!("answer `{id}`: {}", msg.into()))
}

fn unit(id: &str, what: &str, x: f64) -> Result<(), JevError> {
    if x.is_finite() && (0.0..=1.0).contains(&x) {
        Ok(())
    } else {
        Err(invalid(id, format!("{what} {x} is outside [0, 1]")))
    }
}

fn distribution<'a>(
    id: &str,
    probs: &std::collections::BTreeMap<String, f64>,
    allowed: impl Fn(&str) -> bool + 'a,
) -> Result<(), JevError> {
    let mut sum = 0.0;
    for (k, p) in probs {
        if !allowed(k) {
            return Err(invalid(id, format!("probability for unknown option `{k}`")));
        }
        unit(id, "probability", *p)?;
        sum += p;
    }
    if (sum - 1.0).abs() > SUM_TOLERANCE {
        return Err(invalid(
            id,
            format!("probabilities sum to {sum}, expected 1"),
        ));
    }
    Ok(())
}

/// Vérifie que la réponse répond bien à la requête : une réponse par question, du bon type,
/// avec des valeurs dans leur domaine.
pub fn response(req: &Request, resp: &Response) -> Result<(), JevError> {
    for (id, q) in &req.questions {
        let a = resp
            .answers
            .get(id)
            .ok_or_else(|| JevError::InvalidResponse(format!("no answer for question `{id}`")))?;
        match (q, a) {
            (Question::Noul { .. }, Answer::Noul { noul }) => unit(id, "noul", *noul)?,
            (
                Question::Choice { criteria, .. },
                Answer::Choice {
                    choice,
                    probabilities,
                    confidence,
                },
            ) => {
                if !criteria.contains_key(choice) {
                    return Err(invalid(id, format!("choice `{choice}` is not an option")));
                }
                unit(id, "confidence", *confidence)?;
                distribution(id, probabilities, |k| criteria.contains_key(k))?;
            }
            (
                Question::Score { criteria, .. },
                Answer::Score {
                    score,
                    probabilities,
                    confidence,
                    ..
                },
            ) => {
                let max = (criteria.len() - 1) as f64;
                if !score.is_finite() || *score < 0.0 || *score > max {
                    return Err(invalid(id, format!("score {score} is outside [0, {max}]")));
                }
                unit(id, "confidence", *confidence)?;
                let n = criteria.len();
                distribution(id, probabilities, |k| {
                    k.parse::<usize>().is_ok_and(|i| i < n)
                })?;
            }
            _ => return Err(invalid(id, "answer type does not match the question type")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::types::Usage;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn req() -> Request {
        Request {
            model: "m".into(),
            state: json!("s"),
            questions: BTreeMap::from([
                ("n".to_string(), Question::noul("yes?")),
                (
                    "c".to_string(),
                    Question::choice("which?", [("a", "A"), ("b", "B")]),
                ),
                (
                    "s".to_string(),
                    Question::score("how?", ["lo", "mid", "hi"]),
                ),
            ]),
        }
    }

    fn good() -> Response {
        Response {
            id: None,
            model: "m".into(),
            provider: None,
            answers: BTreeMap::from([
                ("n".to_string(), Answer::Noul { noul: 0.9 }),
                (
                    "c".to_string(),
                    Answer::Choice {
                        choice: "a".into(),
                        probabilities: BTreeMap::from([("a".into(), 0.8), ("b".into(), 0.2)]),
                        confidence: 0.7,
                    },
                ),
                (
                    "s".to_string(),
                    Answer::Score {
                        score: 1.5,
                        probabilities: BTreeMap::from([
                            ("0".into(), 0.0),
                            ("1".into(), 0.5),
                            ("2".into(), 0.5),
                        ]),
                        confidence: 0.6,
                        legend: BTreeMap::new(),
                    },
                ),
            ]),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cost: None,
            },
        }
    }

    #[test]
    fn a_consistent_response_is_accepted() {
        assert!(request(&req()).is_ok());
        assert!(response(&req(), &good()).is_ok());
    }

    #[test]
    fn request_limits_are_enforced() {
        let mut r = req();
        r.questions
            .insert("one".into(), Question::choice("x?", [("only", "o")]));
        assert!(matches!(request(&r), Err(JevError::InvalidRequest(_))));

        let mut r = req();
        r.questions.insert(
            "big".into(),
            Question::score("x?", (0..11).map(|i| i.to_string())),
        );
        assert!(matches!(request(&r), Err(JevError::InvalidRequest(_))));

        let mut r = req();
        r.questions.insert("blank".into(), Question::noul("   "));
        assert!(request(&r).is_err());

        r.questions.clear();
        assert!(request(&r).is_err());
    }

    #[test]
    fn missing_answer_is_rejected() {
        let mut r = good();
        r.answers.remove("c");
        assert!(
            matches!(response(&req(), &r), Err(JevError::InvalidResponse(m)) if m.contains("no answer"))
        );
    }

    #[test]
    fn wrong_answer_type_is_rejected() {
        let mut r = good();
        r.answers.insert("n".into(), Answer::Noul { noul: 0.1 });
        r.answers.insert("c".into(), Answer::Noul { noul: 0.1 });
        assert!(response(&req(), &r).is_err());
    }

    #[test]
    fn out_of_domain_values_are_rejected() {
        let cases: Vec<(&str, Answer)> = vec![
            ("n", Answer::Noul { noul: 1.2 }),
            ("n", Answer::Noul { noul: f64::NAN }),
            (
                "c",
                Answer::Choice {
                    choice: "zzz".into(),
                    probabilities: BTreeMap::from([("a".into(), 1.0)]),
                    confidence: 0.5,
                },
            ),
            (
                "c",
                Answer::Choice {
                    choice: "a".into(),
                    probabilities: BTreeMap::from([("a".into(), 0.5), ("b".into(), 0.1)]),
                    confidence: 0.5,
                },
            ),
            (
                "c",
                Answer::Choice {
                    choice: "a".into(),
                    probabilities: BTreeMap::from([("a".into(), 0.5), ("x".into(), 0.5)]),
                    confidence: 0.5,
                },
            ),
            (
                "c",
                Answer::Choice {
                    choice: "a".into(),
                    probabilities: BTreeMap::from([("a".into(), 1.0)]),
                    confidence: 1.5,
                },
            ),
            (
                "s",
                Answer::Score {
                    score: 2.5,
                    probabilities: BTreeMap::from([("2".into(), 1.0)]),
                    confidence: 0.5,
                    legend: BTreeMap::new(),
                },
            ),
            (
                "s",
                Answer::Score {
                    score: 1.0,
                    probabilities: BTreeMap::from([("7".into(), 1.0)]),
                    confidence: 0.5,
                    legend: BTreeMap::new(),
                },
            ),
        ];
        for (id, answer) in cases {
            let mut r = good();
            r.answers.insert(id.into(), answer.clone());
            assert!(response(&req(), &r).is_err(), "{answer:?} must be rejected");
        }
    }

    #[test]
    fn rounded_probabilities_within_tolerance_are_accepted() {
        let mut r = good();
        r.answers.insert(
            "c".into(),
            Answer::Choice {
                choice: "a".into(),
                probabilities: BTreeMap::from([("a".into(), 0.84), ("b".into(), 0.15)]),
                confidence: 0.7,
            },
        );
        assert!(response(&req(), &r).is_ok());
    }
}
