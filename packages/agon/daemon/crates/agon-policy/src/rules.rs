use crate::budget::{Budget, BudgetState};
use agon_core::{CheckCatalog, CheckId, Form, Mutation};
use std::collections::BTreeSet;

/// Remplacement de check explicitement approuvé par un humain (§13.1).
/// Morph et Policy ne déduisent jamais eux-mêmes une équivalence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Equivalence {
    pub from: CheckId,
    pub to: CheckId,
    pub approved_by: String,
}

impl Equivalence {
    fn is_human_approved(&self) -> bool {
        self.approved_by == "human"
    }
}

pub struct Evaluation<'a> {
    pub parent: &'a Form,
    pub candidate: &'a Form,
    pub mutation: &'a Mutation,
    pub catalog: &'a CheckCatalog,
    pub equivalences: &'a [Equivalence],
    pub budget: &'a Budget,
    pub state: &'a BudgetState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// `C(candidate) ⊇ C(parent)` n'est pas respecté (§13).
    NotMonotone {
        missing: Vec<CheckId>,
    },
    /// Une dépendance de check satisfaite dans le parent ne l'est plus (§14).
    BrokenDependency {
        check: CheckId,
        dependency: String,
    },
    /// La définition d'un check existant a changé.
    CheckDefinitionChanged(CheckId),
    /// L'instantané des policies a été modifié.
    PolicySnapshotChanged,
    MalformedOp,
    ParentMismatch,
    /// Check actif absent du catalogue : ses dépendances ne peuvent pas être analysées.
    UnknownCheck(CheckId),
    BudgetExhausted(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny(Vec<Violation>),
}

impl Verdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Verdict::Allow)
    }
}

/// Évalue une transition. Renvoie *toutes* les violations, pas seulement la première.
pub fn evaluate(e: &Evaluation<'_>) -> Verdict {
    let mut v = Vec::new();

    if e.mutation.parent_form != e.parent.id() {
        v.push(Violation::ParentMismatch);
    }
    if e.mutation.ops.iter().any(|op| !op.is_well_formed()) {
        v.push(Violation::MalformedOp);
    }
    if let Some(name) = e.state.exhausted(e.budget) {
        v.push(Violation::BudgetExhausted(name));
    }
    if e.candidate.policies != e.parent.policies {
        v.push(Violation::PolicySnapshotChanged);
    }
    monotone_checks(e, &mut v);
    changed_definitions(e, &mut v);
    broken_dependencies(e, &mut v);

    if v.is_empty() {
        Verdict::Allow
    } else {
        Verdict::Deny(v)
    }
}

fn monotone_checks(e: &Evaluation<'_>, v: &mut Vec<Violation>) {
    let missing: Vec<CheckId> = e
        .parent
        .checks
        .keys()
        .filter(|id| !e.candidate.checks.contains_key(*id) && !replaced_by_equivalent(e, id))
        .cloned()
        .collect();
    if !missing.is_empty() {
        v.push(Violation::NotMonotone { missing });
    }
}

fn replaced_by_equivalent(e: &Evaluation<'_>, id: &CheckId) -> bool {
    e.equivalences
        .iter()
        .any(|q| q.is_human_approved() && &q.from == id && e.candidate.checks.contains_key(&q.to))
}

fn changed_definitions(e: &Evaluation<'_>, v: &mut Vec<Violation>) {
    for (id, hash) in &e.parent.checks {
        if let Some(new) = e.candidate.checks.get(id)
            && new != hash
        {
            v.push(Violation::CheckDefinitionChanged(id.clone()));
        }
    }
}

fn provided(f: &Form) -> BTreeSet<&String> {
    f.capabilities
        .iter()
        .chain(&f.tools)
        .chain(&f.modules)
        .collect()
}

