//! Types de requête et de réponse de `POST /v1/systemone` (TypeSafe et OpenRouter).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Une question typée. `instructions` est une chaîne ou une structure JSON (doc TypeSafe).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    Noul {
        instructions: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        instructions: Value,
        /// option → description (`null` si l'option n'a pas besoin de détail).
        criteria: BTreeMap<String, Option<Value>>,
    },
    Score {
        instructions: Value,
        /// niveaux ordonnés, du plus bas au plus haut.
        criteria: Vec<Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<Value>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<Value>,
}

impl Question {
    pub fn noul(instructions: impl Into<String>) -> Self {
        Question::Noul {
            instructions: Value::String(instructions.into()),
            criteria: None,
        }
    }

    pub fn choice<I, K, V>(instructions: impl Into<String>, options: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Question::Choice {
            instructions: Value::String(instructions.into()),
            criteria: options
                .into_iter()
                .map(|(k, v)| (k.into(), Some(Value::String(v.into()))))
                .collect(),
        }
    }

    pub fn score<I, V>(instructions: impl Into<String>, levels: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        Question::Score {
            instructions: Value::String(instructions.into()),
            criteria: levels
                .into_iter()
                .map(|l| Value::String(l.into()))
                .collect(),
        }
    }

    pub fn instructions(&self) -> &Value {
        match self {
            Question::Noul { instructions, .. }
            | Question::Choice { instructions, .. }
            | Question::Score { instructions, .. } => instructions,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
        #[serde(default)]
        legend: BTreeMap<String, Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Présent via OpenRouter uniquement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Version exacte du modèle ayant répondu (à journaliser, §34).
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openrouter_example_response_deserializes() {
        let r: Response = serde_json::from_value(json!({
            "id": "gen-dec-1", "model": "typesafe/jev-1.13-20260917", "provider": "TypeSafe",
            "answers": {
                "is_bug": {"type": "noul", "noul": 0.96},
                "team": {"type": "choice", "choice": "payments", "confidence": 0.75,
                         "probabilities": {"account": 0, "frontend": 0.16, "payments": 0.84}},
                "urgency": {"type": "score", "score": 1.99, "confidence": 0.99,
                            "legend": {"0": "a", "1": "b", "2": "c"},
                            "probabilities": {"0": 0, "1": 0.01, "2": 0.99}}
            },
            "usage": {"input_tokens": 476, "output_tokens": 70, "cost": 0.00002}
        }))
        .unwrap();
        assert_eq!(r.answers.len(), 3);
        assert_eq!(r.usage.cost, Some(0.00002));
    }

    #[test]
    fn typesafe_response_without_openrouter_fields_deserializes() {
        let r: Response = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {"x": {"type": "noul", "noul": 0.5}},
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }))
        .unwrap();
        assert!(r.id.is_none() && r.usage.cost.is_none());
    }

    #[test]
    fn request_serializes_to_the_documented_shape() {
        let mut q = BTreeMap::new();
        q.insert(
            "t".to_string(),
            Question::choice("Which?", [("a", "A"), ("b", "B")]),
        );
        q.insert("u".to_string(), Question::noul("Yes?"));
        q.insert(
            "s".to_string(),
            Question::score("How much?", ["low", "high"]),
        );
        let v = serde_json::to_value(Request {
            model: "jev-1.13".into(),
            state: json!("hi"),
            questions: q,
        })
        .unwrap();
        assert_eq!(v["questions"]["t"]["type"], "choice");
        assert_eq!(v["questions"]["t"]["criteria"]["a"], "A");
        assert_eq!(
            v["questions"]["u"],
            json!({"type": "noul", "instructions": "Yes?"})
        );
        assert_eq!(v["questions"]["s"]["criteria"], json!(["low", "high"]));
    }

    #[test]
    fn noul_criteria_use_true_false_keys() {
        let q = Question::Noul {
            instructions: json!("ok?"),
            criteria: Some(NoulCriteria {
                yes: Some(json!("y")),
                no: Some(json!("n")),
            }),
        };
        assert_eq!(
            serde_json::to_value(q).unwrap()["criteria"],
            json!({"true": "y", "false": "n"})
        );
    }
}
