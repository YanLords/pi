//! Activation d'une Form (§8.3) : les effets de bord (démarrer Postgres, lancer un service…)
//! relèvent du Kernel via des *capability providers*, jamais de Morph.

use agon_core::Form;

#[derive(Debug, thiserror::Error)]
#[error("activation failed: {0}")]
pub struct ActivationError(pub String);

pub trait Activator {
    /// Amène l'environnement dans l'état décrit par la Form. Doit être idempotent : le Kernel
    /// l'appelle aussi pour revenir à une Form ancêtre (rollback).
    fn activate(&self, form: &Form) -> Result<(), ActivationError>;
}

/// Activation sans effet : convient tant qu'aucun capability provider n'est branché.
pub struct NoopActivator;

impl Activator for NoopActivator {
    fn activate(&self, _form: &Form) -> Result<(), ActivationError> {
        Ok(())
    }
}

// ── activation par commandes déclarées ─────────────────────────────────────────────────────

use crate::runner::ProcessRunner;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Capacité déclarée par l'humain dans `.agon/` : ce sont ces commandes, et elles seules, que
/// l'activation peut exécuter. Une mutation ne fait que *nommer* une capacité (§9.1, §40).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySpec {
    pub activate: Option<String>,
    pub deactivate: Option<String>,
    /// Capacités à démarrer avant celle-ci (et à arrêter après elle).
    pub requires: Vec<String>,
}

/// Active les capacités d'une Form en exécutant leurs commandes déclarées, dans l'ordre des
/// dépendances, et arrête celles qui ne sont plus voulues. Idempotent : une capacité déjà active
/// n'est pas relancée. L'état « actif » est celui que ce processus a lui-même démarré.
pub struct CommandActivator {
    specs: BTreeMap<String, CapabilitySpec>,
    runner: ProcessRunner,
    active: Mutex<BTreeSet<String>>,
}

impl CommandActivator {
    pub fn new(
        specs: BTreeMap<String, CapabilitySpec>,
        runner: ProcessRunner,
    ) -> Result<Self, ActivationError> {
        let this = CommandActivator {
            specs,
            runner,
            active: Mutex::new(BTreeSet::new()),
        };
        for name in this.specs.keys() {
            this.depth(name, &mut Vec::new())?; // détecte cycles et dépendances inconnues
        }
        Ok(this)
    }

    /// Capacités actuellement démarrées par cet activateur.
    pub fn active(&self) -> BTreeSet<String> {
        self.active.lock().unwrap().clone()
    }

    fn depth(&self, name: &str, path: &mut Vec<String>) -> Result<usize, ActivationError> {
        if path.iter().any(|p| p == name) {
            return Err(ActivationError(format!(
                "dependency cycle: {} -> {name}",
                path.join(" -> ")
            )));
        }
        let Some(spec) = self.specs.get(name) else {
            return Ok(0);
        };
        path.push(name.to_string());
        let mut d = 0;
        for r in &spec.requires {
            if !self.specs.contains_key(r) {
                return Err(ActivationError(format!(
                    "capability `{name}` requires unknown capability `{r}`"
                )));
            }
            d = d.max(1 + self.depth(r, path)?);
        }
        path.pop();
        Ok(d)
    }

    fn exec(&self, capability: &str, verb: &str, command: &str) -> Result<(), ActivationError> {
        let obs = self.runner.exec(
            format!("capability.{capability}.{verb}").as_str().into(),
            command,
        );
        if obs.exit_code == Some(0) {
            return Ok(());
        }
        let detail: String = obs.stderr.trim().chars().take(300).collect();
        Err(ActivationError(format!(
            "`{capability}` {verb} failed ({}){}{}",
            if obs.timed_out {
                "timeout".to_string()
            } else {
                format!("exit {:?}", obs.exit_code)
            },
            if detail.is_empty() { "" } else { ": " },
            detail
        )))
    }
}

