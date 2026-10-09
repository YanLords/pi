//! Chargement d'un projet Agon : `.agon/config.toml`, `checks.toml`, `extractors.toml`,
//! `candidates.toml`. Fichiers de configuration de confiance humaine avec validation stricte.

use agon_core::lock::{DecisionLock, LockEntry, Lockfile, Precision};
use agon_core::{Check, CheckCatalog, CheckId, Digest, Form, sha256_of};
use agon_kernel::{CandidateCatalog, CapabilitySpec, ProcessRunner, Tier};
use agon_model::jev::gating::Thresholds;
use agon_model::jev::questions::mutation_template_hash;
use agon_model::{ChatConfig, JevConfig};
use agon_policy::{Budget, Permissions};
use agon_verify::ExtractorSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

pub const DIR: &str = ".agon";

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("io error at {0}: {1}")]
    Io(String, std::io::Error),
    #[error("kernel error: {0}")]
    Kernel(#[from] agon_kernel::KernelError),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct FormCfg {
    pub modules: Vec<String>,
    pub tools: Vec<String>,
    pub capabilities: Vec<String>,
    pub context: Vec<String>,
    /// Identifiants des checks actifs (définis dans `checks.toml`).
    pub checks: Vec<String>,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct JevCfg {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub thresholds: Thresholds,
}

impl Default for JevCfg {
    fn default() -> Self {
        JevCfg {
            base_url: "https://api.typesafe.ai".into(),
            model: "jev-1.13.0".into(),
            api_key_env: "TYPESAFE_API_KEY".into(),
            timeout_ms: 5000,
            max_retries: 3,
            thresholds: Thresholds::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MorphCfg {
    pub budget: Budget,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RunnerCfg {
    pub timeout_secs: u64,
    /// Variables d'environnement transmises en plus de la liste par défaut.
    pub env: Vec<String>,
}

impl Default for RunnerCfg {
    fn default() -> Self {
        RunnerCfg {
            timeout_secs: 300,
            env: vec![],
        }
    }
}

/// Modèles génératifs : API compatible OpenAI, un identifiant de modèle par niveau.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelsCfg {
    pub base_url: String,
    pub api_key_env: String,
    pub small: Option<String>,
    pub medium: Option<String>,
    pub large: Option<String>,
    pub default_tier: Tier,
    pub timeout_secs: u64,
    pub max_retries: u32,
}

impl Default for ModelsCfg {
    fn default() -> Self {
        ModelsCfg {
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key_env: "OPENROUTER_API_KEY".into(),
            small: None,
            medium: None,
            large: None,
            default_tier: Tier::Medium,
            timeout_secs: 120,
            max_retries: 2,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentCfg {
    pub max_steps: u32,
    pub max_rounds: u32,
    pub max_tokens: Option<u32>,
    pub jev_preflight: bool,
}

impl Default for AgentCfg {
    fn default() -> Self {
        AgentCfg {
            max_steps: 25,
            max_rounds: 3,
            max_tokens: None,
            jev_preflight: true,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct CapabilityCfg {
    pub activate: Option<String>,
    pub deactivate: Option<String>,
    pub requires: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub form: FormCfg,
    pub jev: JevCfg,
    pub morph: MorphCfg,
    pub runner: RunnerCfg,
    pub models: ModelsCfg,
    pub agent: AgentCfg,
    pub permissions: Permissions,
    pub capability: BTreeMap<String, CapabilityCfg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckDef {
    command: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default = "default_success")]
    success: String,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default = "one")]
    version: u32,
}

fn default_success() -> String {
    "exit_code == 0".into()
}
fn one() -> u32 {
    1
}

#[derive(Deserialize)]
struct ChecksFile {
    #[serde(default)]
    check: BTreeMap<String, CheckDef>,
}

#[derive(Clone)]
pub struct Project {
    pub root: PathBuf,
    pub config: Config,
    pub catalog: CheckCatalog,
    pub extractors: ExtractorSet,
    pub candidates: CandidateCatalog,
}

fn read_optional(path: &Path) -> Result<Option<String>, ProjectError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ProjectError::Io(path.display().to_string(), e)),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(path: &Path, src: &str) -> Result<T, ProjectError> {
    toml::from_str(src).map_err(|e| ProjectError::Config(format!("{}: {e}", path.display())))
}

impl Project {
    pub fn dir(&self) -> PathBuf {
        self.root.join(DIR)
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.dir().join("sessions")
    }

    pub fn forms_dir(&self) -> PathBuf {
        self.dir().join("forms")
    }

    pub fn registry_path(&self) -> PathBuf {
        self.dir().join("mutations").join("registry.toml")
    }

    pub fn load(root: &Path) -> Result<Self, ProjectError> {
        let dir = root.join(DIR);
        if !dir.is_dir() {
            return Err(ProjectError::Config(format!(
                "{} not found: run `agon init` first",
                dir.display()
            )));
        }
        let cfg_path = dir.join("config.toml");
        let config: Config = match read_optional(&cfg_path)? {
            Some(src) => parse(&cfg_path, &src)?,
            None => Config::default(),
        };

        let checks_path = dir.join("checks.toml");
        let file: ChecksFile = match read_optional(&checks_path)? {
            Some(src) => parse(&checks_path, &src)?,
            None => ChecksFile {
                check: BTreeMap::new(),
            },
        };
        let catalog: CheckCatalog = file
            .check
            .into_iter()
            .map(|(id, d)| {
                let id = CheckId(id);
                let check = Check {
                    id: id.clone(),
                    command: d.command,
                    dependencies: d.dependencies,
                    success: d.success,
                    outputs: d.outputs,
                    version: d.version,
                };
                check
                    .success
                    .parse::<agon_verify::Condition>()
                    .map_err(|e| ProjectError::Config(format!("check `{id}`: {e}")))?;
                Ok((id, check))
            })
            .collect::<Result<_, ProjectError>>()?;

        let ext_path = dir.join("extractors.toml");
        let extractors = match read_optional(&ext_path)? {
            Some(src) => ExtractorSet::from_toml(&src)
                .map_err(|e| ProjectError::Config(format!("{}: {e}", ext_path.display())))?,
            None => ExtractorSet::default(),
        };

        let cand_path = dir.join("candidates.toml");
        let known = config.capability.keys().cloned();
        let candidates = match read_optional(&cand_path)? {
            Some(src) => CandidateCatalog::from_toml(&src, known)
                .map_err(|e| ProjectError::Config(format!("{}: {e}", cand_path.display())))?,
            None => CandidateCatalog::new(vec![], known),
        };

        for id in &config.form.checks {
            if !catalog.contains_key(&CheckId(id.clone())) {
                return Err(ProjectError::Config(format!(
                    "[form] checks: `{id}` is not defined in checks.toml"
                )));
            }
        }
        Ok(Project {
            root: root.to_path_buf(),
            config,
            catalog,
            extractors,
            candidates,
        })
    }

    pub fn runner(&self) -> ProcessRunner {
        let mut r = ProcessRunner::new(&self.root)
            .with_timeout(Duration::from_secs(self.config.runner.timeout_secs));
        for name in &self.config.runner.env {
            r = r.with_env(name.clone());
        }
        r
    }

    pub fn permissions(&self) -> Permissions {
        self.config.permissions.clone()
    }

    pub fn chat_config(&self) -> ChatConfig {
        let m = &self.config.models;
        ChatConfig {
            base_url: m.base_url.clone(),
            timeout: Duration::from_secs(m.timeout_secs),
            max_retries: m.max_retries,
            ..ChatConfig::default()
        }
    }

    pub fn capability_specs(&self) -> BTreeMap<String, CapabilitySpec> {
        self.config
            .capability
            .iter()
            .map(|(k, c)| {
                (
                    k.clone(),
                    CapabilitySpec {
                        activate: c.activate.clone(),
                        deactivate: c.deactivate.clone(),
                        requires: c.requires.clone(),
                    },
                )
            })
            .collect()
    }

    pub fn jev_config(&self) -> JevConfig {
        let j = &self.config.jev;
        JevConfig {
            base_url: j.base_url.clone(),
            model: j.model.clone(),
            timeout: Duration::from_millis(j.timeout_ms),
            max_retries: j.max_retries,
            ..JevConfig::default()
        }
    }

    /// Instantané des policies : tout ce qui limite ce qu'une mutation peut faire.
    pub fn policy_digest(&self) -> Digest {
        sha256_of(&(
            "policy-v2",
            &self.config.morph.budget,
            &self.config.permissions,
        ))
        .expect("serializable")
    }

    /// Résout le lockfile : version d'Agon, versions des outils (`<tool> --version`),
    /// Jev épinglé, hash des checks et du gabarit de questions.
    pub fn resolve_lock(&self) -> Lockfile {
        let tools = self
            .config
            .form
            .tools
            .iter()
            .map(|t| (t.clone(), tool_version(t)))
            .collect();
        let jev = &self.config.jev;
        let pinned = !jev.model.ends_with("latest");
        Lockfile {
            agon: env!("CARGO_PKG_VERSION").into(),
            morph_schema: "1".into(),
            modules: BTreeMap::new(),
            tools,
            model: None,
            decision: Some(DecisionLock {
                provider: "typesafe".into(),
                model: jev.model.clone(),
                precision: if pinned {
                    Precision::Exact
                } else {
                    Precision::BestEffort
                },
                question_sets: BTreeMap::from([("mutation".to_string(), mutation_template_hash())]),
            }),
            checks: self
                .config
                .form
                .checks
                .iter()
                .map(|id| {
                    (
                        id.clone(),
                        self.catalog[&CheckId(id.clone())].definition_hash(),
                    )
                })
                .collect(),
        }
    }

    /// Form de départ décrite par `[form]`.
    pub fn base_form(&self) -> Form {
        let f = &self.config.form;
        let set = |v: &[String]| v.iter().cloned().collect::<BTreeSet<_>>();
        Form {
            configuration: BTreeMap::new(),
            modules: set(&f.modules),
            tools: set(&f.tools),
            capabilities: set(&f.capabilities),
            model: f.model.clone().unwrap_or_else(|| "medium".into()),
            context: set(&f.context),
            checks: f
                .checks
                .iter()
                .map(|id| {
                    (
                        CheckId(id.clone()),
                        self.catalog[&CheckId(id.clone())].definition_hash(),
                    )
                })
                .collect(),
            policies: self.policy_digest(),
            lock: self.resolve_lock(),
        }
    }
}

fn valid_tool_name(t: &str) -> bool {
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Première ligne de `<tool> --version`. Sans shell : le nom d'outil est validé puis exécuté directement.
pub fn tool_version(tool: &str) -> LockEntry {
    let unresolved = LockEntry {
        version: "unresolved".into(),
        precision: Precision::BestEffort,
    };
    if !valid_tool_name(tool) {
        return unresolved;
    }
    match std::process::Command::new(tool)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(if o.stdout.is_empty() {
                &o.stderr
            } else {
                &o.stdout
            })
            .into_owned();
            match text.lines().next().map(str::trim).filter(|l| !l.is_empty()) {
                Some(line) => LockEntry {
                    version: line.chars().take(120).collect(),
                    precision: Precision::Exact,
                },
                None => unresolved,
            }
        }
        _ => unresolved,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn project_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agon-bridge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(DIR)).unwrap();
        for (f, c) in files {
            std::fs::write(root.join(DIR).join(f), c).unwrap();
        }
        root
    }

    const CHECKS: &str = r#"
[check."verify.build"]
command = "true"
dependencies = ["cargo"]

[check."verify.integration"]
command = "true"
dependencies = ["cargo", "docker", "postgres"]
"#;

    #[test]
    fn missing_agon_dir_points_to_init() {
        let err = Project::load(&std::env::temp_dir().join("agon-definitely-missing"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("agon init"), "{err}");
    }

    #[test]
    fn defaults_apply_when_files_are_absent_or_partial() {
        let root = project_dir(
            "defaults",
            &[("config.toml", "[jev]\nmodel = \"typesafe/jev-1.13\"\n")],
        );
        let p = Project::load(&root).unwrap();
        assert_eq!(p.config.jev.model, "typesafe/jev-1.13");
        assert_eq!(p.config.jev.api_key_env, "TYPESAFE_API_KEY");
        assert_eq!(p.config.morph.budget, Budget::default());
        assert!(p.catalog.is_empty() && p.extractors.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn typos_in_config_are_errors_not_silently_ignored() {
        let root = project_dir("typo", &[("config.toml", "[jev]\nbase_ulr = \"x\"\n")]);
        let err = Project::load(&root).err().unwrap();
        assert!(err.to_string().contains("base_ulr"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn form_checks_must_exist_and_success_conditions_must_parse() {
        let root = project_dir(
            "badform",
            &[
                ("config.toml", "[form]\nchecks = [\"verify.nope\"]\n"),
                ("checks.toml", CHECKS),
            ],
        );
        assert!(
            Project::load(&root)
                .err()
                .unwrap()
                .to_string()
                .contains("verify.nope")
        );

        let bad = CHECKS.replace(
            "dependencies = [\"cargo\"]",
            "success = \"stdout contains ok\"",
        );
        std::fs::write(root.join(DIR).join("config.toml"), "").unwrap();
        std::fs::write(root.join(DIR).join("checks.toml"), bad).unwrap();
        assert!(
            Project::load(&root)
                .err()
                .unwrap()
                .to_string()
                .contains("unsupported success condition")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn base_form_and_lock_reflect_the_configuration() {
        let cfg = "[form]\ntools = [\"cargo\"]\nchecks = [\"verify.build\"]\n[jev]\nmodel = \"jev-1.13.0\"\n";
        let root = project_dir("form", &[("config.toml", cfg), ("checks.toml", CHECKS)]);
        let p = Project::load(&root).unwrap();
        let form = p.base_form();
        assert!(form.tools.contains("cargo") && form.checks.contains_key(&"verify.build".into()));
        assert!(
            !form.checks.contains_key(&"verify.integration".into()),
            "only listed checks are active"
        );

        let lock = &form.lock;
        assert_eq!(lock.tools["cargo"].precision, Precision::Exact);
        assert!(
            lock.tools["cargo"].version.starts_with("cargo"),
            "{:?}",
            lock.tools["cargo"]
        );
        let d = lock.decision.as_ref().unwrap();
        assert_eq!(
            (d.model.as_str(), d.precision),
            ("jev-1.13.0", Precision::Exact)
        );
        assert_eq!(d.question_sets["mutation"], mutation_template_hash());
        assert_eq!(p.base_form().id(), form.id(), "resolution is deterministic");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_alias_model_is_only_best_effort_in_the_lock() {
        let root = project_dir(
            "alias",
            &[("config.toml", "[jev]\nmodel = \"~typesafe/jev-latest\"\n")],
        );
        let lock = Project::load(&root).unwrap().resolve_lock();
        assert_eq!(lock.decision.unwrap().precision, Precision::BestEffort);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_tool_is_reported_as_unresolved_not_a_crash() {
        assert_eq!(
            tool_version("definitely-not-a-tool-xyz").version,
            "unresolved"
        );
        assert_eq!(
            tool_version("rm -rf /").version,
            "unresolved",
            "tool names are never run through a shell"
        );
        assert_eq!(tool_version("").precision, Precision::BestEffort);
    }

    #[test]
    fn permissions_have_safe_defaults_and_are_configurable() {
        let cfg = "[permissions]\nshell_allow = [\"cargo test\"]\nwrite = \"allow\"\n";
        let p = Project::load(&project_dir("permcfg", &[("config.toml", cfg)])).unwrap();
        let perms = p.permissions();
        assert_eq!(perms.write, agon_policy::Level::Allow);
        assert_eq!(
            perms.git_push,
            agon_policy::Level::Deny,
            "unset permissions keep the CDC defaults"
        );
        assert_eq!(perms.shell, agon_policy::Level::Confirm);
    }

    #[test]
    fn permissions_are_part_of_the_policy_snapshot() {
        let a = Project::load(&project_dir("perm-a", &[])).unwrap();
        let b = Project::load(&project_dir(
            "perm-b",
            &[("config.toml", "[permissions]\nwrite = \"allow\"\n")],
        ))
        .unwrap();
        assert_ne!(
            a.policy_digest(),
            b.policy_digest(),
            "relaxing a permission changes the Form identity"
        );
    }
}
