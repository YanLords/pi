use crate::{Observation, VerifyError};
use agon_core::Check;
use std::str::FromStr;

/// Condition de succès d'un check. V0 : comparaison du code de sortie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Condition {
    code: i32,
    negate: bool,
}

impl FromStr for Condition {
    type Err = VerifyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || VerifyError::UnsupportedCondition(s.to_string());
        let parts: Vec<&str> = s.split_whitespace().collect();
        let [lhs, op, rhs] = parts.as_slice() else {
            return Err(bad());
        };
        if *lhs != "exit_code" {
            return Err(bad());
        }
        let negate = match *op {
            "==" => false,
            "!=" => true,
            _ => return Err(bad()),
        };
        Ok(Condition {
            code: rhs.parse().map_err(|_| bad())?,
            negate,
        })
    }
}

impl Condition {
    pub fn holds(&self, obs: &Observation) -> bool {
        match obs.exit_code {
            Some(c) if !obs.timed_out => (c == self.code) != self.negate,
            _ => false, // tué ou timeout : jamais un succès
        }
    }
}

/// `true` si l'observation satisfait la condition de succès du check.
pub fn evaluate(check: &Check, obs: &Observation) -> Result<bool, VerifyError> {
    Ok(check.success.parse::<Condition>()?.holds(obs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agon_core::fixtures::build_check;

    pub(crate) fn obs(code: Option<i32>, timed_out: bool) -> Observation {
        Observation {
            check: "verify.build".into(),
            exit_code: code,
            stdout: String::new(),
            stderr: String::new(),
            timed_out,
        }
    }

    #[test]
    fn exit_code_equality_and_inequality() {
        let mut c = build_check();
        assert!(evaluate(&c, &obs(Some(0), false)).unwrap());
        assert!(!evaluate(&c, &obs(Some(1), false)).unwrap());
        c.success = "exit_code != 0".into();
        assert!(evaluate(&c, &obs(Some(2), false)).unwrap());
    }

    #[test]
    fn killed_or_timed_out_is_never_success() {
        let mut c = build_check();
        assert!(!evaluate(&c, &obs(None, false)).unwrap());
        assert!(!evaluate(&c, &obs(Some(0), true)).unwrap());
        c.success = "exit_code != 0".into();
        assert!(
            !evaluate(&c, &obs(None, false)).unwrap(),
            "a signal is not a `!= 0` success either"
        );
    }

    #[test]
    fn unsupported_conditions_are_errors() {
        for s in [
            "",
            "exit_code",
            "exit_code == x",
            "stdout == 0",
            "exit_code >= 0",
            "exit_code == 0 && ok",
        ] {
            let mut c = build_check();
            c.success = s.into();
            assert!(
                matches!(
                    evaluate(&c, &obs(Some(0), false)),
                    Err(VerifyError::UnsupportedCondition(_))
                ),
                "{s:?}"
            );
        }
    }
}
