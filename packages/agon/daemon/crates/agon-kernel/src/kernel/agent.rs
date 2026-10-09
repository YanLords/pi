//! Boucle agent (§5) : INPUT → CONTEXT → DECISION → MODEL → TOOL → RESULT → MODEL → VERIFY → COMPLETE.
//!
//! * **Jev décide** du niveau de modèle et retire les outils inutiles ; il n'accorde aucune permission.
//! * **Le modèle génératif agit**, par appels d'outils, chacun soumis aux permissions (§32).
//! * **La vérification établit la vérité** (§2.5) : une tâche n'est jamais « terminée » parce que le
//!   modèle l'affirme, mais parce que les checks de la Form passent. Un échec d'environnement suit le
//!   chemin d'adaptation existant (`repair`) ; un échec de code est renvoyé au modèle.

use super::{HumanReason, Kernel, KernelError, Outcome};
use crate::activation::Activator;
use crate::events::EventKind;
use crate::tools::{
    Confirmer, ESSENTIAL_TOOLS, Guard, TOOL_NAMES, Tools, action_of, describe_tool,
};

/// Outils disponibles en `chat` et en `plan` : ils ne modifient rien et n'exécutent rien.
pub const READ_ONLY_TOOLS: &[&str] = &["fs_read", "fs_list", "fs_search", "git_status", "git_diff"];
use agon_core::CheckId;
use agon_model::jev::gating::{Gate, gate_choice, noul_is_yes};
use agon_model::jev::questions as q;
use agon_model::{ChatRequest, DecisionProvider, Exchange, Message, ModelProvider, request_digest};
use agon_policy::Level;
use agon_verify::CheckRunner;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Small,
    Medium,
    Large,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Small => "small",
            Tier::Medium => "medium",
            Tier::Large => "large",
        }
    }
    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "small" => Some(Tier::Small),
            "medium" => Some(Tier::Medium),
            "large" => Some(Tier::Large),
            _ => None,
        }
    }
}

/// Identifiant de modèle par niveau (§31). Un niveau sans modèle retombe sur le niveau par défaut.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tiers {
    pub small: Option<String>,
    pub medium: Option<String>,
    pub large: Option<String>,
}

impl Tiers {
    fn get(&self, t: Tier) -> Option<&str> {
        match t {
            Tier::Small => &self.small,
            Tier::Medium => &self.medium,
            Tier::Large => &self.large,
        }
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub tiers: Tiers,
    pub default_tier: Tier,
    /// Appels au modèle par round.
    pub max_steps: u32,
    /// Rounds « agir puis vérifier » avant de rendre la main.
    pub max_rounds: u32,
    pub max_tokens: Option<u32>,
    /// Demander à Jev le niveau de modèle et les outils pertinents.
    pub jev_preflight: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            tiers: Tiers::default(),
            default_tier: Tier::Medium,
            max_steps: 25,
            max_rounds: 3,
            max_tokens: None,
            jev_preflight: true,
        }
    }
}