impl Activator for CommandActivator {
    fn activate(&self, form: &Form) -> Result<(), ActivationError> {
        let want = &form.capabilities;
        for c in want {
            for r in self
                .specs
                .get(c)
                .map(|s| s.requires.as_slice())
                .unwrap_or(&[])
            {
                if !want.contains(r) {
                    return Err(ActivationError(format!(
                        "capability `{c}` requires `{r}`, which the form does not enable"
                    )));
                }
            }
        }
        let mut active = self.active.lock().unwrap();
        let depth = |c: &String| self.depth(c, &mut Vec::new()).unwrap_or(0);

        // Arrêt : les plus dépendantes d'abord.
        let mut stop: Vec<String> = active.difference(want).cloned().collect();
        stop.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.cmp(b)));
        for c in stop {
            if let Some(cmd) = self.specs.get(&c).and_then(|s| s.deactivate.as_deref()) {
                self.exec(&c, "deactivate", cmd)?;
            }
            active.remove(&c);
        }

        // Démarrage : les dépendances d'abord.
        let mut start: Vec<String> = want.difference(&active).cloned().collect();
        start.sort_by(|a, b| depth(a).cmp(&depth(b)).then_with(|| a.cmp(b)));
        for c in start {
            if let Some(cmd) = self.specs.get(&c).and_then(|s| s.activate.as_deref()) {
                self.exec(&c, "activate", cmd)?;
            }
            active.insert(c);
        }
        Ok(())
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use agon_core::fixtures::base_form;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agon-act-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn spec(name: &str, requires: &[&str]) -> (String, CapabilitySpec) {
        (
            name.to_string(),
            CapabilitySpec {
                activate: Some(format!("echo start-{name} >> log")),
                deactivate: Some(format!("echo stop-{name} >> log")),
                requires: requires.iter().map(|s| s.to_string()).collect(),
            },
        )
    }

    fn form_with(caps: &[&str]) -> Form {
        let mut f = base_form();
        f.capabilities = caps.iter().map(|s| s.to_string()).collect();
        f
    }

    fn log(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn activator(dir: &std::path::Path) -> CommandActivator {
        let specs = BTreeMap::from([
            spec("docker", &[]),
            spec("postgres", &["docker"]),
            spec("app", &["postgres"]),
        ]);
        CommandActivator::new(specs, ProcessRunner::new(dir)).unwrap()
    }

    #[test]
    fn starts_dependencies_first_and_is_idempotent() {
        let dir = scratch("order");
        let a = activator(&dir);
        a.activate(&form_with(&["app", "docker", "postgres"]))
            .unwrap();
        assert_eq!(log(&dir), ["start-docker", "start-postgres", "start-app"]);
        a.activate(&form_with(&["app", "docker", "postgres"]))
            .unwrap();
        assert_eq!(
            log(&dir).len(),
            3,
            "already-active capabilities are not restarted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stops_dependents_first_when_the_form_no_longer_wants_them() {
        let dir = scratch("stop");
        let a = activator(&dir);
        a.activate(&form_with(&["app", "docker", "postgres"]))
            .unwrap();
        a.activate(&form_with(&[])).unwrap();
        assert_eq!(
            &log(&dir)[3..],
            ["stop-app", "stop-postgres", "stop-docker"]
        );
        assert!(a.active().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_to_a_smaller_form_stops_only_the_difference() {
        let dir = scratch("rollback");
        let a = activator(&dir);
        a.activate(&form_with(&["docker"])).unwrap();
        a.activate(&form_with(&["docker", "postgres"])).unwrap();
        a.activate(&form_with(&["docker"])).unwrap();
        assert_eq!(
            log(&dir),
            ["start-docker", "start-postgres", "stop-postgres"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_form_missing_a_required_capability_is_refused_before_running_anything() {
        let dir = scratch("requires");
        let err = activator(&dir)
            .activate(&form_with(&["postgres"]))
            .unwrap_err();
        assert!(err.0.contains("requires `docker`"), "{err}");
        assert!(log(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_command_is_reported_and_the_capability_stays_inactive() {
        let dir = scratch("fail");
        let bad = CapabilitySpec {
            activate: Some("echo boom >&2; exit 7".into()),
            ..CapabilitySpec::default()
        };
        let a = CommandActivator::new(
            BTreeMap::from([("db".to_string(), bad)]),
            ProcessRunner::new(&dir),
        )
        .unwrap();
        let err = a.activate(&form_with(&["db"])).unwrap_err();
        assert!(
            err.0.contains("exit Some(7)") && err.0.contains("boom"),
            "{err}"
        );
        assert!(a.active().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capabilities_without_a_spec_are_declarative_labels() {
        let dir = scratch("label");
        let a = activator(&dir);
        a.activate(&form_with(&["docker", "whatever"])).unwrap();
        assert!(a.active().contains("whatever"));
        assert_eq!(log(&dir), ["start-docker"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cycles_and_unknown_requirements_are_rejected_at_construction() {
        let dir = scratch("cycle");
        let cyc = BTreeMap::from([spec("a", &["b"]), spec("b", &["a"])]);
        assert!(CommandActivator::new(cyc, ProcessRunner::new(&dir)).is_err());
        let unknown = BTreeMap::from([spec("a", &["ghost"])]);
        assert!(CommandActivator::new(unknown, ProcessRunner::new(&dir)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
