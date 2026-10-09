//! La boucle agent de bout en bout : faux modèle scripté, faux Jev, runner scripté, vrais outils.

use agon_core::fixtures::{base_form, catalog, integration_check};
use agon_core::{Check, Form};
use agon_kernel::tools::{AutoApprove, ConfirmRequest, Confirmation, Confirmer, DenyAll, Tools};
use agon_kernel::{
    Agent, AgentConfig, CandidateCatalog, EventKind, HumanReason, Kernel, KernelConfig,
    KernelParts, NoopActivator, ProcessRunner, Session, TaskOutcome, TaskReason, Tiers,
};
use agon_model::chat::ChatUsage;
use agon_model::jev::{Answer, JevError, Question, Response, Usage};
use agon_model::{
    ChatRequest, ChatResponse, DecisionProvider, Message, ModelError, ModelProvider, ToolCall,
};
use agon_morph::Registry;
use agon_policy::{Level, Permissions};
use agon_verify::{CheckRunner, ExtractorSet, Observation};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// ── faux modèle génératif ──────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct ScriptedModel {
    replies: Arc<Mutex<VecDeque<Result<ChatResponse, ModelError>>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ScriptedModel {
    fn new(replies: Vec<Result<ChatResponse, ModelError>>) -> Self {
        ScriptedModel {
            replies: Arc::new(Mutex::new(replies.into())),
            requests: Arc::default(),
        }
    }
    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelProvider for ScriptedModel {
    fn chat(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = Result<ChatResponse, ModelError>> + Send {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(ModelError::InvalidResponse("script exhausted".into())));
        async move { reply }
    }
}

fn response(
    content: Option<&str>,
    calls: Vec<ToolCall>,
    finish: &str,
) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse {
        message: Message::Assistant {
            content: content.map(str::to_owned),
            tool_calls: calls,
        },
        finish_reason: Some(finish.into()),
        model: "vendor/answered".into(),
        usage: ChatUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            cost: Some(0.00001),
        },
    })
}

fn say(text: &str) -> Result<ChatResponse, ModelError> {
    response(Some(text), vec![], "stop")
}

fn use_tools(calls: &[(&str, Value)]) -> Result<ChatResponse, ModelError> {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| ToolCall::new(format!("call_{i}"), *name, args.to_string()))
        .collect();
    response(None, calls, "tool_calls")
}

// ── faux Jev ───────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct FakeJev {
    tier: &'static str,
    confidence: f64,
    tools_yes: Vec<&'static str>,
    mutation_pick: Option<&'static str>,
    down: bool,
    calls: Arc<AtomicU32>,
}

impl FakeJev {
    fn new(tier: &'static str) -> Self {
        FakeJev {
            tier,
            confidence: 0.9,
            tools_yes: vec![],
            mutation_pick: None,
            down: false,
            calls: Arc::default(),
        }
    }
    fn with_tools(tier: &'static str, tools: &[&'static str]) -> Self {
        FakeJev {
            tools_yes: tools.to_vec(),
            ..Self::new(tier)
        }
    }
    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
    fn respond(&self, questions: &BTreeMap<String, Question>) -> Result<Response, JevError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.down {
            return Err(JevError::Unavailable {
                attempts: 4,
                last: "http 503".into(),
            });
        }
        let mut answers = BTreeMap::new();
        for (key, q) in questions {
            let answer = match (key.as_str(), q) {
                ("model_tier", _) => Answer::Choice {
                    choice: self.tier.into(),
                    probabilities: BTreeMap::from([(self.tier.to_string(), 1.0)]),
                    confidence: self.confidence,
                },
                ("mutation", Question::Choice { criteria, .. }) => {
                    let choice = self
                        .mutation_pick
                        .and_then(|p| {
                            criteria.iter().find(|(_, d)| {
                                d.as_ref()
                                    .and_then(|v| v.as_str())
                                    .is_some_and(|t| t.contains(p))
                            })
                        })
                        .map(|(k, _)| k.clone())
                        .unwrap_or_else(|| "none".into());
                    Answer::Choice {
                        choice: choice.clone(),
                        probabilities: BTreeMap::from([(choice, 1.0)]),
                        confidence: 0.9,
                    }
                }
                (k, _) if k.starts_with("tool.") => {
                    let name = &k["tool.".len()..];
                    Answer::Noul {
                        noul: if self.tools_yes.contains(&name) {
                            0.9
                        } else {
                            0.1
                        },
                    }
                }
                _ => panic!("unexpected question `{key}`"),
            };
            answers.insert(key.clone(), answer);
        }
        Ok(Response {
            id: None,
            model: "jev-1.13.0".into(),
            provider: None,
            answers,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cost: None,
            },
        })
    }
}

