//! Chat, plan et code : une conversation continue, où l'architecture ne s'enclenche qu'au moment de coder.

use agon_core::fixtures::{base_form, catalog};
use agon_core::{Check, Form};
use agon_kernel::tools::{AutoApprove, Tools};
use agon_kernel::{
    Agent, AgentConfig, CancelToken, CandidateCatalog, ChatOutcome, Conversation, Kernel,
    KernelConfig, KernelParts, Mode, NoopActivator, ProcessRunner, Session, TaskOutcome,
    TaskReason, Tiers,
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

// ── doubles (mêmes principes que agent_loop.rs) ───────────────────────────────────────────

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

fn say(text: &str) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse {
        message: Message::Assistant {
            content: Some(text.into()),
            tool_calls: vec![],
        },
        finish_reason: Some("stop".into()),
        model: "vendor/answered".into(),
        usage: ChatUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            cost: None,
        },
    })
}

fn use_tool(name: &str, args: Value) -> Result<ChatResponse, ModelError> {
    Ok(ChatResponse {
        message: Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall::new("call_0", name, args.to_string())],
        },
        finish_reason: Some("tool_calls".into()),
        model: "vendor/answered".into(),
        usage: ChatUsage::default(),
    })
}

#[derive(Clone, Default)]
struct FakeJev {
    calls: Arc<AtomicU32>,
}

impl DecisionProvider for FakeJev {
    fn decide(
        &self,
        _s: Value,
        questions: BTreeMap<String, Question>,
    ) -> impl Future<Output = Result<Response, JevError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut answers = BTreeMap::new();
        for k in questions.keys() {
            let a = if k == "model_tier" {
                Answer::Choice {
                    choice: "medium".into(),
                    probabilities: BTreeMap::from([("medium".into(), 1.0)]),
                    confidence: 0.9,
                }
            } else {
                Answer::Noul { noul: 0.9 }
            };
            answers.insert(k.clone(), a);
        }
        let r = Ok(Response {
            id: None,
            model: "jev-1.13.0".into(),
            provider: None,
            answers,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cost: None,
            },
        });
        async move { r }
    }
}

struct Script(Mutex<(Vec<i32>, usize)>);

impl CheckRunner for Script {
    fn run(&self, check: &Check) -> Observation {
        let mut g = self.0.lock().unwrap();
        let code = g.0[g.1.min(g.0.len() - 1)];
        g.1 += 1;
        Observation {
            check: check.id.clone(),
            exit_code: Some(code),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        }
    }
}

fn project(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agon-chat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join(".agon")).unwrap();
    std::fs::write(d.join("notes.txt"), "the answer is 42\n").unwrap();
    d.canonicalize().unwrap()
}

fn config() -> AgentConfig {
    AgentConfig {
        tiers: Tiers {
            medium: Some("m/medium".into()),
            ..Tiers::default()
        },
        ..AgentConfig::default()
    }
}

type TestKernel = Kernel<FakeJev, Script, NoopActivator>;

fn kernel(jev: FakeJev, codes: &[i32], form: Form) -> TestKernel {
    Kernel::new(KernelParts {
        decisions: jev,
        runner: Script(Mutex::new((codes.to_vec(), 0))),
        activator: NoopActivator,
        config: KernelConfig::default(),
        catalog: catalog(),
        extractors: ExtractorSet::default(),
        candidates: CandidateCatalog::default(),
        registry: Registry::new(),
        form,
        session: Session::in_memory("t"),
    })
    .unwrap()
}

fn agent(
    model: ScriptedModel,
    root: &Path,
    perms: Permissions,
) -> Agent<ScriptedModel, AutoApprove> {
    Agent {
        model,
        tools: Tools::new(root, perms, ProcessRunner::new(root)).unwrap(),
        confirmer: AutoApprove,
        config: config(),
    }
}

fn names(r: &ChatRequest) -> Vec<String> {
    r.tools.iter().map(|t| t.name.clone()).collect()
}

fn system_text(r: &ChatRequest) -> String {
    let Message::System { content } = &r.messages[0] else {
        panic!("the first message is the system prompt")
    };
    content.clone()
}

const PLAN: &str = "# Plan\n\n## Goal\nAdd the feature.\n\n## Steps\n1. Edit notes.txt\n\n## Verification\nverify.build";

