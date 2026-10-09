use crate::{MorphError, display};
use agon_core::{Form, FormId};
use std::collections::BTreeMap;

/// Historique append-only des Forms d'une session. Les Forms abandonnées après un rollback
/// restent consultables ; seul le pointeur `current` bouge.
#[derive(Debug)]
pub struct History {
    forms: BTreeMap<FormId, Form>,
    parents: BTreeMap<FormId, Option<FormId>>,
    current: FormId,
}

impl History {
    pub fn new(root: Form) -> Self {
        let id = root.id();
        History {
            forms: BTreeMap::from([(id.clone(), root)]),
            parents: BTreeMap::from([(id.clone(), None)]),
            current: id,
        }
    }

    pub fn current_id(&self) -> &FormId {
        &self.current
    }

    pub fn current(&self) -> &Form {
        &self.forms[&self.current]
    }

    pub fn get(&self, id: &FormId) -> Option<&Form> {
        self.forms.get(id)
    }

    /// Enregistre `form` comme enfant de la Form courante et en fait la Form courante.
    /// Si une Form identique existe déjà (même hash), elle est réutilisée sans changer son parent.
    pub fn commit(&mut self, form: Form) -> FormId {
        let id = form.id();
        if !self.forms.contains_key(&id) {
            self.parents.insert(id.clone(), Some(self.current.clone()));
            self.forms.insert(id.clone(), form);
        }
        self.current = id.clone();
        id
    }

    /// Ancêtres stricts de la Form courante, du plus proche au plus ancien.
    pub fn ancestors(&self) -> Vec<&FormId> {
        let mut out = Vec::new();
        let mut cursor = self.parents.get(&self.current).and_then(|p| p.as_ref());
        while let Some(id) = cursor {
            out.push(id);
            cursor = self.parents.get(id).and_then(|p| p.as_ref());
        }
        out
    }

    /// Rollback (§13.2) : uniquement vers une Form ancêtre déjà connue ; jamais vers une Form inconnue.
    pub fn rollback(&mut self, to: &FormId) -> Result<&Form, MorphError> {
        if !self.forms.contains_key(to) {
            return Err(MorphError::UnknownForm(display(to)));
        }
        if !self.ancestors().contains(&to) {
            return Err(MorphError::NotAnAncestor(display(to)));
        }
        self.current = to.clone();
        Ok(self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::apply;
    use crate::apply::tests::{mutation, op};
    use agon_core::fixtures::*;
    use agon_core::mutation::{OpKind, Target};

    fn step(h: &mut History, values: &[&str]) -> FormId {
        let m = mutation(
            h.current(),
            vec![op(OpKind::Add, Target::Capabilities, values)],
        );
        let next = apply(h.current(), &m, &catalog()).unwrap();
        h.commit(next)
    }

    #[test]
    fn scenario_6_rollback_restores_previous_form() {
        let mut h = History::new(base_form());
        let f0 = h.current_id().clone();
        let f1 = step(&mut h, &["docker"]);
        assert_ne!(f0, f1);
        let restored = h.rollback(&f0).unwrap();
        assert_eq!(restored.id(), f0);
        assert_eq!(h.current_id(), &f0);
        assert!(h.get(&f1).is_some(), "history is append-only");
    }

    #[test]
    fn rollback_to_unknown_form_is_refused() {
        let mut h = History::new(base_form());
        let stranger = integration_form().id();
        assert!(matches!(
            h.rollback(&stranger),
            Err(MorphError::UnknownForm(_))
        ));
    }

    #[test]
    fn rollback_to_non_ancestor_is_refused() {
        let mut h = History::new(base_form());
        let f0 = h.current_id().clone();
        let f1 = step(&mut h, &["docker"]);
        h.rollback(&f0).unwrap();
        let _f2 = step(&mut h, &["postgres"]);
        // f1 est une branche abandonnée : connue, mais pas un ancêtre de f2.
        assert!(matches!(h.rollback(&f1), Err(MorphError::NotAnAncestor(_))));
    }

    #[test]
    fn ancestors_are_listed_nearest_first() {
        let mut h = History::new(base_form());
        let f0 = h.current_id().clone();
        let f1 = step(&mut h, &["docker"]);
        let _f2 = step(&mut h, &["postgres"]);
        assert_eq!(h.ancestors(), vec![&f1, &f0]);
    }
}
