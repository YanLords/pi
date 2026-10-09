//! Policy : autorité de sécurité et de cohérence (§7). Règles déterministes, aucun LLM, aucune E/S.
//!
//! Policy juge une transition `parent Form → candidate Form` produite par Morph. Elle n'applique
//! rien : le Kernel appelle `evaluate` en amont (filtrage des candidats) et en aval (décision
//! composée), voir §7.3.

mod budget;
mod permissions;
mod rules;

pub use budget::{Budget, BudgetState};
pub use permissions::{Action, Level, Permission, Permissions};
pub use rules::{Equivalence, Evaluation, Verdict, Violation, evaluate};