// ── chat ──────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_answers_with_read_only_tools_and_never_touches_jev() {
    let root = project("chat");
    let model = ScriptedModel::new(vec![
        use_tool("fs_read", json!({"path": "notes.txt"})),
        say("The notes say the answer is 42."),
    ]);
    let jev = FakeJev::default();
    let mut k = kernel(jev.clone(), &[0], base_form());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();

    let out = k
        .chat_turn(&a, &mut conv, Mode::Chat, "what do the notes say?")
        .await
        .unwrap();
    assert_eq!(
        out,
        ChatOutcome::Answer("The notes say the answer is 42.".into())
    );
    assert_eq!(
        jev.calls.load(Ordering::SeqCst),
        0,
        "chatting is cheap: no Jev, no verification"
    );

    let req = &model.requests()[0];
    assert_eq!(
        names(req),
        ["fs_read", "fs_list", "fs_search", "git_status", "git_diff"],
        "read-only tools only"
    );
    assert!(
        system_text(req).contains("can NOT modify files"),
        "{}",
        system_text(req)
    );
    // Le résultat de l'outil est revenu au modèle.
    let second_request = model.requests()[1].clone();
    let Message::Tool { content, .. } = second_request.messages.last().unwrap() else {
        panic!()
    };
    assert!(content.contains("the answer is 42"));

    let kinds = k.session().kinds();
    assert!(
        kinds.contains(&"chat.user".to_string())
            && kinds.contains(&"tool.called".to_string())
            && kinds.contains(&"agent.message".to_string()),
        "{kinds:?}"
    );
    assert!(
        !kinds.contains(&"verification.started".to_string()),
        "no check is run while chatting"
    );
}

#[tokio::test]
async fn chat_cannot_write_even_if_the_model_tries_and_everything_is_auto_approved() {
    let root = project("readonly");
    let attempts = vec![
        use_tool("fs_write", json!({"path": "pwned.txt", "content": "x"})),
        use_tool("shell_run", json!({"command": "touch pwned2"})),
        use_tool(
            "git_commit",
            json!({"message": "m", "paths": ["notes.txt"]}),
        ),
        say("I could not change anything."),
    ];
    let model = ScriptedModel::new(attempts);
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();
    for mode in [Mode::Chat, Mode::Plan] {
        conv.clear();
        model.replies.lock().unwrap().clear();
        let script = vec![
            use_tool("fs_write", json!({"path": "pwned.txt", "content": "x"})),
            use_tool("shell_run", json!({"command": "touch pwned2"})),
            say("done"),
        ];
        model.replies.lock().unwrap().extend(script);
        k.chat_turn(&a, &mut conv, mode, "change things")
            .await
            .unwrap();
        assert!(
            !root.join("pwned.txt").exists() && !root.join("pwned2").exists(),
            "{mode:?}: nothing may be written or executed"
        );
    }
    let last_tool = model
        .requests()
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter_map(|m| {
            if let Message::Tool { content, .. } = m {
                Some(content.clone())
            } else {
                None
            }
        })
        .next_back()
        .unwrap();
    assert!(last_tool.contains("not available"), "{last_tool}");
}

#[tokio::test]
async fn the_conversation_is_kept_between_messages_and_the_system_prompt_is_never_duplicated() {
    let root = project("continuity");
    let model = ScriptedModel::new(vec![say("First answer."), say("Second answer.")]);
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();

    k.chat_turn(&a, &mut conv, Mode::Chat, "first question")
        .await
        .unwrap();
    k.chat_turn(&a, &mut conv, Mode::Chat, "and a follow-up?")
        .await
        .unwrap();

    let second = &model.requests()[1];
    let texts: Vec<String> = second
        .messages
        .iter()
        .map(|m| match m {
            Message::System { content }
            | Message::User { content }
            | Message::Tool { content, .. } => content.clone(),
            Message::Assistant { content, .. } => content.clone().unwrap_or_default(),
        })
        .collect();
    assert!(
        texts[1] == "first question"
            && texts[2] == "First answer."
            && texts[3] == "and a follow-up?",
        "{texts:?}"
    );
    assert_eq!(
        second
            .messages
            .iter()
            .filter(|m| matches!(m, Message::System { .. }))
            .count(),
        1
    );
    assert_eq!(conv.len(), 5, "system + 2 user + 2 assistant");
}

