use agon_core::{Check, CheckId};
use serde::{Deserialize, Serialize};

/// Résultat brut d'un check (§18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub check: CheckId,
    /// `None` : processus tué (timeout ou signal).
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Exécute un check dans l'environnement courant. Implémenté par le Kernel (processus réels)
/// ou par des doubles de test.
pub trait CheckRunner {
    fn run(&self, check: &Check) -> Observation;
}