impl DecisionProvider for FakeJev {
    fn decide(
        &self,
        _state: Value,
        questions: BTreeMap<String, Question>,
    ) -> impl Future<Output = Result<Response, JevError>> + Send {
        let r = self.respond(&questions);
        async move { r }
    }
}

// ── runner scripté ─────────────────────────────────────────────────────────────────────────

struct Script(Mutex<(Vec<i32>, usize)>);

fn script(codes: &[i32]) -> Script {
    Script(Mutex::new((codes.to_vec(), 0)))
}

impl CheckRunner for Script {
    fn run(&self, check: &Check) -> Observation {
        let mut g = self.0.lock().unwrap();
        let code = g.0[g.1.min(g.0.len() - 1)];
        g.1 += 1;
        Observation {
            check: check.id.clone(),
            exit_code: Some(code),
            stdout: String::new(),
            stderr: if code != 0 {
                "ECONNREFUSED 127.0.0.1:5432".into()
            } else {
                String::new()
            },
            timed_out: false,
        }
    }
}

// ── montage ────────────────────────────────────────────────────────────────────────────────

fn project(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agon-agent-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".agon")).unwrap();
    std::fs::write(d.join(".agon/config.toml"), "# policy\n").unwrap();
    d.canonicalize().unwrap()
}

fn tiers() -> Tiers {
    Tiers {
        small: Some("m/small".into()),
        medium: Some("m/medium".into()),
        large: Some("m/large".into()),
    }
}

fn cfg() -> AgentConfig {
    AgentConfig {
        tiers: tiers(),
        ..AgentConfig::default()
    }
}

fn no_checks() -> Form {
    let mut f = base_form();
    f.checks.clear();
    f
}

fn integration_failing_form() -> Form {
    let mut f = base_form();
    f.checks.clear();
    f.checks.insert(
        integration_check().id,
        integration_check().definition_hash(),
    );
    f
}

const PG_EXTRACTOR: &str = r#"
[extractor.pg]
check = "verify.integration"
class = "connection_refused"
priority = 100
[[extractor.pg.match]]
source = "exit_code"
operator = "not_equals"
value = 0
[[extractor.pg.match]]
source = "stderr"
regex = 'ECONNREFUSED [0-9.]+:(?P<port>\d+)'
[extractor.pg.fields]
port = { from = "port", type = "integer" }
service = { lookup = "port", table = { "5432" = "postgres" } }
"#;

type TestKernel = Kernel<FakeJev, Script, NoopActivator>;

fn kernel(
    jev: FakeJev,
    codes: &[i32],
    form: Form,
    candidates: CandidateCatalog,
    session: Session,
) -> TestKernel {
    Kernel::new(KernelParts {
        decisions: jev,
        runner: script(codes),
        activator: NoopActivator,
        config: KernelConfig::default(),
        catalog: catalog(),
        extractors: ExtractorSet::from_toml(PG_EXTRACTOR).unwrap(),
        candidates,
        registry: Registry::new(),
        form,
        session,
    })
    .unwrap()
}

fn agent<C: Confirmer>(
    model: ScriptedModel,
    root: &Path,
    perms: Permissions,
    confirmer: C,
    config: AgentConfig,
) -> Agent<ScriptedModel, C> {
    Agent {
        model,
        tools: Tools::new(root, perms, ProcessRunner::new(root)).unwrap(),
        confirmer,
        config,
    }
}

fn kinds(k: &TestKernel) -> Vec<String> {
    k.session().kinds()
}

fn assert_in_order(kinds: &[String], expected: &[&str]) {
    let mut it = kinds.iter();
    for e in expected {
        assert!(
            it.any(|k| k == e),
            "expected `{e}` (in this order) in {kinds:?}"
        );
    }
}

fn tool_names(req: &ChatRequest) -> Vec<String> {
    req.tools.iter().map(|t| t.name.clone()).collect()
}