#[tokio::test]
async fn switching_mode_replaces_the_system_prompt_but_keeps_the_history() {
    let root = project("switch");
    let model = ScriptedModel::new(vec![say("Chat answer."), say("Plan-mode answer.")]);
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();
    k.chat_turn(&a, &mut conv, Mode::Chat, "hello")
        .await
        .unwrap();
    k.chat_turn(&a, &mut conv, Mode::Plan, "let's plan")
        .await
        .unwrap();

    let (first, second) = (&model.requests()[0], &model.requests()[1]);
    assert!(!system_text(first).contains("PLAN mode") && system_text(second).contains("PLAN mode"));
    assert_eq!(
        second
            .messages
            .iter()
            .filter(|m| matches!(m, Message::System { .. }))
            .count(),
        1,
        "replaced, not appended"
    );
    assert!(
        second
            .messages
            .iter()
            .any(|m| matches!(m, Message::User { content } if content == "hello")),
        "the earlier message is still in the context"
    );
}

// ── plan ──────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_finished_plan_is_captured_but_a_clarifying_question_is_not() {
    let root = project("plan");
    let model = ScriptedModel::new(vec![say("Which database do you use?"), say(PLAN)]);
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();

    let q = k
        .chat_turn(&a, &mut conv, Mode::Plan, "add persistence")
        .await
        .unwrap();
    assert!(
        matches!(q, ChatOutcome::Answer(_)),
        "a question is not a plan: {q:?}"
    );
    assert!(!k.session().kinds().contains(&"plan.proposed".to_string()));

    let p = k
        .chat_turn(&a, &mut conv, Mode::Plan, "postgres")
        .await
        .unwrap();
    assert_eq!(p, ChatOutcome::Plan(PLAN.into()));
    assert_eq!(
        k.session()
            .kinds()
            .iter()
            .filter(|k| *k == "plan.proposed")
            .count(),
        1
    );
    assert!(
        system_text(&model.requests()[0]).contains("# Plan"),
        "the model is told how to format a plan"
    );
}

#[tokio::test]
async fn a_plan_in_chat_mode_is_just_an_answer() {
    let root = project("planchat");
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = agent(
        ScriptedModel::new(vec![say(PLAN)]),
        &root,
        Permissions::default(),
    );
    let out = k
        .chat_turn(&a, &mut Conversation::new(), Mode::Chat, "x")
        .await
        .unwrap();
    assert!(
        matches!(out, ChatOutcome::Answer(_)),
        "only plan mode captures plans"
    );
}

// ── go : l'architecture s'enclenche ───────────────────────────────────────────────────────

#[tokio::test]
async fn approving_the_plan_starts_coding_with_the_whole_architecture_and_the_plan_in_context() {
    let root = project("go");
    let model = ScriptedModel::new(vec![
        say(PLAN), // plan
        use_tool(
            "fs_write",
            json!({"path": "feature.txt", "content": "done"}),
        ), // code
        say("Implemented the plan."),
    ]);
    let jev = FakeJev::default();
    let mut k = kernel(jev.clone(), &[0], base_form());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();

    k.chat_turn(&a, &mut conv, Mode::Plan, "add the feature")
        .await
        .unwrap();
    assert_eq!(
        jev.calls.load(Ordering::SeqCst),
        0,
        "planning does not involve Jev"
    );

    let out = k
        .run_task_in(&a, &mut conv, "Implement the plan above.")
        .await
        .unwrap();
    assert!(
        matches!(&out, TaskOutcome::Completed { summary, .. } if summary == "Implemented the plan."),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("feature.txt")).unwrap(),
        "done"
    );

    // Jev décide maintenant (niveau de modèle et outils), les checks jugent.
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);
    let reqs = model.requests();
    let code_req = &reqs[1];
    assert!(
        names(code_req).contains(&"fs_write".to_string()),
        "code mode gets the tools that act"
    );
    assert!(
        system_text(code_req).contains("If a plan was agreed"),
        "{}",
        system_text(code_req)
    );
    assert!(
        code_req.messages.iter().any(
            |m| matches!(m, Message::Assistant { content: Some(c), .. } if c.starts_with("# Plan"))
        ),
        "the approved plan is in the context"
    );
    assert_eq!(
        code_req
            .messages
            .iter()
            .filter(|m| matches!(m, Message::System { .. }))
            .count(),
        1
    );
    let kinds = k.session().kinds();
    for expected in [
        "plan.proposed",
        "agent.started",
        "decision.completed",
        "agent.configured",
        "verification.passed",
        "agent.completed",
    ] {
        assert!(
            kinds.contains(&expected.to_string()),
            "missing `{expected}` in {kinds:?}"
        );
    }
}

