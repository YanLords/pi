use crate::{MorphError, display};
use agon_core::mutation::{Op, OpKind, Target};
use agon_core::{CheckCatalog, Form, Mutation};
use std::collections::BTreeSet;

/// Applique une mutation à une Form et renvoie la Form candidate.
///
/// Morph ne juge pas l'opportunité (Policy) : il refuse seulement ce qui est structurellement
/// impossible (parent incohérent, opération mal formée, retrait ou redéfinition de check).
pub fn apply(
    parent: &Form,
    mutation: &Mutation,
    catalog: &CheckCatalog,
) -> Result<Form, MorphError> {
    let actual = parent.id();
    if mutation.parent_form != actual {
        return Err(MorphError::ParentMismatch {
            expected: display(&mutation.parent_form),
            actual: display(&actual),
        });
    }
    let mut form = parent.clone();
    for op in &mutation.ops {
        if !op.is_well_formed() {
            return Err(MorphError::MalformedOp(format!(
                "{:?} {:?}",
                op.op, op.target
            )));
        }
        match op.target {
            Target::Capabilities => edit(&mut form.capabilities, op),
            Target::Tools => edit(&mut form.tools, op),
            Target::Modules => edit(&mut form.modules, op),
            Target::Context => edit(&mut form.context, op),
            Target::Model => form.model = op.value[0].clone(),
            Target::Checks => add_checks(&mut form, op, catalog)?,
        }
    }
    Ok(form)
}

fn edit(set: &mut BTreeSet<String>, op: &Op) {
    match op.op {
        OpKind::Add => set.extend(op.value.iter().cloned()),
        OpKind::Remove => {
            for v in &op.value {
                set.remove(v);
            }
        }
        OpKind::Set => *set = op.value.iter().cloned().collect(),
    }
}

fn add_checks(form: &mut Form, op: &Op, catalog: &CheckCatalog) -> Result<(), MorphError> {
    if op.op != OpKind::Add {
        return Err(MorphError::ChecksAreAppendOnly);
    }
    for name in &op.value {
        let id = agon_core::CheckId(name.clone());
        let check = catalog
            .get(&id)
            .ok_or_else(|| MorphError::UnknownCheck(id.clone()))?;
        let hash = check.definition_hash();
        match form.checks.get(&id) {
            Some(existing) if *existing != hash => return Err(MorphError::CheckRedefinition(id)),
            _ => {
                form.checks.insert(id, hash);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agon_core::fixtures::*;
    use agon_core::mutation::{Origin, Scope, Trigger};
    use agon_core::{MutationId, Signature};

    pub fn mutation(parent: &Form, ops: Vec<Op>) -> Mutation {
        let sig = Signature::unknown("verify.integration".into());
        Mutation {
            id: MutationId("m-001".into()),
            parent_form: parent.id(),
            scope: Scope::Task,
            origin: Origin::Catalog,
            trigger: Trigger {
                check: "verify.integration".into(),
                signature: sig.id(),
            },
            success_check: "verify.integration".into(),
            ops,
        }
    }

    pub fn op(kind: OpKind, target: Target, values: &[&str]) -> Op {
        Op {
            op: kind,
            target,
            value: values.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn adds_capabilities_and_checks() {
        let parent = base_form();
        let m = mutation(
            &parent,
            vec![
                op(OpKind::Add, Target::Capabilities, &["docker", "postgres"]),
                op(OpKind::Add, Target::Checks, &["verify.integration"]),
            ],
        );
        let child = apply(&parent, &m, &catalog()).unwrap();
        assert_eq!(child, integration_form());
        assert_ne!(child.id(), parent.id());
    }

    #[test]
    fn apply_is_deterministic() {
        let parent = base_form();
        let m = mutation(
            &parent,
            vec![op(OpKind::Add, Target::Capabilities, &["docker"])],
        );
        assert_eq!(
            apply(&parent, &m, &catalog()).unwrap().id(),
            apply(&parent, &m, &catalog()).unwrap().id()
        );
    }

    #[test]
    fn stale_parent_is_rejected() {
        let m = mutation(
            &base_form(),
            vec![op(OpKind::Add, Target::Capabilities, &["docker"])],
        );
        let err = apply(&integration_form(), &m, &catalog()).unwrap_err();
        assert!(matches!(err, MorphError::ParentMismatch { .. }));
    }

    #[test]
    fn checks_cannot_be_removed_or_replaced() {
        let parent = integration_form();
        for kind in [OpKind::Remove, OpKind::Set] {
            let m = mutation(&parent, vec![op(kind, Target::Checks, &["verify.build"])]);
            assert_eq!(
                apply(&parent, &m, &catalog()),
                Err(MorphError::ChecksAreAppendOnly)
            );
        }
    }

    #[test]
    fn unknown_check_and_redefinition_are_rejected() {
        let parent = base_form();
        let m = mutation(
            &parent,
            vec![op(OpKind::Add, Target::Checks, &["verify.nope"])],
        );
        assert!(matches!(
            apply(&parent, &m, &catalog()),
            Err(MorphError::UnknownCheck(_))
        ));

        let mut changed = catalog();
        changed.get_mut(&"verify.build".into()).unwrap().command = "cargo build --release".into();
        let m = mutation(
            &parent,
            vec![op(OpKind::Add, Target::Checks, &["verify.build"])],
        );
        assert!(matches!(
            apply(&parent, &m, &changed),
            Err(MorphError::CheckRedefinition(_))
        ));
    }

    #[test]
    fn malformed_model_op_is_rejected() {
        let parent = base_form();
        let m = mutation(&parent, vec![op(OpKind::Add, Target::Model, &["large"])]);
        assert!(matches!(
            apply(&parent, &m, &catalog()),
            Err(MorphError::MalformedOp(_))
        ));
    }

    #[test]
    fn set_model_changes_only_model() {
        let parent = base_form();
        let m = mutation(&parent, vec![op(OpKind::Set, Target::Model, &["large"])]);
        let child = apply(&parent, &m, &catalog()).unwrap();
        assert_eq!(child.model, "large");
        assert_eq!(child.checks, parent.checks);
        assert_eq!(child.policies, parent.policies);
    }
}
