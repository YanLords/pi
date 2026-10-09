//! État et logique de service par processus pour Agon Bridge.
//! Encapsule la racine canonique, Project, Form, Lockfile, Permissions, Registry,
//! Guard de détection de falsification et exécution contrôlée de verify/repair.

use crate::lazy::LazyJev;
use crate::project::{Project, ProjectError};
use agon_core::lock::Lockfile;
use agon_core::{CheckId, Form};
use agon_kernel::runner::kill_active_processes;
use agon_kernel::tools::Guard;
use agon_kernel::{
    Activator, CommandActivator, EventKind, Kernel, KernelConfig, KernelParts, Outcome, Session,
};
use agon_morph::Registry;
use agon_policy::{Action, Permission, Permissions};
use agon_verify::run_check;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("project error: {0}")]
    Project(#[from] ProjectError),
    #[error("io error at {0}: {1}")]
    Io(String, std::io::Error),
    #[error("tampering detected in policy files: {0:?}")]
    Tampering(Vec<String>),
    #[error("unknown check `{0}`")]
    UnknownCheck(String),
    #[error("unknown action `{0}`")]
    UnknownAction(String),
    #[error("kernel error: {0}")]
    Kernel(#[from] agon_kernel::KernelError),
    #[error("activation error: {0}")]
    Activation(#[from] agon_kernel::ActivationError),
    #[error("verify error: {0}")]
    Verify(#[from] agon_verify::VerifyError),
    #[error("session error: {0}")]
    Session(#[from] agon_kernel::SessionError),
    #[error("{0}")]
    Other(String),
}

pub struct Service {
    pub root: PathBuf,
    pub project: Project,
    pub form: Form,
    pub lock: Lockfile,
    pub permissions: Permissions,
    pub registry: Registry,
    pub guard: Guard,
    pub request_count: u64,
}

pub fn load_registry(project: &Project) -> Result<Registry, ServiceError> {
    let path = project.registry_path();
    match std::fs::read_to_string(&path) {
        Ok(src) => Registry::from_toml(&src).map_err(|e| {
            ServiceError::Project(ProjectError::Config(format!("{}: {e}", path.display())))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::new()),
        Err(e) => Err(ServiceError::Io(path.display().to_string(), e)),
    }
}

pub fn save_registry(project: &Project, registry: &Registry) -> Result<(), ServiceError> {
    let path = project.registry_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ServiceError::Io(dir.display().to_string(), e))?;
    }
    let toml = registry
        .to_toml()
        .map_err(|e| ServiceError::Other(format!("registry serialize error: {e}")))?;
    std::fs::write(&path, toml).map_err(|e| ServiceError::Io(path.display().to_string(), e))?;
    Ok(())
}

pub fn save_forms(project: &Project, session: &Session) -> Result<(), ServiceError> {
    let dir = project.forms_dir();
    std::fs::create_dir_all(&dir).map_err(|e| ServiceError::Io(dir.display().to_string(), e))?;
    for form in session.forms() {
        let hex = form
            .id()
            .0
            .as_str()
            .trim_start_matches("sha256:")
            .to_string();
        let path = dir.join(format!("{hex}.json"));
        if !path.exists() {
            let s = serde_json::to_string_pretty(form)
                .map_err(|e| ServiceError::Other(e.to_string()))?;
            std::fs::write(&path, s)
                .map_err(|e| ServiceError::Io(path.display().to_string(), e))?;
        }
    }
    Ok(())
}

fn new_session(project: &Project) -> Result<Session, ServiceError> {
    let dir = project.sessions_dir();
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for n in 0..100 {
        let id = if n == 0 {
            format!("s-{base}")
        } else {
            format!("s-{base}-{n}")
        };
        match Session::create(&dir, id) {
            Ok(s) => return Ok(s),
            Err(agon_kernel::SessionError::Io(e))
                if e.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(e) => return Err(ServiceError::Session(e)),
        }
    }
    Err(ServiceError::Other("could not allocate session id".into()))
}

impl Service {
    pub fn new(root: &Path) -> Result<Self, ServiceError> {
        let canonical = root
            .canonicalize()
            .map_err(|e| ServiceError::Io(root.display().to_string(), e))?;
        let project = Project::load(&canonical)?;
        let guard = Guard::snapshot(&canonical);
        let registry = load_registry(&project)?;
        let form = project.base_form();
        let lock = project.resolve_lock();
        let permissions = project.permissions();
        Ok(Service {
            root: canonical,
            project,
            form,
            lock,
            permissions,
            registry,
            guard,
            request_count: 0,
        })
    }

    pub fn check_tampering(&self) -> Result<(), ServiceError> {
        let violations = self.guard.violations();
        if !violations.is_empty() {
            return Err(ServiceError::Tampering(violations));
        }
        Ok(())
    }

    pub fn hello(&mut self) -> Result<serde_json::Value, ServiceError> {
        self.request_count += 1;
        self.check_tampering()?;
        let res = json!({
            "protocol": 1,
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": ["status", "authorize", "verify", "repair", "shutdown"],
            "agon_version": env!("CARGO_PKG_VERSION")
        });
        self.check_tampering()?;
        Ok(res)
    }

    pub fn status(&mut self) -> Result<serde_json::Value, ServiceError> {
        self.request_count += 1;
        let violations = self.guard.violations();
        let active_checks: Vec<String> = self.form.checks.keys().map(|k| k.0.clone()).collect();
        let lock_hash = agon_core::sha256_of(&self.lock)
            .map(|d| d.to_string())
            .unwrap_or_default();
        let budget = &self.project.config.morph.budget;

        Ok(json!({
            "form_id": self.form.id().0,
            "lock_hash": lock_hash,
            "active_checks": active_checks,
            "budget": budget,
            "registry_size": self.registry.len(),
            "tampering_violations": violations
        }))
    }

    pub fn authorize(
        &mut self,
        action_str: &str,
        detail: &str,
    ) -> Result<serde_json::Value, ServiceError> {
        self.request_count += 1;
        self.check_tampering()?;
        let action = match action_str {
            "read" => Action::Read,
            "search" => Action::Search,
            "write" => Action::Write,
            "edit" => Action::Edit,
            "shell" => Action::Shell,
            "git_commit" => Action::GitCommit,
            "git_push" => Action::GitPush,
            other => return Err(ServiceError::UnknownAction(other.to_string())),
        };

        let decision = self.permissions.decide(action, detail);
        let res = match decision {
            Permission::Allow => json!({ "verdict": "allow" }),
            Permission::Confirm { reason } => json!({ "verdict": "confirm", "reason": reason }),
            Permission::Deny { reason } => json!({ "verdict": "deny", "reason": reason }),
        };
        self.check_tampering()?;
        Ok(res)
    }

    pub fn verify(&mut self, check_id: Option<&str>) -> Result<serde_json::Value, ServiceError> {
        self.request_count += 1;
        self.check_tampering()?;
        let form = &self.form;
        let ids: Vec<CheckId> = match check_id {
            Some(id) => {
                let id = CheckId(id.to_string());
                if !self.project.catalog.contains_key(&id) {
                    return Err(ServiceError::UnknownCheck(id.0));
                }
                vec![id]
            }
            None => form.checks.keys().cloned().collect(),
        };

        if ids.is_empty() {
            return Err(ServiceError::Other("no checks defined to verify".into()));
        }

        let activator =
            CommandActivator::new(self.project.capability_specs(), self.project.runner())
                .map_err(|e| ServiceError::Project(ProjectError::Config(e.to_string())))?;
        activator.activate(form).map_err(ServiceError::Activation)?;

        let runner = self.project.runner();
        let mut results = Vec::new();

        for id in &ids {
            let check = &self.project.catalog[id];
            let evidence = run_check(&runner, check).map_err(ServiceError::Verify)?;
            let signature = self.project.extractors.normalize(&evidence.observation);

            results.push(json!({
                "check_id": evidence.check.0,
                "passed": evidence.passed,
                "exit_code": evidence.observation.exit_code,
                "timed_out": evidence.observation.timed_out,
                "stdout": evidence.observation.stdout,
                "stderr": evidence.observation.stderr,
                "signature": {
                    "class": signature.class,
                    "fields": signature.fields,
                },
                "evidence": evidence
            }));
        }

        self.check_tampering()?;
        if results.len() == 1 {
            Ok(results.remove(0))
        } else {
            Ok(json!({ "results": results }))
        }
    }

    pub async fn repair<F>(
        &mut self,
        check_id: &str,
        on_event: F,
    ) -> Result<serde_json::Value, ServiceError>
    where
        F: Fn(&str, serde_json::Value) + Send + Sync + 'static,
    {
        self.request_count += 1;
        self.check_tampering()?;
        let id = CheckId(check_id.to_string());
        if !self.project.catalog.contains_key(&id) {
            return Err(ServiceError::UnknownCheck(check_id.to_string()));
        }

        let activator =
            CommandActivator::new(self.project.capability_specs(), self.project.runner())
                .map_err(|e| ServiceError::Project(ProjectError::Config(e.to_string())))?;
        let mut session = new_session(&self.project)?;

        session.set_sink(move |event| {
            let (event_name, data) = match &event.kind {
                EventKind::SignatureDetected { id, signature } => (
                    "SignatureDetected",
                    json!({ "id": id.0, "signature": signature }),
                ),
                EventKind::MutationProposed { mutation } => (
                    "MutationProposed",
                    json!({ "mutation": mutation }),
                ),
                EventKind::MutationApproved { mutation } => (
                    "MutationApproved",
                    json!({ "mutation": mutation.0 }),
                ),
                EventKind::MutationRejected { mutation, reasons } => (
                    "MutationRejected",
                    json!({ "mutation": mutation.0, "reasons": reasons }),
                ),
                EventKind::DecisionRequested { decision_id, template, request, candidates } => (
                    "DecisionRequested",
                    json!({ "decision_id": decision_id, "template": template.as_str(), "request": request.as_str(), "candidates": candidates }),
                ),
                EventKind::DecisionCompleted { decision_id, jev_model_requested, jev_model_answered, model_drift, outcome, .. } => (
                    "DecisionCompleted",
                    json!({ "decision_id": decision_id, "jev_model_requested": jev_model_requested, "jev_model_answered": jev_model_answered, "model_drift": model_drift, "outcome": outcome }),
                ),
                EventKind::MutationApplied { mutation, parent, child } => (
                    "MutationApplied",
                    json!({ "mutation": mutation.0, "parent": parent.0, "child": child.0 }),
                ),
                EventKind::MutationValidated { mutation } => (
                    "MutationValidated",
                    json!({ "mutation": mutation.0 }),
                ),
                EventKind::MutationUnproven { mutation } => (
                    "MutationUnproven",
                    json!({ "mutation": mutation.0 }),
                ),
                EventKind::TaskCompleted { outcome } => (
                    "TaskCompleted",
                    json!({ "outcome": outcome }),
                ),
                _ => return,
            };
            on_event(event_name, data);
        });

        let config = KernelConfig {
            budget: self.project.config.morph.budget,
            thresholds: self.project.config.jev.thresholds,
            jev_model: self.project.config.jev.model.clone(),
            equivalences: vec![],
        };

        let known_before = self.registry.len();
        let decisions = LazyJev::new(
            self.project.jev_config(),
            self.project.config.jev.api_key_env.clone(),
        );

        let mut kernel = Kernel::new(KernelParts {
            decisions,
            runner: self.project.runner(),
            activator,
            config,
            catalog: self.project.catalog.clone(),
            extractors: self.project.extractors.clone(),
            candidates: self.project.candidates.clone(),
            registry: self.registry.clone(),
            form: self.form.clone(),
            session,
        })?;

        let outcome = kernel.repair(&id).await?;

        // Persister les Forms et le registre
        save_forms(&self.project, kernel.session())?;
        if kernel.registry().len() != known_before {
            save_registry(&self.project, kernel.registry())?;
        }
        self.form = kernel.history().current().clone();
        self.registry = kernel.into_registry();
        self.guard = Guard::snapshot(&self.root);

        let res = match outcome {
            Outcome::Passed => json!({ "outcome": "Passed" }),
            Outcome::Flaky => json!({ "outcome": "Flaky" }),
            Outcome::NoChange => json!({ "outcome": "NoChange" }),
            Outcome::Fixed {
                mutation,
                proof,
                form,
            } => json!({
                "outcome": "Fixed",
                "mutation": mutation.0,
                "proof": proof,
                "form": form.0
            }),
            Outcome::HumanRequired { why } => json!({
                "outcome": "HumanRequired",
                "why": format!("{why:?}")
            }),
        };

        self.check_tampering()?;
        Ok(res)
    }

    pub fn shutdown(&mut self) -> Result<serde_json::Value, ServiceError> {
        self.request_count += 1;
        kill_active_processes();
        Ok(json!({ "status": "shutdown" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agon-service-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".agon")).unwrap();
        for (f, c) in files {
            std::fs::write(root.join(".agon").join(f), c).unwrap();
        }
        root
    }

    const CFG: &str = r#"
[form]
tools = ["cargo"]
checks = ["verify.ok"]

[permissions]
shell_allow = ["cargo test"]
"#;

    const CHECKS: &str = r#"
[check."verify.ok"]
command = "true"
success = "exit_code == 0"

[check."verify.fail"]
command = "false"
success = "exit_code == 0"
"#;

    #[test]
    fn service_initialization_and_status() {
        let root = temp_project("init", &[("config.toml", CFG), ("checks.toml", CHECKS)]);
        let mut s = Service::new(&root).unwrap();

        let hello = s.hello().unwrap();
        assert_eq!(hello["protocol"], 1);

        let status = s.status().unwrap();
        assert_eq!(status["active_checks"], json!(["verify.ok"]));
        assert_eq!(status["tampering_violations"], json!([]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn service_authorization() {
        let root = temp_project("auth", &[("config.toml", CFG), ("checks.toml", CHECKS)]);
        let mut s = Service::new(&root).unwrap();

        assert_eq!(
            s.authorize("read", "src/lib.rs").unwrap()["verdict"],
            "allow"
        );
        assert_eq!(
            s.authorize("shell", "cargo test").unwrap()["verdict"],
            "allow"
        );
        assert_eq!(s.authorize("shell", "git push").unwrap()["verdict"], "deny");
        assert_eq!(
            s.authorize("write", "foo.txt").unwrap()["verdict"],
            "confirm"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn service_verify_success_and_failure() {
        let root = temp_project("verify", &[("config.toml", CFG), ("checks.toml", CHECKS)]);
        let mut s = Service::new(&root).unwrap();

        let ok = s.verify(Some("verify.ok")).unwrap();
        assert_eq!(ok["passed"], true);
        assert_eq!(ok["exit_code"], 0);

        let fail = s.verify(Some("verify.fail")).unwrap();
        assert_eq!(fail["passed"], false);
        assert_ne!(fail["exit_code"], 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tampering_blocks_operations() {
        let root = temp_project("tamper", &[("config.toml", CFG), ("checks.toml", CHECKS)]);
        let mut s = Service::new(&root).unwrap();
        assert!(s.check_tampering().is_ok());

        // Modifie checks.toml
        std::fs::write(root.join(".agon/checks.toml"), "corrupted").unwrap();
        let err = s.authorize("read", "a").unwrap_err();
        assert!(matches!(err, ServiceError::Tampering(_)));

        // Status rapporte les violations
        let status = s.status().unwrap();
        let violations = status["tampering_violations"].as_array().unwrap();
        assert!(!violations.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