#[tokio::test]
async fn coding_still_needs_the_checks_to_pass_after_a_conversation() {
    let root = project("verify");
    // Le modèle « termine » sans rien faire, deux fois : la vérification échoue, la main est rendue.
    let model = ScriptedModel::new(vec![
        say("Sure, let's talk."),
        say("Done!"),
        say("Done again!"),
    ]);
    let mut k = kernel(FakeJev::default(), &[1], base_form());
    let a = Agent {
        config: AgentConfig {
            max_rounds: 2,
            ..config()
        },
        ..agent(model, &root, Permissions::default())
    };
    let mut conv = Conversation::new();
    k.chat_turn(&a, &mut conv, Mode::Chat, "hi").await.unwrap();
    let out = k.run_task_in(&a, &mut conv, "fix it").await.unwrap();
    assert_eq!(
        out,
        TaskOutcome::HumanRequired {
            why: TaskReason::VerificationFailed {
                checks: vec!["verify.build".into()]
            }
        }
    );
}

#[tokio::test]
async fn run_task_without_a_conversation_still_works_as_before() {
    let root = project("compat");
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = agent(
        ScriptedModel::new(vec![say("ok")]),
        &root,
        Permissions::default(),
    );
    assert!(matches!(
        k.run_task(&a, "task").await.unwrap(),
        TaskOutcome::Completed { .. }
    ));
}

// ── limites ───────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn without_a_model_the_turn_stops_with_a_clear_message() {
    let root = project("nomodel");
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = Agent {
        config: AgentConfig {
            tiers: Tiers::default(),
            ..config()
        },
        ..agent(ScriptedModel::default(), &root, Permissions::default())
    };
    let out = k
        .chat_turn(&a, &mut Conversation::new(), Mode::Chat, "hi")
        .await
        .unwrap();
    assert!(
        matches!(&out, ChatOutcome::Stopped(TaskReason::ModelError { detail }) if detail.contains("no model is configured") && detail.contains("/model")),
        "{out:?}"
    );
}

#[tokio::test]
async fn an_oversized_conversation_is_refused_before_any_call_and_the_message_is_not_kept() {
    let root = project("toolong");
    let model = ScriptedModel::new(vec![say("never reached")]);
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();
    let huge = "x".repeat(agon_kernel::Conversation::new().approx_chars() + 600_000);
    let out = k.chat_turn(&a, &mut conv, Mode::Chat, &huge).await.unwrap();
    assert!(
        matches!(&out, ChatOutcome::Stopped(TaskReason::ModelError { detail }) if detail.contains("too long") && detail.contains("/new")),
        "{out:?}"
    );
    assert!(model.requests().is_empty(), "nothing is sent");
    assert!(
        !conv
            .messages()
            .iter()
            .any(|m| matches!(m, Message::User { content } if content.len() > 1000)),
        "the oversized message was dropped"
    );
}

#[tokio::test]
async fn the_step_budget_and_model_errors_stop_a_chat_turn() {
    let root = project("limits");
    let looping: Vec<_> = (0..10).map(|_| use_tool("fs_list", json!({}))).collect();
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let a = Agent {
        config: AgentConfig {
            max_steps: 3,
            ..config()
        },
        ..agent(ScriptedModel::new(looping), &root, Permissions::default())
    };
    let out = k
        .chat_turn(&a, &mut Conversation::new(), Mode::Chat, "loop")
        .await
        .unwrap();
    assert_eq!(
        out,
        ChatOutcome::Stopped(TaskReason::StepBudget { steps: 3 })
    );

    let a = agent(
        ScriptedModel::new(vec![Err(ModelError::Unavailable {
            attempts: 3,
            last: "http 503".into(),
        })]),
        &root,
        Permissions::default(),
    );
    let out = k
        .chat_turn(&a, &mut Conversation::new(), Mode::Chat, "hi")
        .await
        .unwrap();
    assert!(
        matches!(&out, ChatOutcome::Stopped(TaskReason::ModelError { detail }) if detail.contains("unavailable")),
        "{out:?}"
    );
}