fn last_tool_message(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find_map(|m| {
            if let Message::Tool { content, .. } = m {
                Some(content.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

// ── scénarios ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_task_is_completed_only_once_the_checks_pass() {
    let root = project("complete");
    let dir = std::env::temp_dir().join(format!("agon-agent-journal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let model = ScriptedModel::new(vec![
        use_tools(&[("fs_write", json!({"path": "out.txt", "content": "ok"}))]),
        say("Created out.txt."),
    ]);
    let jev = FakeJev::with_tools("medium", &["fs_write"]);
    let mut k = kernel(
        jev.clone(),
        &[0],
        base_form(),
        CandidateCatalog::default(),
        Session::create(&dir, "t1").unwrap(),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        cfg(),
    );

    let out = k.run_task(&a, "create out.txt").await.unwrap();
    let TaskOutcome::Completed { summary, checks } = &out else {
        panic!("{out:?}")
    };
    assert_eq!((summary.as_str(), checks.len()), ("Created out.txt.", 1));
    assert_eq!(std::fs::read_to_string(root.join("out.txt")).unwrap(), "ok");
    assert_eq!(jev.calls(), 1, "one preflight call");

    assert_in_order(
        &kinds(&k),
        &[
            "agent.started",
            "decision.requested",
            "decision.completed",
            "agent.configured",
            "agent.message",
            "tool.called",
            "agent.message",
            "verification.passed",
            "task.completed",
            "agent.completed",
        ],
    );
    // Le journal sur disque se relit avec les nouveaux types d'événements.
    let path = k.session().path().unwrap().to_path_buf();
    assert_eq!(Session::load(&path).unwrap().kinds(), kinds(&k));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn failing_checks_are_fed_back_and_the_agent_gets_another_round() {
    let root = project("feedback");
    let model = ScriptedModel::new(vec![
        say("I believe it is done."),
        use_tools(&[("fs_write", json!({"path": "fix.txt", "content": "x"}))]),
        say("Fixed."),
    ]);
    // round 1 : FAIL, recheck FAIL (reproductible, aucune adaptation possible) ; round 2 : PASS.
    let mut k = kernel(
        FakeJev::with_tools("medium", &["fs_write"]),
        &[1, 1, 0],
        base_form(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        cfg(),
    );

    let out = k.run_task(&a, "make it pass").await.unwrap();
    assert!(
        matches!(&out, TaskOutcome::Completed { summary, .. } if summary == "Fixed."),
        "{out:?}"
    );

    let reqs = model.requests();
    assert_eq!(reqs.len(), 3);
    let Message::User { content } = reqs[1].messages.last().unwrap() else {
        panic!("feedback must be a user message")
    };
    for expected in [
        "Verification failed",
        "verify.build",
        "exit code 1",
        "ECONNREFUSED",
    ] {
        assert!(
            content.contains(expected),
            "feedback lacks `{expected}`:\n{content}"
        );
    }
    assert!(std::fs::read_to_string(root.join("fix.txt")).is_ok());
}

#[tokio::test]
async fn a_claim_of_success_is_never_enough() {
    let root = project("nevertrue");
    let model = ScriptedModel::new(vec![say("Done, all good."), say("Really done.")]);
    let mut k = kernel(
        FakeJev::new("medium"),
        &[1],
        base_form(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        AgentConfig {
            max_rounds: 2,
            ..cfg()
        },
    );

    let out = k.run_task(&a, "task").await.unwrap();
    assert_eq!(
        out,
        TaskOutcome::HumanRequired {
            why: TaskReason::VerificationFailed {
                checks: vec!["verify.build".into()]
            }
        }
    );
    assert_eq!(model.requests().len(), 2, "exactly max_rounds rounds");
}

#[tokio::test]
async fn without_any_check_the_result_is_explicitly_unverified() {
    let root = project("unverified");
    let model = ScriptedModel::new(vec![say("I changed things.")]);
    let mut k = kernel(
        FakeJev::new("medium"),
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        cfg(),
    );
    let out = k.run_task(&a, "task").await.unwrap();
    assert_eq!(
        out,
        TaskOutcome::Unverified {
            summary: "I changed things.".into()
        }
    );
    let Message::System { content } = &model.requests()[0].messages[0] else {
        panic!()
    };
    assert!(
        content.contains("cannot be verified"),
        "the model is told up front"
    );
}

#[tokio::test]
async fn a_declined_permission_is_reported_to_the_model_and_nothing_is_written() {
    let root = project("denied");
    let model = ScriptedModel::new(vec![
        use_tools(&[(
            "fs_write",
            json!({"path": "secret-plan.txt", "content": "x"}),
        )]),
        say("I could not write the file."),
    ]);
    let mut k = kernel(
        FakeJev::with_tools("medium", &["fs_write"]),
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(model.clone(), &root, Permissions::default(), DenyAll, cfg());

    k.run_task(&a, "write it").await.unwrap();
    assert!(!root.join("secret-plan.txt").exists());
    assert!(last_tool_message(&model.requests()[1]).contains("permission denied"));
    let denied = k.session().events().iter().any(|e| {
        matches!(
            &e.kind,
            EventKind::ToolCalled {
                ok: false,
                authorization: agon_kernel::tools::Authorization::Denied { .. },
                ..
            }
        )
    });
    assert!(denied, "the refusal is journaled");
}

#[tokio::test]
async fn an_action_cannot_rewrite_the_rules_that_govern_it() {
    let root = project("tamper");
    let model = ScriptedModel::new(vec![
        use_tools(&[(
            "shell_run",
            json!({"command": "echo 'budget = 999' >> .agon/config.toml"}),
        )]),
        say("unreachable"),
    ]);
    let mut k = kernel(
        FakeJev::with_tools("medium", &["shell_run"]),
        &[0],
        base_form(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    // Même approuvée par l'humain, la commande ne doit pas pouvoir modifier la politique.
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        cfg(),
    );

    let out = k.run_task(&a, "task").await.unwrap();
    assert_eq!(
        out,
        TaskOutcome::HumanRequired {
            why: TaskReason::Tampering {
                files: vec![".agon/config.toml".into()]
            }
        }
    );
    assert_eq!(
        model.requests().len(),
        1,
        "the task stops immediately: the model is not called again"
    );
}

#[tokio::test]
async fn the_step_budget_stops_a_runaway_agent() {
    let root = project("steps");
    let looping: Vec<_> = (0..10)
        .map(|_| use_tools(&[("fs_list", json!({}))]))
        .collect();
    let model = ScriptedModel::new(looping);
    let mut k = kernel(
        FakeJev::new("medium"),
        &[0],
        base_form(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        AgentConfig {
            max_steps: 3,
            ..cfg()
        },
    );
    let out = k.run_task(&a, "task").await.unwrap();
    assert_eq!(
        out,
        TaskOutcome::HumanRequired {
            why: TaskReason::StepBudget { steps: 3 }
        }
    );
    assert_eq!(model.requests().len(), 3);
}

#[tokio::test]
async fn a_model_outage_hands_back_control_with_the_reason() {
    let root = project("outage");
    let model = ScriptedModel::new(vec![Err(ModelError::Unavailable {
        attempts: 3,
        last: "http 503".into(),
    })]);
    let mut k = kernel(
        FakeJev::new("medium"),
        &[0],
        base_form(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(model, &root, Permissions::default(), AutoApprove, cfg());
    let out = k.run_task(&a, "task").await.unwrap();
    assert!(
        matches!(&out, TaskOutcome::HumanRequired { why: TaskReason::ModelError { detail } } if detail.contains("unavailable")),
        "{out:?}"
    );
}

#[tokio::test]
async fn jev_picks_the_model_tier_and_prunes_the_tools() {
    let root = project("preflight");
    let model = ScriptedModel::new(vec![say("ok")]);
    let jev = FakeJev {
        tools_yes: vec!["fs_write", "fs_edit"],
        ..FakeJev::new("large")
    };
    let mut k = kernel(
        jev,
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        cfg(),
    );
    k.run_task(&a, "refactor the parser").await.unwrap();

    let req = &model.requests()[0];
    assert_eq!(req.model, "m/large");
    assert_eq!(
        tool_names(req),
        ["fs_read", "fs_list", "fs_search", "fs_write", "fs_edit"],
        "essential tools always, others only if Jev says so; shell is pruned"
    );
    let configured = k.session().events().iter().find_map(|e| match &e.kind {
        EventKind::AgentConfigured { tier, source, .. } => Some((tier.clone(), source.clone())),
        _ => None,
    });
    assert_eq!(configured, Some(("large".into(), "jev".into())));
}

#[tokio::test]
async fn low_confidence_or_a_dead_jev_falls_back_to_the_default_tier_with_every_tool() {
    let root = project("fallback");
    for (label, jev) in [
        (
            "low confidence",
            FakeJev {
                confidence: 0.3,
                ..FakeJev::new("large")
            },
        ),
        (
            "jev down",
            FakeJev {
                down: true,
                ..FakeJev::new("large")
            },
        ),
    ] {
        let model = ScriptedModel::new(vec![say("ok")]);
        let mut k = kernel(
            jev,
            &[0],
            no_checks(),
            CandidateCatalog::default(),
            Session::in_memory("t"),
        );
        let a = agent(
            model.clone(),
            &root,
            Permissions::default(),
            AutoApprove,
            cfg(),
        );
        k.run_task(&a, "task").await.unwrap();
        let req = &model.requests()[0];
        assert_eq!(req.model, "m/medium", "{label}");
        if label == "jev down" {
            assert_eq!(req.tools.len(), 9, "all tools when Jev is unavailable");
            assert!(kinds(&k).contains(&"decision.unavailable".to_string()));
        }
    }
}

#[tokio::test]
async fn a_tier_without_a_model_falls_back_to_the_default_and_no_model_at_all_is_reported() {
    let root = project("tiers");
    let model = ScriptedModel::new(vec![say("ok")]);
    let only_medium = AgentConfig {
        tiers: Tiers {
            medium: Some("m/medium".into()),
            ..Tiers::default()
        },
        ..AgentConfig::default()
    };
    let mut k = kernel(
        FakeJev::new("large"),
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    k.run_task(
        &agent(
            model.clone(),
            &root,
            Permissions::default(),
            AutoApprove,
            only_medium,
        ),
        "t",
    )
    .await
    .unwrap();
    assert_eq!(model.requests()[0].model, "m/medium");

    let none = AgentConfig {
        tiers: Tiers::default(),
        ..AgentConfig::default()
    };
    let mut k = kernel(
        FakeJev::new("large"),
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let out = k
        .run_task(
            &agent(
                ScriptedModel::default(),
                &root,
                Permissions::default(),
                AutoApprove,
                none,
            ),
            "t",
        )
        .await
        .unwrap();
    assert!(
        matches!(&out, TaskOutcome::HumanRequired { why: TaskReason::ModelError { detail } } if detail.contains("no model is configured")),
        "{out:?}"
    );
}

#[tokio::test]
async fn a_tool_pruned_by_jev_cannot_be_called_anyway() {
    let root = project("pruned");
    let model = ScriptedModel::new(vec![
        use_tools(&[("shell_run", json!({"command": "touch pwned"}))]),
        say("ok"),
    ]);
    let mut k = kernel(
        FakeJev::new("medium"),
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        cfg(),
    );
    k.run_task(&a, "task").await.unwrap();
    assert!(!root.join("pwned").exists(), "the command must not run");
    assert!(last_tool_message(&model.requests()[1]).contains("not available"));
}

#[tokio::test]
async fn jev_can_never_grant_a_permission_only_take_tools_away() {
    let root = project("noescalation");
    // Jev répond « oui » à tout, y compris git_commit ; la politique interdit le shell : il n'est pas proposé.
    let jev = FakeJev {
        tools_yes: vec!["shell_run", "git_commit", "fs_write"],
        ..FakeJev::new("medium")
    };
    let perms = Permissions {
        shell: Level::Deny,
        ..Permissions::default()
    };
    let model = ScriptedModel::new(vec![
        use_tools(&[("shell_run", json!({"command": "touch pwned"}))]),
        say("ok"),
    ]);
    let mut k = kernel(
        jev,
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    k.run_task(
        &agent(model.clone(), &root, perms, AutoApprove, cfg()),
        "task",
    )
    .await
    .unwrap();
    let names = tool_names(&model.requests()[0]);
    assert!(
        !names.contains(&"shell_run".to_string()) && names.contains(&"git_commit".to_string()),
        "{names:?}"
    );
    assert!(!root.join("pwned").exists());
}

#[tokio::test]
async fn preflight_can_be_disabled() {
    let root = project("nopreflight");
    let jev = FakeJev::new("large");
    let model = ScriptedModel::new(vec![say("ok")]);
    let mut k = kernel(
        jev.clone(),
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    k.run_task(
        &agent(
            model.clone(),
            &root,
            Permissions::default(),
            AutoApprove,
            AgentConfig {
                jev_preflight: false,
                ..cfg()
            },
        ),
        "t",
    )
    .await
    .unwrap();
    assert_eq!(jev.calls(), 0);
    assert_eq!(model.requests()[0].model, "m/medium");
}

#[tokio::test]
async fn an_environment_failure_takes_the_adaptation_path_not_the_model() {
    let root = project("env");
    let candidates = CandidateCatalog::new(vec![], ["docker".to_string(), "postgres".to_string()]);
    let jev = FakeJev {
        mutation_pick: Some("docker"),
        ..FakeJev::new("medium")
    };
    // Le modèle se contente de conclure ; la vérification échoue pour une raison d'environnement,
    // Jev choisit la mutation, elle est prouvée (le parent échoue toujours) et la tâche aboutit.
    let model = ScriptedModel::new(vec![say("nothing to change in the code")]);
    let mut k = kernel(
        jev.clone(),
        &[1, 1, 0, 1],
        integration_failing_form(),
        candidates,
        Session::in_memory("t"),
    );
    let a = agent(
        model.clone(),
        &root,
        Permissions::default(),
        AutoApprove,
        cfg(),
    );

    let out = k.run_task(&a, "make integration pass").await.unwrap();
    assert!(matches!(&out, TaskOutcome::Completed { .. }), "{out:?}");
    assert_eq!(
        model.requests().len(),
        1,
        "the model was not asked to fix an environment problem"
    );
    assert_eq!(jev.calls(), 2, "one preflight, one mutation choice");
    assert_in_order(
        &kinds(&k),
        &[
            "agent.started",
            "mutation.applied",
            "mutation.validated",
            "agent.completed",
        ],
    );
}

#[tokio::test]
async fn an_unresolvable_environment_problem_is_surfaced_not_looped_on() {
    let root = project("envstuck");
    let jev = FakeJev {
        down: true,
        ..FakeJev::new("medium")
    };
    let candidates = CandidateCatalog::new(vec![], ["docker".to_string(), "postgres".to_string()]);
    let model = ScriptedModel::new(vec![say("done")]);
    let mut k = kernel(
        jev,
        &[1],
        integration_failing_form(),
        candidates,
        Session::in_memory("t"),
    );
    let out = k
        .run_task(
            &agent(
                model.clone(),
                &root,
                Permissions::default(),
                AutoApprove,
                cfg(),
            ),
            "task",
        )
        .await
        .unwrap();
    assert!(
        matches!(
            &out,
            TaskOutcome::HumanRequired {
                why: TaskReason::Environment {
                    why: HumanReason::JevUnavailable { .. }
                }
            }
        ),
        "{out:?}"
    );
    assert_eq!(
        model.requests().len(),
        1,
        "no second round on an environment failure"
    );
}

#[tokio::test]
async fn the_confirmer_sees_exactly_what_will_run() {
    struct Spy(Mutex<Vec<String>>);
    impl Confirmer for Spy {
        fn confirm(&self, r: &ConfirmRequest<'_>) -> Confirmation {
            self.0
                .lock()
                .unwrap()
                .push(format!("{}: {}", r.tool, r.detail));
            Confirmation::Deny
        }
    }
    let root = project("spy");
    let model = ScriptedModel::new(vec![
        use_tools(&[
            ("shell_run", json!({"command": "cargo test --lib"})),
            ("fs_edit", json!({"path": "a.rs", "old": "x", "new": "y"})),
        ]),
        say("ok"),
    ]);
    let jev = FakeJev {
        tools_yes: vec!["shell_run", "fs_edit"],
        ..FakeJev::new("medium")
    };
    let mut k = kernel(
        jev,
        &[0],
        no_checks(),
        CandidateCatalog::default(),
        Session::in_memory("t"),
    );
    let spy = Spy(Mutex::default());
    let a = agent(model, &root, Permissions::default(), spy, cfg());
    k.run_task(&a, "task").await.unwrap();
    let seen = a.confirmer.0.lock().unwrap().clone();
    assert_eq!(
        seen,
        [
            "shell_run: cargo test --lib",
            "fs_edit: a.rs: replace `x` → `y`"
        ]
    );
}
