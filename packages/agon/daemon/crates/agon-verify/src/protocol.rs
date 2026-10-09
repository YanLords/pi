//! Protocole de vérification : preuves (§2.5), reproductibilité d'un échec (§16) et causalité
//! d'une mutation (§16.1). Fonctions pures : le Kernel décide *quand* et *dans quelle Form*
//! exécuter chaque check.

use crate::{CheckRunner, Observation, VerifyError, condition};
use agon_core::{Check, CheckId, Digest};
use serde::{Deserialize, Serialize};

/// Preuve objective : ACTION → VERIFICATION → EVIDENCE → SUCCESS / FAILURE.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub check: CheckId,
    /// Hash de la définition du check qui a produit la preuve.
    pub definition: Digest,
    pub passed: bool,
    pub observation: Observation,
}

pub fn run_check(runner: &dyn CheckRunner, check: &Check) -> Result<Evidence, VerifyError> {
    let observation = runner.run(check);
    let passed = condition::evaluate(check, &observation)?;
    Ok(Evidence {
        check: check.id.clone(),
        definition: check.definition_hash(),
        passed,
        observation,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reproduction {
    /// FAIL puis PASS sur la même Form : flaky / transitoire, ne justifie aucune mutation.
    Flaky,
    /// FAIL puis FAIL : l'échec est confirmé, une mutation peut être proposée.
    Reproducible,
}

/// Classe un échec à partir du recheck sur la même Form (§16).
pub fn reproduction(recheck: &Evidence) -> Reproduction {
    if recheck.passed {
        Reproduction::Flaky
    } else {
        Reproduction::Reproducible
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Causality {
    /// Le parent échoue toujours : c'est bien la mutation qui a corrigé.
    Proven,
    /// Le parent passe aussi : l'échec a disparu sans la mutation.
    Unproven,
}

/// Verdict de causalité après succès de la candidate, à partir du recheck du parent (§16.1).
pub fn causality(parent_recheck: &Evidence) -> Causality {
    if parent_recheck.passed {
        Causality::Unproven
    } else {
        Causality::Proven
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agon_core::fixtures::build_check;
    use std::cell::RefCell;

    /// Double de test : rejoue des codes de sortie prédéfinis.
    pub struct Scripted(pub RefCell<Vec<i32>>);

    impl CheckRunner for Scripted {
        fn run(&self, check: &Check) -> Observation {
            let code = self.0.borrow_mut().remove(0);
            Observation {
                check: check.id.clone(),
                exit_code: Some(code),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
            }
        }
    }

    fn evidence(codes: &[i32]) -> Vec<Evidence> {
        let runner = Scripted(RefCell::new(codes.to_vec()));
        codes
            .iter()
            .map(|_| run_check(&runner, &build_check()).unwrap())
            .collect()
    }

    #[test]
    fn evidence_records_check_definition_and_verdict() {
        let ev = &evidence(&[1])[0];
        assert!(!ev.passed);
        assert_eq!(ev.definition, build_check().definition_hash());
    }

    #[test]
    fn fail_then_pass_is_flaky() {
        let ev = evidence(&[1, 0]);
        assert_eq!(reproduction(&ev[1]), Reproduction::Flaky);
    }

    #[test]
    fn fail_then_fail_is_reproducible() {
        let ev = evidence(&[1, 1]);
        assert_eq!(reproduction(&ev[1]), Reproduction::Reproducible);
    }

    #[test]
    fn scenario_4_parent_passing_again_means_unproven() {
        // parent FAIL, FAIL (reproductible) ; candidate PASS ; parent recheck PASS
        let ev = evidence(&[1, 1, 0, 0]);
        assert_eq!(reproduction(&ev[1]), Reproduction::Reproducible);
        assert!(ev[2].passed);
        assert_eq!(causality(&ev[3]), Causality::Unproven);
    }

    #[test]
    fn scenario_2_parent_still_failing_means_proven() {
        let ev = evidence(&[1, 1, 0, 1]);
        assert_eq!(causality(&ev[3]), Causality::Proven);
    }

    #[test]
    fn unsupported_condition_surfaces_as_error() {
        let mut c = build_check();
        c.success = "stdout contains ok".into();
        let runner = Scripted(RefCell::new(vec![0]));
        assert!(run_check(&runner, &c).is_err());
    }
}