pub struct Agent<M, C> {
    pub model: M,
    pub tools: Tools,
    pub confirmer: C,
    pub config: AgentConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum TaskReason {
    /// Les checks échouent toujours après tous les rounds.
    VerificationFailed {
        checks: Vec<String>,
    },
    StepBudget {
        steps: u32,
    },
    /// Un fichier de politique de `.agon/` a été modifié pendant la tâche.
    Tampering {
        files: Vec<String>,
    },
    ModelError {
        detail: String,
    },
    /// Le humain a interrompu le tour (Échap).
    Cancelled,
    /// Problème d'environnement que l'adaptation n'a pas su traiter (budget, Jev, activation…).
    Environment {
        why: HumanReason,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TaskOutcome {
    /// Le modèle a terminé **et** tous les checks de la Form passent.
    Completed {
        summary: String,
        checks: Vec<CheckId>,
    },
    /// Le modèle a terminé, mais la Form n'a aucun check : rien ne prouve le résultat (§2.5).
    Unverified {
        summary: String,
    },
    HumanRequired {
        why: TaskReason,
    },
}

enum Turn {
    Finished(String),
    StepBudget,
    Tampering(Vec<String>),
    ModelError(String),
    Cancelled,
}

/// Jeton d'annulation : l'interface le lève, le Kernel le consulte entre deux étapes et pendant
/// l'attente du modèle. Une action déjà lancée n'est pas laissée à moitié : les outils en cours
/// sont tués par l'interface (`kill_active_processes`), puis la boucle s'arrête proprement.
#[derive(Clone, Default)]
pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn reset(&self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
    async fn cancelled(&self) {
        while !self.is_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

/// Mode d'interaction (à la manière des agents en terminal) :
///
/// * `Chat` : on discute ; le modèle explore le code avec des outils en **lecture seule** ;
/// * `Plan` : idem, et le modèle propose un plan (`# Plan`) qu'on approuve ensuite ;
/// * `Code` : le modèle **agit** ; toute l'architecture s'enclenche (Jev, permissions, confirmations,
///   vérification par les checks, adaptation de la Form).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Chat,
    Plan,
    Code,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Chat => "chat",
            Mode::Plan => "plan",
            Mode::Code => "code",
        }
    }

    /// chat → plan → code → chat
    pub fn next(self) -> Mode {
        match self {
            Mode::Chat => Mode::Plan,
            Mode::Plan => Mode::Code,
            Mode::Code => Mode::Chat,
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "chat" => Some(Mode::Chat),
            "plan" => Some(Mode::Plan),
            "code" => Some(Mode::Code),
            _ => None,
        }
    }
}

/// Au-delà, on refuse d'envoyer la conversation au modèle : elle ne tiendrait pas dans son contexte.
pub const MAX_CONTEXT_CHARS: usize = 500_000;

/// Historique d'une conversation avec le modèle, conservé d'un message à l'autre (et d'un mode à
/// l'autre : un plan discuté en `plan` est encore dans le contexte quand on passe en `code`).
/// Le premier message est toujours le message système du mode courant, remplacé et jamais dupliqué.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstruit une conversation à partir d'un journal : les messages de l'utilisateur et le
    /// texte du modèle. Les appels d'outils et leurs résultats ne sont pas rejoués (le journal n'en
    /// garde qu'un aperçu) : le modèle reprend sur ce qui a été *dit*, et relit le code au besoin.
    pub fn from_events(events: &[crate::Event]) -> Self {
        use crate::EventKind;
        let mut c = Conversation::new();
        for e in events {
            match &e.kind {
                EventKind::ChatUser { text, .. } => c.messages.push(Message::user(text.clone())),
                EventKind::AgentStarted { prompt } => {
                    c.messages.push(Message::user(prompt.clone()))
                }
                EventKind::AgentMessage { text, .. } if !text.trim().is_empty() => {
                    c.messages.push(Message::assistant(text.clone()))
                }
                _ => {}
            }
        }
        c
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Taille approximative (caractères) : sert à refuser une conversation qui déborderait du contexte.
    pub fn approx_chars(&self) -> usize {
        self.messages
            .iter()
            .map(|m| match m {
                Message::System { content }
                | Message::User { content }
                | Message::Tool { content, .. } => content.len(),
                Message::Assistant {
                    content,
                    tool_calls,
                } => {
                    content.as_deref().map_or(0, str::len)
                        + tool_calls
                            .iter()
                            .map(|c| c.function.arguments.len() + c.function.name.len())
                            .sum::<usize>()
                }
            })
            .sum()
    }

    fn set_system(&mut self, text: String) {
        match self.messages.first_mut() {
            Some(Message::System { content }) => *content = text,
            _ => self.messages.insert(0, Message::system(text)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChatOutcome {
    /// Réponse du modèle.
    Answer(String),
    /// Réponse qui est un plan complet (mode `plan`).
    Plan(String),
    /// Le tour s'est arrêté (budget d'appels, erreur du modèle, conversation trop longue).
    Stopped(TaskReason),
}

/// Un plan se reconnaît à un titre `# Plan` (niveaux 1 à 3) : une simple question de clarification n'en est pas un.
pub fn looks_like_plan(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        let hashes = t.chars().take_while(|c| *c == '#').count();
        let rest = &t[hashes..];
        // Un titre Markdown exige une espace après les `#` : `#planning` est un hashtag, pas un titre.
        (1..=3).contains(&hashes)
            && rest.starts_with(char::is_whitespace)
            && rest.trim_start().to_lowercase().starts_with("plan")
    })
}

fn project_name(root: &std::path::Path) -> &str {
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
}

fn checks_listing(checks: &[(CheckId, String)]) -> String {
    if checks.is_empty() {
        return "This project defines no automated check.\n".into();
    }
    let mut s =
        String::from("Project checks (they will judge the result once the code is written):\n");
    for (id, cmd) in checks {
        s.push_str(&format!("- {id}: `{cmd}`\n"));
    }
    s
}

fn chat_prompt(root: &std::path::Path, mode: Mode, checks: &[(CheckId, String)]) -> String {
    let mut p = format!(
        "You are Agon, a software engineering assistant working in the project `{}`.\n\
         You are talking with the developer. You can read the project with read-only tools (read, list, search, git status/diff) \
         to answer accurately; you can NOT modify files or run commands in this mode. Paths are relative to the project root; \
         secrets (.env, keys) and Agon's `.agon/` directory are off limits. Be concise and concrete.\n",
        project_name(root)
    );
    match mode {
        Mode::Plan => p.push_str(
            "\nYou are in PLAN mode. Explore the code, ask a short clarifying question if something essential is missing, \
             then write a concrete plan. A finished plan must start with a heading `# Plan` and contain: Goal; Steps (ordered, \
             each naming the files it touches); How it will be verified (which project checks, or new tests to add); Risks and \
             assumptions. Do not implement anything: the developer will approve the plan with /go, and only then will code be written.\n",
        ),
        _ => p.push_str(
            "\nIf the developer wants changes made, tell them they can design them first with /plan, or start right away with /code.\n",
        ),
    }
    p.push_str(&checks_listing(checks));
    p
}

fn head(s: &str, n: usize) -> String {
    let flat: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{flat}…")
    } else {
        flat
    }
}

fn system_prompt(root: &std::path::Path, checks: &[(CheckId, String)]) -> String {
    let mut p = format!(
        "You are Agon, a careful software engineering agent working in the project `{}`.\n\
         Use the tools to inspect the code before changing it, make the smallest change that solves the task, \
         and never claim success without evidence.\n\
         Rules:\n\
         - Paths are relative to the project root. Secrets (.env, keys) and Agon's own `.agon/` directory are off limits.\n\
         - Some tools need the user's confirmation; if an action is denied, do not retry it — pick another approach or say what you need.\n\
         - Pushing to a remote is impossible.\n\
         - If a plan was agreed earlier in this conversation, follow it.\n\
         - When you believe the task is done, reply with a short summary and no tool call. Agon then runs the project's checks itself; if they fail you will be told why.\n",
        root.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
    );
    if checks.is_empty() {
        p.push_str("This project defines no automated check, so your result cannot be verified: say so plainly.\n");
    } else {
        p.push_str("Project checks (run by Agon after you finish; you may run them yourself with the shell tool):\n");
        for (id, cmd) in checks {
            p.push_str(&format!("- {id}: `{cmd}`\n"));
        }
    }
    p
}

impl<D: DecisionProvider, R: CheckRunner, A: Activator> Kernel<D, R, A> {
    /// Réalise `prompt` avec l'agent, puis vérifie, dans une conversation neuve.
    pub async fn run_task<M: ModelProvider, C: Confirmer>(
        &mut self,
        agent: &Agent<M, C>,
        prompt: &str,
    ) -> Result<TaskOutcome, KernelError> {
        self.run_task_in(agent, &mut Conversation::new(), prompt)
            .await
    }

    /// Réalise `prompt` (mode `code`) dans `conversation` : le modèle voit ce qui a été discuté avant,
    /// par exemple le plan approuvé. Toute l'architecture s'enclenche : Jev choisit le niveau de modèle
    /// et les outils, chaque action passe par les permissions, puis les checks tranchent.
    pub async fn run_task_in<M: ModelProvider, C: Confirmer>(
        &mut self,
        agent: &Agent<M, C>,
        conversation: &mut Conversation,
        prompt: &str,
    ) -> Result<TaskOutcome, KernelError> {
        self.emit(EventKind::AgentStarted {
            prompt: head(prompt, 2000),
        })?;
        let guard = Guard::snapshot(agent.tools.root());
        let outcome = self.task(agent, conversation, prompt, &guard).await?;
        self.emit(EventKind::AgentCompleted {
            outcome: outcome.clone(),
        })?;
        Ok(outcome)
    }

    /// Un message de l'utilisateur en mode `chat` ou `plan` : le modèle répond, en explorant le projet
    /// avec des outils **en lecture seule**. Rien n'est écrit, aucune commande n'est lancée.
    pub async fn chat_turn<M: ModelProvider, C: Confirmer>(
        &mut self,
        agent: &Agent<M, C>,
        conversation: &mut Conversation,
        mode: Mode,
        text: &str,
    ) -> Result<ChatOutcome, KernelError> {
        let mode = if mode == Mode::Code { Mode::Chat } else { mode };
        self.emit(EventKind::ChatUser {
            mode,
            text: head(text, 4000),
        })?;
        let stopped = |detail: String| Ok(ChatOutcome::Stopped(TaskReason::ModelError { detail }));

        let Some(model_id) = agent
            .config
            .tiers
            .get(agent.config.default_tier)
            .or_else(|| agent.config.tiers.get(Tier::Medium))
            .or_else(|| agent.config.tiers.get(Tier::Large))
            .or_else(|| agent.config.tiers.get(Tier::Small))
            .map(str::to_owned)
        else {
            return stopped(
                "no model is configured (set [models] in .agon/config.toml, or use /model)".into(),
            );
        };

        let listed = self.listed_checks();
        conversation.set_system(chat_prompt(agent.tools.root(), mode, &listed));
        conversation.messages.push(Message::user(text));
        if conversation.approx_chars() > MAX_CONTEXT_CHARS {
            conversation.messages.pop();
            return stopped(format!(
                "the conversation is too long ({} characters): start a new one with /new",
                conversation.approx_chars()
            ));
        }

        // Lecture seule : les outils qui écrivent ou exécutent ne sont ni proposés, ni appelables.
        let perms = agent.tools.permissions();
        let enabled: BTreeSet<String> = READ_ONLY_TOOLS
            .iter()
            .filter(|t| action_of(t).is_none_or(|a| perms.level(a) != Level::Deny))
            .map(|t| t.to_string())
            .collect();
        let specs = agent.tools.specs(Some(&enabled));
        let guard = Guard::snapshot(agent.tools.root());
        match self
            .turn(
                agent,
                &mut conversation.messages,
                &specs,
                &enabled,
                &model_id,
                &guard,
            )
            .await?
        {
            Turn::Finished(answer) if mode == Mode::Plan && looks_like_plan(&answer) => {
                self.emit(EventKind::PlanProposed {
                    plan: head(&answer, 20_000),
                })?;
                Ok(ChatOutcome::Plan(answer))
            }
            Turn::Finished(answer) => Ok(ChatOutcome::Answer(answer)),
            Turn::StepBudget => Ok(ChatOutcome::Stopped(TaskReason::StepBudget {
                steps: agent.config.max_steps,
            })),
            Turn::Tampering(files) => Ok(ChatOutcome::Stopped(TaskReason::Tampering { files })),
            Turn::ModelError(detail) => Ok(ChatOutcome::Stopped(TaskReason::ModelError { detail })),
            Turn::Cancelled => Ok(ChatOutcome::Stopped(TaskReason::Cancelled)),
        }
    }

    fn listed_checks(&self) -> Vec<(CheckId, String)> {
        self.history
            .current()
            .checks
            .keys()
            .filter_map(|id| {
                self.catalog
                    .get(id)
                    .map(|c| (id.clone(), c.command.clone()))
            })
            .collect()
    }

    async fn task<M: ModelProvider, C: Confirmer>(
        &mut self,
        agent: &Agent<M, C>,
        conversation: &mut Conversation,
        prompt: &str,
        guard: &Guard,
    ) -> Result<TaskOutcome, KernelError> {
        let (tier, enabled, source) = self.configure(agent, prompt).await?;
        let Some(model_id) = agent
            .config
            .tiers
            .get(tier)
            .or_else(|| agent.config.tiers.get(agent.config.default_tier))
            .map(str::to_owned)
        else {
            return Ok(TaskOutcome::HumanRequired {
                why: TaskReason::ModelError {
                    detail: format!(
                        "no model is configured for tier `{}` (set [models] in .agon/config.toml)",
                        tier.name()
                    ),
                },
            });
        };
        let specs = agent.tools.specs(Some(&enabled));
        self.emit(EventKind::AgentConfigured {
            tier: tier.name().into(),
            model: model_id.clone(),
            tools: specs.iter().map(|s| s.name.clone()).collect(),
            source: source.into(),
        })?;

        let listed = self.listed_checks();
        conversation.set_system(system_prompt(agent.tools.root(), &listed));
        conversation.messages.push(Message::user(prompt));
        if conversation.approx_chars() > MAX_CONTEXT_CHARS {
            conversation.messages.pop();
            return Ok(TaskOutcome::HumanRequired {
                why: TaskReason::ModelError {
                    detail: format!(
                        "the conversation is too long ({} characters): start a new one with /new",
                        conversation.approx_chars()
                    ),
                },
            });
        }

        for round in 1..=agent.config.max_rounds.max(1) {
            let summary = match self
                .turn(
                    agent,
                    &mut conversation.messages,
                    &specs,
                    &enabled,
                    &model_id,
                    guard,
                )
                .await?
            {
                Turn::Finished(text) => text,
                Turn::StepBudget => {
                    return Ok(TaskOutcome::HumanRequired {
                        why: TaskReason::StepBudget {
                            steps: agent.config.max_steps,
                        },
                    });
                }
                Turn::Tampering(files) => {
                    return Ok(TaskOutcome::HumanRequired {
                        why: TaskReason::Tampering { files },
                    });
                }
                Turn::ModelError(detail) => {
                    return Ok(TaskOutcome::HumanRequired {
                        why: TaskReason::ModelError { detail },
                    });
                }
                Turn::Cancelled => {
                    return Ok(TaskOutcome::HumanRequired {
                        why: TaskReason::Cancelled,
                    });
                }
            };

            let checks: Vec<CheckId> = self.history.current().checks.keys().cloned().collect();
            if checks.is_empty() {
                return Ok(TaskOutcome::Unverified { summary });
            }
            let mut failing: Vec<CheckId> = Vec::new();
            for id in &checks {
                match self.repair(id).await? {
                    Outcome::Passed | Outcome::Flaky | Outcome::Fixed { .. } => {}
                    // Aucune adaptation de la Form n'aide : c'est un problème de code, à renvoyer au modèle.
                    Outcome::NoChange
                    | Outcome::HumanRequired {
                        why: HumanReason::NoCandidate | HumanReason::JevRequestedHuman,
                    } => failing.push(id.clone()),
                    Outcome::HumanRequired { why } => {
                        return Ok(TaskOutcome::HumanRequired {
                            why: TaskReason::Environment { why },
                        });
                    }
                }
            }
            if failing.is_empty() {
                return Ok(TaskOutcome::Completed { summary, checks });
            }
            if round == agent.config.max_rounds.max(1) {
                return Ok(TaskOutcome::HumanRequired {
                    why: TaskReason::VerificationFailed {
                        checks: failing.into_iter().map(|c| c.0).collect(),
                    },
                });
            }
            conversation
                .messages
                .push(Message::user(self.feedback(&failing)));
        }
        unreachable!("the last round always returns")
    }

    /// Message de retour au modèle : ce qui a échoué, avec la fin de la sortie (là où sont les erreurs).
    fn feedback(&self, failing: &[CheckId]) -> String {
        let mut out = String::from(
            "Verification failed. Agon ran the project's checks and these still fail:\n",
        );
        for id in failing {
            let command = self
                .catalog
                .get(id)
                .map(|c| c.command.as_str())
                .unwrap_or("?");
            out.push_str(&format!("\n- check `{id}` (`{command}`)"));
            let evidence = self
                .session
                .events()
                .iter()
                .rev()
                .find_map(|e| match &e.kind {
                    EventKind::VerificationFailed { evidence, .. } if &evidence.check == id => {
                        Some(evidence.clone())
                    }
                    _ => None,
                });
            if let Some(ev) = evidence {
                let o = &ev.observation;
                out.push_str(&format!(
                    ", {}",
                    o.exit_code
                        .map(|c| format!("exit code {c}"))
                        .unwrap_or_else(|| "killed".into())
                ));
                for (label, text) in [("stderr", &o.stderr), ("stdout", &o.stdout)] {
                    let text = text.trim();
                    if !text.is_empty() {
                        let n = text.chars().count();
                        let tail: String = text.chars().skip(n.saturating_sub(1500)).collect();
                        out.push_str(&format!("\n  {label} (end):\n{tail}"));
                    }
                }
            }
        }
        out.push_str("\n\nFix the cause, then reply with a short summary when done.");
        out
    }

    /// Jev choisit le niveau de modèle et retire les outils inutiles (§6.3). Repli sûr si Jev échoue.
    async fn configure<M: ModelProvider, C: Confirmer>(
        &mut self,
        agent: &Agent<M, C>,
        prompt: &str,
    ) -> Result<(Tier, BTreeSet<String>, &'static str), KernelError> {
        let perms = agent.tools.permissions();
        // Un outil dont l'action est interdite n'est même pas proposé.
        let offered: Vec<&str> = TOOL_NAMES
            .iter()
            .copied()
            .filter(|t| action_of(t).is_none_or(|a| perms.level(a) != Level::Deny))
            .collect();
        let all: BTreeSet<String> = offered.iter().map(|t| t.to_string()).collect();
        let fallback = (agent.config.default_tier, all.clone(), "default");
        if !agent.config.jev_preflight {
            return Ok(fallback);
        }

        let optional: Vec<&str> = offered
            .iter()
            .copied()
            .filter(|t| !ESSENTIAL_TOOLS.contains(t))
            .collect();
        let mut questions = BTreeMap::new();
        questions.insert(q::MODEL_TIER_KEY.to_string(), q::model_tier_question());
        for t in &optional {
            let (k, question) = q::tool_question(t, describe_tool(t));
            questions.insert(k, question);
        }
        let form = self.history.current();
        let state = json!({
            "task": head(prompt, 4000),
            "checks": form.checks.keys().map(|c| &c.0).collect::<Vec<_>>(),
            "capabilities": form.capabilities,
        });

        self.decision_no += 1;
        let decision_id = format!("d-{:04}", self.decision_no);
        let request = request_digest(&state, &questions);
        self.emit(EventKind::DecisionRequested {
            decision_id: decision_id.clone(),
            template: q::template_hash(&questions, &[]),
            request: request.clone(),
            candidates: questions.keys().cloned().collect(),
        })?;

        let response = match self.decisions.decide(state, questions).await {
            Ok(r) => r,
            Err(e) => {
                self.emit(EventKind::DecisionUnavailable {
                    decision_id,
                    detail: e.to_string(),
                })?;
                return Ok(fallback);
            }
        };

        // Confidence gating (§6.4) : sous le seuil, le niveau par défaut s'applique.
        let th = self.cfg.thresholds;
        let tier = match response
            .answers
            .get(q::MODEL_TIER_KEY)
            .map(|a| gate_choice(a, th.choice_min_confidence))
        {
            Some(Ok(Gate::Pass(t))) => Tier::parse(&t).unwrap_or(agent.config.default_tier),
            _ => agent.config.default_tier,
        };
        let mut enabled: BTreeSet<String> = ESSENTIAL_TOOLS
            .iter()
            .filter(|t| all.contains(**t))
            .map(|t| t.to_string())
            .collect();
        for t in &optional {
            if response
                .answers
                .get(&format!("tool.{t}"))
                .is_some_and(|a| noul_is_yes(a, th.noul_yes).unwrap_or(false))
            {
                enabled.insert(t.to_string());
            }
        }

        let (requested, answered) = (self.cfg.jev_model.clone(), response.model.clone());
        self.emit(EventKind::DecisionCompleted {
            decision_id,
            model_drift: !crate::state::model_matches(&requested, &answered),
            jev_model_requested: requested,
            jev_model_answered: answered,
            exchange: Box::new(Exchange { request, response }),
            outcome: format!(
                "tier {}, tools [{}]",
                tier.name(),
                enabled.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        })?;
        Ok((tier, enabled, "jev"))
    }

    /// Un round : le modèle et les outils dialoguent jusqu'à ce que le modèle réponde sans appel d'outil.
    async fn turn<M: ModelProvider, C: Confirmer>(
        &mut self,
        agent: &Agent<M, C>,
        messages: &mut Vec<Message>,
        specs: &[agon_model::ToolSpec],
        enabled: &BTreeSet<String>,
        model_id: &str,
        guard: &Guard,
    ) -> Result<Turn, KernelError> {
        for _ in 0..agent.config.max_steps {
            let request = ChatRequest {
                model: model_id.to_string(),
                messages: messages.clone(),
                tools: specs.to_vec(),
                max_tokens: agent.config.max_tokens,
                temperature: None,
            };
            let delta = self.delta.clone();
            let mut forward = move |d: &str| {
                if let Some(f) = &delta {
                    f(d)
                }
            };
            if self.cancel.is_cancelled() {
                return Ok(Turn::Cancelled);
            }
            let cancel = self.cancel.clone();
            let response = tokio::select! {
                r = agent.model.chat_stream(request, &mut forward) => match r {
                    Ok(r) => r,
                    Err(e) => return Ok(Turn::ModelError(e.to_string())),
                },
                _ = cancel.cancelled() => return Ok(Turn::Cancelled),
            };
            let calls = response.tool_calls().to_vec();
            self.emit(EventKind::AgentMessage {
                text: head(response.text(), 8000),
                tool_calls: calls.iter().map(|c| c.function.name.clone()).collect(),
                model: response.model.clone(),
                prompt_tokens: response.usage.prompt_tokens,
                completion_tokens: response.usage.completion_tokens,
                cost: response.usage.cost,
            })?;
            messages.push(response.message.clone());

            if calls.is_empty() {
                if response.finish_reason.as_deref() == Some("length") {
                    messages.push(Message::user(
                        "Your reply was cut off. Continue, and finish with a short summary.",
                    ));
                    continue;
                }
                return Ok(Turn::Finished(response.text().to_string()));
            }

            for (i, call) in calls.iter().enumerate() {
                if self.cancel.is_cancelled() {
                    // Chaque appel d'outil doit recevoir une réponse pour que la conversation
                    // reste valide à la reprise.
                    for rest in &calls[i..] {
                        messages.push(Message::tool(
                            rest.id.clone(),
                            "cancelled by the user before this tool ran",
                        ));
                    }
                    return Ok(Turn::Cancelled);
                }
                let outcome = if enabled.contains(&call.function.name) {
                    agent.tools.execute(call, &agent.confirmer)
                } else {
                    crate::tools::ToolOutcome {
                        tool: call.function.name.clone(),
                        detail: String::new(),
                        authorization: crate::tools::Authorization::Invalid {
                            reason: "tool not available".into(),
                        },
                        ok: false,
                        content: format!(
                            "error: tool `{}` is not available for this task; available: {}",
                            call.function.name,
                            enabled.iter().cloned().collect::<Vec<_>>().join(", ")
                        ),
                    }
                };
                self.emit(EventKind::ToolCalled {
                    tool: outcome.tool.clone(),
                    detail: head(&outcome.detail, 300),
                    authorization: outcome.authorization.clone(),
                    ok: outcome.ok,
                    preview: head(outcome.content.lines().next().unwrap_or(""), 160),
                })?;
                messages.push(Message::tool(call.id.clone(), outcome.content));

                // Une action, même approuvée, ne doit pas réécrire les règles qui la gouvernent.
                let touched = guard.violations();
                if !touched.is_empty() {
                    return Ok(Turn::Tampering(touched));
                }
            }
        }
        Ok(Turn::StepBudget)
    }
}