#[tokio::test]
async fn a_denied_read_permission_removes_the_tool_from_chat_too() {
    let root = project("denyread");
    let model = ScriptedModel::new(vec![say("ok")]);
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let perms = Permissions {
        search: Level::Deny,
        ..Permissions::default()
    };
    let a = agent(model.clone(), &root, perms);
    k.chat_turn(&a, &mut Conversation::new(), Mode::Chat, "hi")
        .await
        .unwrap();
    assert!(
        !names(&model.requests()[0]).contains(&"fs_search".to_string()),
        "policy still applies in chat"
    );
}

// ── modes et détection de plan (unités) ───────────────────────────────────────────────────

#[test]
fn modes_cycle_parse_and_serialize() {
    assert_eq!(
        (Mode::Chat.next(), Mode::Plan.next(), Mode::Code.next()),
        (Mode::Plan, Mode::Code, Mode::Chat)
    );
    for m in [Mode::Chat, Mode::Plan, Mode::Code] {
        assert_eq!(Mode::parse(m.name()), Some(m));
        assert_eq!(serde_json::to_value(m).unwrap(), json!(m.name()));
    }
    assert_eq!(Mode::parse("yolo"), None);
    assert_eq!(Mode::default(), Mode::Chat, "the safe mode is the default");
}

#[test]
fn plan_detection_needs_a_plan_heading_not_just_the_word() {
    use agon_kernel::looks_like_plan;
    for yes in [
        "# Plan\nsteps",
        "intro\n## Plan\n1. x",
        "  ### Plan: caching",
        "# plan",
    ] {
        assert!(looks_like_plan(yes), "{yes:?}");
    }
    for no in [
        "I plan to look at it",
        "Which database?",
        "the plan is simple",
        "#planning is fun",
        "#### Plan",
        "",
    ] {
        assert!(!looks_like_plan(no), "{no:?}");
    }
}

// ── streaming ─────────────────────────────────────────────────────────────────────────────

/// Modèle qui streame : deux fragments, puis le message complet.
struct StreamingModel;

impl ModelProvider for StreamingModel {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, ModelError> {
        say("hello")
    }

    fn chat_stream(
        &self,
        _request: ChatRequest,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> impl Future<Output = Result<ChatResponse, ModelError>> + Send {
        on_delta("he");
        on_delta("llo");
        async { say("hello") }
    }
}

#[tokio::test]
async fn streamed_fragments_reach_the_display_but_only_the_full_message_is_journaled() {
    let root = project("stream");
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    k.set_delta_sink(move |d| sink.lock().unwrap().push(d.to_string()));
    let a = Agent {
        model: StreamingModel,
        tools: Tools::new(&root, Permissions::default(), ProcessRunner::new(&root)).unwrap(),
        confirmer: AutoApprove,
        config: config(),
    };
    let mut conv = Conversation::new();
    let out = k.chat_turn(&a, &mut conv, Mode::Chat, "hi").await.unwrap();

    assert_eq!(out, ChatOutcome::Answer("hello".into()));
    assert_eq!(*seen.lock().unwrap(), ["he", "llo"]);
    let messages = k
        .session()
        .kinds()
        .iter()
        .filter(|k| *k == "agent.message")
        .count();
    assert_eq!(messages, 1, "one journal entry, not one per fragment");
}

#[tokio::test]
async fn a_model_that_does_not_stream_works_without_a_display_sink() {
    let root = project("nostream");
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    k.set_delta_sink(|_| panic!("a non-streaming model produces no fragments"));
    let a = agent(
        ScriptedModel::new(vec![say("plain")]),
        &root,
        Permissions::default(),
    );
    let mut conv = Conversation::new();
    let out = k.chat_turn(&a, &mut conv, Mode::Chat, "hi").await.unwrap();
    assert_eq!(out, ChatOutcome::Answer("plain".into()));
}

// ── annulation ────────────────────────────────────────────────────────────────────────────

/// Modèle qui ne répond jamais.
struct Hanging;

impl ModelProvider for Hanging {
    async fn chat(&self, _r: ChatRequest) -> Result<ChatResponse, ModelError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancelling_stops_a_turn_waiting_for_the_model() {
    let root = project("cancel-wait");
    let mut k = kernel(FakeJev::default(), &[0], base_form());
    let token: CancelToken = k.cancel_token();
    let a = Agent {
        model: Hanging,
        tools: Tools::new(&root, Permissions::default(), ProcessRunner::new(&root)).unwrap(),
        confirmer: AutoApprove,
        config: config(),
    };
    let mut conv = Conversation::new();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        token.cancel();
    });
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        k.chat_turn(&a, &mut conv, Mode::Chat, "hi"),
    )
    .await
    .expect("cancelling must not wait for the model")
    .unwrap();
    assert_eq!(out, ChatOutcome::Stopped(TaskReason::Cancelled));
}