/// Refuse toute mutation qui retire ce dont un check actif dépend, sauf si la dépendance
/// n'était déjà pas satisfaite dans le parent (c'est précisément le cas qu'une mutation répare).
fn broken_dependencies(e: &Evaluation<'_>, v: &mut Vec<Violation>) {
    let before = provided(e.parent);
    let after = provided(e.candidate);
    for id in e.candidate.checks.keys() {
        let Some(check) = e.catalog.get(id) else {
            v.push(Violation::UnknownCheck(id.clone()));
            continue;
        };
        for dep in &check.dependencies {
            if before.contains(dep) && !after.contains(dep) {
                v.push(Violation::BrokenDependency {
                    check: id.clone(),
                    dependency: dep.clone(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agon_core::fixtures::*;
    use agon_core::mutation::{Op, OpKind, Origin, Scope, Target, Trigger};
    use agon_core::{Digest, MutationId, Signature};

    fn mutation_for(parent: &Form, ops: Vec<Op>) -> Mutation {
        Mutation {
            id: MutationId("m-001".into()),
            parent_form: parent.id(),
            scope: Scope::Task,
            origin: Origin::Catalog,
            trigger: Trigger {
                check: "verify.integration".into(),
                signature: Signature::unknown("verify.integration".into()).id(),
            },
            success_check: "verify.integration".into(),
            ops,
        }
    }

    fn op(kind: OpKind, target: Target, values: &[&str]) -> Op {
        Op {
            op: kind,
            target,
            value: values.iter().map(|s| s.to_string()).collect(),
        }
    }

    struct Fixture {
        catalog: CheckCatalog,
        budget: Budget,
        state: BudgetState,
        equivalences: Vec<Equivalence>,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                catalog: catalog(),
                budget: Budget::default(),
                state: BudgetState::default(),
                equivalences: vec![],
            }
        }
        fn eval(&self, parent: &Form, candidate: &Form, m: &Mutation) -> Verdict {
            evaluate(&Evaluation {
                parent,
                candidate,
                mutation: m,
                catalog: &self.catalog,
                equivalences: &self.equivalences,
                budget: &self.budget,
                state: &self.state,
            })
        }
    }

    /// Applique via Morph puis évalue : reproduit le chemin réel du Kernel.
    fn through_morph(fx: &Fixture, parent: &Form, ops: Vec<Op>) -> Verdict {
        let m = mutation_for(parent, ops);
        let candidate = agon_morph::apply(parent, &m, &fx.catalog).expect("morph applies");
        fx.eval(parent, &candidate, &m)
    }

    #[test]
    fn adding_missing_dependencies_is_allowed() {
        let fx = Fixture::new();
        let v = through_morph(
            &fx,
            &base_form(),
            vec![
                op(OpKind::Add, Target::Capabilities, &["docker", "postgres"]),
                op(OpKind::Add, Target::Checks, &["verify.integration"]),
            ],
        );
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn scenario_3_removing_docker_is_denied() {
        let fx = Fixture::new();
        let v = through_morph(
            &fx,
            &integration_form(),
            vec![op(OpKind::Remove, Target::Capabilities, &["docker"])],
        );
        assert_eq!(
            v,
            Verdict::Deny(vec![Violation::BrokenDependency {
                check: "verify.integration".into(),
                dependency: "docker".into()
            }])
        );
    }

    #[test]
    fn removing_an_unused_capability_is_allowed() {
        let fx = Fixture::new();
        let mut parent = integration_form();
        parent.capabilities.insert("redis".into());
        assert_eq!(
            through_morph(
                &fx,
                &parent,
                vec![op(OpKind::Remove, Target::Capabilities, &["redis"])]
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn dropping_a_check_breaks_monotonicity() {
        let fx = Fixture::new();
        let parent = integration_form();
        let m = mutation_for(&parent, vec![op(OpKind::Add, Target::Tools, &["x"])]);
        let mut candidate = parent.clone();
        candidate.checks.remove(&"verify.integration".into());
        let v = fx.eval(&parent, &candidate, &m);
        assert_eq!(
            v,
            Verdict::Deny(vec![Violation::NotMonotone {
                missing: vec!["verify.integration".into()]
            }])
        );
    }

    #[test]
    fn human_approved_equivalence_allows_replacement() {
        let mut fx = Fixture::new();
        let parent = integration_form();
        let m = mutation_for(&parent, vec![op(OpKind::Add, Target::Tools, &["biome"])]);
        let mut candidate = parent.clone();
        candidate.checks.remove(&"verify.build".into());
        candidate
            .checks
            .insert("verify.build2".into(), Digest::of_bytes(b"other"));
        fx.catalog.insert("verify.build2".into(), {
            let mut c = build_check();
            c.id = "verify.build2".into();
            c
        });

        assert!(
            !fx.eval(&parent, &candidate, &m).is_allowed(),
            "no equivalence yet"
        );

        fx.equivalences.push(Equivalence {
            from: "verify.build".into(),
            to: "verify.build2".into(),
            approved_by: "model".into(),
        });
        assert!(
            !fx.eval(&parent, &candidate, &m).is_allowed(),
            "only a human can approve"
        );

        fx.equivalences[0].approved_by = "human".into();
        assert_eq!(fx.eval(&parent, &candidate, &m), Verdict::Allow);
    }

    #[test]
    fn check_definition_and_policy_snapshot_are_read_only() {
        let fx = Fixture::new();
        let parent = integration_form();
        let m = mutation_for(&parent, vec![op(OpKind::Add, Target::Tools, &["x"])]);
        let mut candidate = parent.clone();
        candidate
            .checks
            .insert("verify.build".into(), Digest::of_bytes(b"weakened"));
        candidate.policies = Digest::of_bytes(b"policies-relaxed");
        let Verdict::Deny(v) = fx.eval(&parent, &candidate, &m) else {
            panic!("must deny")
        };
        assert!(v.contains(&Violation::CheckDefinitionChanged("verify.build".into())));
        assert!(v.contains(&Violation::PolicySnapshotChanged));
    }

    #[test]
    fn exhausted_budget_denies_and_reports_the_ceiling() {
        let mut fx = Fixture::new();
        fx.state.mutations_applied = 3;
        let v = through_morph(
            &fx,
            &base_form(),
            vec![op(OpKind::Add, Target::Capabilities, &["docker"])],
        );
        assert_eq!(
            v,
            Verdict::Deny(vec![Violation::BudgetExhausted("max_mutations_per_task")])
        );
    }

    #[test]
    fn stale_parent_is_denied() {
        let fx = Fixture::new();
        let stale = mutation_for(&base_form(), vec![op(OpKind::Add, Target::Tools, &["x"])]);
        let parent = integration_form();
        let v = fx.eval(&parent, &parent, &stale);
        assert_eq!(v, Verdict::Deny(vec![Violation::ParentMismatch]));
    }

    #[test]
    fn all_violations_are_reported_together() {
        let mut fx = Fixture::new();
        fx.state.consecutive_failures = 3;
        let parent = integration_form();
        let m = mutation_for(
            &parent,
            vec![op(OpKind::Remove, Target::Capabilities, &["docker"])],
        );
        let candidate = agon_morph::apply(&parent, &m, &fx.catalog).unwrap();
        let Verdict::Deny(v) = fx.eval(&parent, &candidate, &m) else {
            panic!("must deny")
        };
        assert_eq!(v.len(), 2);
    }
}
