use serde::{Deserialize, Serialize};

/// Budget de mutations d'une tâche (§23).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    pub max_mutations_per_task: u32,
    pub max_consecutive_failures: u32,
    pub max_same_form_retries: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_mutations_per_task: 3,
            max_consecutive_failures: 3,
            max_same_form_retries: 1,
        }
    }
}

/// Consommation courante, tenue par le Kernel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetState {
    pub mutations_applied: u32,
    pub consecutive_failures: u32,
    pub same_form_retries: u32,
}

impl BudgetState {
    /// Nom du premier plafond atteint, s'il y en a un (→ `HUMAN_REQUIRED`).
    pub fn exhausted(&self, budget: &Budget) -> Option<&'static str> {
        if self.mutations_applied >= budget.max_mutations_per_task {
            Some("max_mutations_per_task")
        } else if self.consecutive_failures >= budget.max_consecutive_failures {
            Some("max_consecutive_failures")
        } else if self.same_form_retries > budget.max_same_form_retries {
            Some("max_same_form_retries")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_is_within_budget() {
        assert_eq!(BudgetState::default().exhausted(&Budget::default()), None);
    }

    #[test]
    fn each_ceiling_is_reported() {
        let b = Budget::default();
        let s = |m, f, r| BudgetState {
            mutations_applied: m,
            consecutive_failures: f,
            same_form_retries: r,
        };
        assert_eq!(s(3, 0, 0).exhausted(&b), Some("max_mutations_per_task"));
        assert_eq!(s(0, 3, 0).exhausted(&b), Some("max_consecutive_failures"));
        assert_eq!(s(0, 0, 2).exhausted(&b), Some("max_same_form_retries"));
        assert_eq!(s(2, 2, 1).exhausted(&b), None);
    }
}