#[tokio::test]
async fn a_cancelled_tool_batch_leaves_a_valid_conversation_and_the_next_turn_works() {
    let root = project("cancel-tools");
    // Deux appels d'outils dans le même message ; l'annulation arrive avant le premier.
    let two_calls = Ok(ChatResponse {
        message: Message::Assistant {
            content: None,
            tool_calls: vec![
                ToolCall::new("c1", "fs_read", json!({"path": "notes.txt"}).to_string()),
                ToolCall::new("c2", "fs_list", json!({"path": "."}).to_string()),
            ],
        },
        finish_reason: Some("tool_calls".into()),
        model: "vendor/answered".into(),
        usage: ChatUsage::default(),
    });
    let model = ScriptedModel::new(vec![two_calls, say("back to normal")]);
    // Le jeton est levé dès que le modèle a répondu : depuis un observateur du journal.
    let slot: Arc<Mutex<Option<CancelToken>>> = Arc::default();
    let mut session = Session::in_memory("t");
    let s2 = slot.clone();
    session.set_sink(move |e| {
        if matches!(e.kind, agon_kernel::EventKind::AgentMessage { .. })
            && let Some(t) = s2.lock().unwrap().as_ref()
        {
            t.cancel();
        }
    });
    let mut k = Kernel::new(KernelParts {
        decisions: FakeJev::default(),
        runner: Script(Mutex::new((vec![0], 0))),
        activator: NoopActivator,
        config: KernelConfig::default(),
        catalog: catalog(),
        extractors: ExtractorSet::default(),
        candidates: CandidateCatalog::default(),
        registry: Registry::new(),
        form: base_form(),
        session,
    })
    .unwrap();
    let token = k.cancel_token();
    *slot.lock().unwrap() = Some(token.clone());
    let a = agent(model.clone(), &root, Permissions::default());
    let mut conv = Conversation::new();

    let out = k
        .chat_turn(&a, &mut conv, Mode::Chat, "look around")
        .await
        .unwrap();
    assert_eq!(out, ChatOutcome::Stopped(TaskReason::Cancelled));

    // Chaque tool_call a reçu une réponse : sans cela, l'API refuserait la suite.
    let tool_ids: Vec<&str> = conv
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_ids, ["c1", "c2"]);

    token.reset();
    let out = k.chat_turn(&a, &mut conv, Mode::Chat, "ok?").await.unwrap();
    assert_eq!(out, ChatOutcome::Answer("back to normal".into()));
}

// ── reprise ───────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_conversation_is_rebuilt_from_the_journal_and_the_model_sees_it() {
    let root = project("resume");
    let m1 = ScriptedModel::new(vec![
        use_tool("fs_read", json!({"path": "notes.txt"})),
        say("It says 42."),
    ]);
    let mut k1 = kernel(FakeJev::default(), &[0], base_form());
    let a1 = agent(m1, &root, Permissions::default());
    let mut c1 = Conversation::new();
    k1.chat_turn(&a1, &mut c1, Mode::Chat, "what do the notes say?")
        .await
        .unwrap();

    // Nouvelle « exécution » : seul le journal survit.
    let rebuilt = Conversation::from_events(k1.session().events());
    let texts: Vec<String> = rebuilt
        .messages()
        .iter()
        .map(|m| match m {
            Message::User { content } => format!("user:{content}"),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert!(tool_calls.is_empty(), "tool calls are not replayed");
                format!("assistant:{}", content.clone().unwrap())
            }
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        texts,
        ["user:what do the notes say?", "assistant:It says 42."]
    );

    let m2 = ScriptedModel::new(vec![say("Yes, 42.")]);
    let mut k2 = kernel(FakeJev::default(), &[0], base_form());
    let a2 = agent(m2.clone(), &root, Permissions::default());
    let mut c2 = rebuilt;
    k2.chat_turn(&a2, &mut c2, Mode::Chat, "are you sure?")
        .await
        .unwrap();
    let req = &m2.requests()[0];
    assert_eq!(req.messages.len(), 4, "system + 2 restored + new question");
    assert!(
        matches!(&req.messages[2], Message::Assistant { content: Some(c), .. } if c == "It says 42.")
    );
}
