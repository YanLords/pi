//! Les outils de l'agent : chemins confinés, permissions, confirmation, git réel.

use agon_kernel::ProcessRunner;
use agon_kernel::tools::{
    Authorization, AutoApprove, ConfirmRequest, Confirmation, Confirmer, DenyAll, Limits,
    TOOL_NAMES, ToolOutcome, Tools,
};
use agon_model::ToolCall;
use agon_policy::{Action, Level, Permissions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agon-tools-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn tools(root: &Path) -> Tools {
    Tools::new(root, Permissions::default(), ProcessRunner::new(root)).unwrap()
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall::new("call_1", name, args.to_string())
}

fn run(t: &Tools, name: &str, args: Value, c: &dyn Confirmer) -> ToolOutcome {
    t.execute(&call(name, args), c)
}

/// Enregistre les demandes et répond toujours `answer`.
struct Recording {
    asked: Mutex<Vec<String>>,
    answer: Confirmation,
}
impl Recording {
    fn new(answer: Confirmation) -> Self {
        Recording {
            asked: Mutex::new(vec![]),
            answer,
        }
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}
impl Confirmer for Recording {
    fn confirm(&self, r: &ConfirmRequest<'_>) -> Confirmation {
        self.asked
            .lock()
            .unwrap()
            .push(format!("{}|{:?}|{}", r.tool, r.action, r.detail));
        self.answer
    }
}

fn denied(o: &ToolOutcome) -> bool {
    matches!(o.authorization, Authorization::Denied { .. })
}

fn invalid(o: &ToolOutcome) -> bool {
    matches!(o.authorization, Authorization::Invalid { .. })
}

// ── lecture ────────────────────────────────────────────────────────────────────────────────

#[test]
fn read_numbers_lines_and_pages() {
    let root = scratch("read");
    let text: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    std::fs::write(root.join("a.txt"), text).unwrap();
    let t = tools(&root);

    let o = run(&t, "fs_read", json!({"path": "a.txt"}), &DenyAll);
    assert!(o.ok && o.authorization == Authorization::Allowed, "{o:?}");
    assert!(
        o.content.starts_with("    1\tline 1\n") && o.content.contains("   10\tline 10"),
        "{}",
        o.content
    );

    let o = run(
        &t,
        "fs_read",
        json!({"path": "a.txt", "offset": 3, "limit": 2}),
        &DenyAll,
    );
    assert!(
        o.content.contains("    3\tline 3")
            && o.content.contains("    4\tline 4")
            && !o.content.contains("line 5\n")
    );
    assert!(
        o.content.contains("use offset=5 to continue"),
        "{}",
        o.content
    );

    let o = run(
        &t,
        "fs_read",
        json!({"path": "a.txt", "offset": 99}),
        &DenyAll,
    );
    assert!(!o.ok && o.content.contains("past the end"), "{}", o.content);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_limits_binary_files_and_missing_files() {
    let root = scratch("read2");
    std::fs::write(root.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    std::fs::write(root.join("big.txt"), ("x".repeat(200) + "\n").repeat(1000)).unwrap();
    let t = Tools::new(&root, Permissions::default(), ProcessRunner::new(&root))
        .unwrap()
        .with_limits(Limits {
            max_read_bytes: 1000,
            ..Limits::default()
        });
    assert!(
        run(&t, "fs_read", json!({"path": "bin.dat"}), &DenyAll)
            .content
            .contains("binary")
    );
    assert!(!run(&t, "fs_read", json!({"path": "nope.txt"}), &DenyAll).ok);
    let o = run(&t, "fs_read", json!({"path": "big.txt"}), &DenyAll);
    assert!(
        o.content.len() < 1500 && o.content.contains("to continue"),
        "output is bounded: {}",
        o.content.len()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn secrets_git_internals_and_escapes_are_off_limits_for_reading() {
    let root = scratch("secrets");
    std::fs::write(root.join(".env"), "API_KEY=hunter2").unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join(".git/config"),
        "[remote] url = https://token@example.com",
    )
    .unwrap();
    std::fs::write(root.join("deploy.pem"), "-----BEGIN").unwrap();
    let outside = scratch("secrets-outside");
    std::fs::write(outside.join("x.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let t = tools(&root);

    for path in [
        ".env",
        ".git/config",
        "deploy.pem",
        "../x.txt",
        "/etc/hosts",
        "link/x.txt",
    ] {
        let o = run(&t, "fs_read", json!({"path": path}), &AutoApprove);
        assert!(invalid(&o) && !o.ok, "{path}: {o:?}");
        assert!(
            !o.content.contains("hunter2") && !o.content.contains("token@"),
            "{path} leaked: {}",
            o.content
        );
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn list_hides_git_marks_directories_and_caps_entries() {
    let root = scratch("list");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    for i in 0..5 {
        std::fs::write(root.join(format!("f{i}.txt")), "").unwrap();
    }
    let t = Tools::new(&root, Permissions::default(), ProcessRunner::new(&root))
        .unwrap()
        .with_limits(Limits {
            max_list_entries: 3,
            ..Limits::default()
        });
    let o = run(&t, "fs_list", json!({}), &DenyAll);
    assert!(
        o.ok && !o.content.contains(".git") && o.content.contains("[+3 more entries]"),
        "{}",
        o.content
    );
    let all = tools(&root);
    let o = run(&all, "fs_list", json!({"path": "."}), &DenyAll);
    assert!(
        o.content.contains("src/") && o.content.contains("f0.txt"),
        "{}",
        o.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_finds_matches_and_skips_noise_secrets_binaries_and_symlinks() {
    let root = scratch("search");
    for d in ["src", "target", "node_modules", ".git", ".agon"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
        std::fs::write(root.join(d).join("hit.rs"), "needle here\n").unwrap();
    }
    std::fs::write(root.join("src/lib.rs"), "fn a() {}\nlet needle = 1;\n").unwrap();
    std::fs::write(root.join("notes.md"), "a needle in notes\n").unwrap();
    std::fs::write(root.join(".env"), "needle=secret\n").unwrap();
    std::fs::write(root.join("blob.bin"), b"needle\0binary").unwrap();
    let outside = scratch("search-outside");
    std::fs::write(outside.join("o.txt"), "needle outside\n").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let t = tools(&root);

    let o = run(&t, "fs_search", json!({"pattern": "needle"}), &DenyAll);
    assert!(o.ok, "{o:?}");
    assert!(
        o.content.contains("src/lib.rs:2:")
            && o.content.contains("notes.md:1:")
            && o.content.contains("src/hit.rs:1:"),
        "{}",
        o.content
    );
    for skipped in [
        "target/",
        "node_modules/",
        ".git/",
        ".agon/",
        ".env",
        "blob.bin",
        "outside",
    ] {
        assert!(
            !o.content.contains(skipped),
            "must skip {skipped}: {}",
            o.content
        );
    }
    let o = run(
        &t,
        "fs_search",
        json!({"pattern": "needle", "glob": "*.md"}),
        &DenyAll,
    );
    assert!(
        o.content.contains("notes.md") && !o.content.contains("lib.rs"),
        "{}",
        o.content
    );
    assert!(
        invalid(&run(&t, "fs_search", json!({"pattern": "("}), &DenyAll)),
        "a bad regex is reported, not fatal"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn search_results_are_capped() {
    let root = scratch("search-cap");
    std::fs::write(root.join("many.txt"), "hit\n".repeat(50)).unwrap();
    let t = Tools::new(&root, Permissions::default(), ProcessRunner::new(&root))
        .unwrap()
        .with_limits(Limits {
            max_search_matches: 5,
            ..Limits::default()
        });
    let o = run(&t, "fs_search", json!({"pattern": "hit"}), &DenyAll);
    assert!(
        o.content.contains("5 match(es), truncated"),
        "{}",
        o.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── écriture, confirmation ─────────────────────────────────────────────────────────────────

#[test]
fn writing_asks_first_and_a_refusal_leaves_no_trace() {
    let root = scratch("write");
    let t = tools(&root);

    let no = Recording::new(Confirmation::Deny);
    let o = run(
        &t,
        "fs_write",
        json!({"path": "new.txt", "content": "hello"}),
        &no,
    );
    assert!(
        denied(&o) && !o.ok && o.content.contains("permission denied"),
        "{o:?}"
    );
    assert!(!root.join("new.txt").exists(), "nothing written");
    assert_eq!(no.asked(), ["fs_write|Write|new.txt (5 bytes, new file)"]);

    let yes = Recording::new(Confirmation::Allow);
    let o = run(
        &t,
        "fs_write",
        json!({"path": "deep/dir/new.txt", "content": "hello"}),
        &yes,
    );
    assert!(o.ok && o.authorization == Authorization::Confirmed, "{o:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("deep/dir/new.txt")).unwrap(),
        "hello"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn allow_for_the_session_is_remembered_for_write_and_edit_only() {
    let root = scratch("session");
    let t = tools(&root);
    let c = Recording::new(Confirmation::AllowSession);

    run(&t, "fs_write", json!({"path": "a.txt", "content": "1"}), &c);
    let o = run(&t, "fs_write", json!({"path": "b.txt", "content": "2"}), &c);
    assert_eq!(o.authorization, Authorization::ConfirmedForSession);
    assert_eq!(c.asked().len(), 1, "the second write did not ask again");
    let o = run(
        &t,
        "fs_edit",
        json!({"path": "a.txt", "old": "1", "new": "3"}),
        &c,
    );
    assert!(
        o.ok && c.asked().len() == 2,
        "edit is a different action and asks once"
    );

    for _ in 0..2 {
        run(&t, "shell_run", json!({"command": "true"}), &c);
    }
    assert_eq!(
        c.asked()
            .iter()
            .filter(|a| a.starts_with("shell_run"))
            .count(),
        2,
        "shell asks every time"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn agon_state_and_the_project_boundary_cannot_be_written() {
    let root = scratch("protected");
    std::fs::create_dir_all(root.join(".agon")).unwrap();
    std::fs::write(root.join(".agon/config.toml"), "[morph.budget]\n").unwrap();
    let t = tools(&root);
    let never = Recording::new(Confirmation::Allow);

    for path in [
        ".agon/config.toml",
        ".agon/new.toml",
        ".env",
        ".git/hooks/pre-commit",
        "../escape.txt",
        "/tmp/agon-escape.txt",
    ] {
        let o = run(
            &t,
            "fs_write",
            json!({"path": path, "content": "x"}),
            &never,
        );
        assert!(invalid(&o), "{path}: {o:?}");
    }
    assert!(
        never.asked().is_empty(),
        "the human is never asked about an impossible action"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".agon/config.toml")).unwrap(),
        "[morph.budget]\n"
    );
    // Lire la configuration reste possible : elle ne contient pas de secret.
    assert!(
        run(
            &t,
            "fs_read",
            json!({"path": ".agon/config.toml"}),
            &DenyAll
        )
        .ok
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn oversized_writes_are_refused() {
    let root = scratch("big");
    let t = Tools::new(&root, Permissions::default(), ProcessRunner::new(&root))
        .unwrap()
        .with_limits(Limits {
            max_write_bytes: 10,
            ..Limits::default()
        });
    assert!(invalid(&run(
        &t,
        "fs_write",
        json!({"path": "a", "content": "x".repeat(11)}),
        &AutoApprove
    )));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn edit_requires_an_exact_unique_match() {
    let root = scratch("edit");
    std::fs::write(root.join("a.txt"), "one two two three\n").unwrap();
    let t = tools(&root);
    let y = AutoApprove;

    let o = run(
        &t,
        "fs_edit",
        json!({"path": "a.txt", "old": "missing", "new": "x"}),
        &y,
    );
    assert!(!o.ok && o.content.contains("not found"), "{}", o.content);
    let o = run(
        &t,
        "fs_edit",
        json!({"path": "a.txt", "old": "two", "new": "2"}),
        &y,
    );
    assert!(
        !o.ok && o.content.contains("matches 2 times"),
        "{}",
        o.content
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one two two three\n",
        "unchanged after a refused edit"
    );

    assert!(
        run(
            &t,
            "fs_edit",
            json!({"path": "a.txt", "old": "one", "new": "1"}),
            &y
        )
        .ok
    );
    assert!(
        run(
            &t,
            "fs_edit",
            json!({"path": "a.txt", "old": "two", "new": "2", "replace_all": true}),
            &y
        )
        .content
        .contains("2 replacements")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "1 2 2 three\n"
    );
    assert!(invalid(&run(
        &t,
        "fs_edit",
        json!({"path": "a.txt", "old": "", "new": "x"}),
        &y
    )));
    let _ = std::fs::remove_dir_all(&root);
}

// ── shell ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn shell_reports_exit_code_and_output_and_asks_unless_allowlisted() {
    let root = scratch("shell");
    let perms = Permissions {
        shell_allow: vec!["echo".into()],
        ..Permissions::default()
    };
    let t = Tools::new(&root, perms, ProcessRunner::new(&root)).unwrap();
    let ask = Recording::new(Confirmation::Deny);

    let o = run(&t, "shell_run", json!({"command": "echo hi"}), &ask);
    assert!(
        o.ok && o.authorization == Authorization::Allowed
            && o.content.contains("exit code: 0")
            && o.content.contains("hi"),
        "{o:?}"
    );
    assert!(ask.asked().is_empty(), "allow-listed commands do not ask");

    let o = run(
        &t,
        "shell_run",
        json!({"command": "echo hi; touch pwned"}),
        &ask,
    );
    assert!(
        denied(&o) && !root.join("pwned").exists(),
        "an operator defeats the allow-list, the human said no"
    );
    assert_eq!(ask.asked().len(), 1);

    let o = run(
        &t,
        "shell_run",
        json!({"command": "echo boom >&2; exit 3"}),
        &AutoApprove,
    );
    assert!(
        o.ok && o.content.contains("exit code: 3")
            && o.content.contains("--- stderr ---")
            && o.content.contains("boom"),
        "{}",
        o.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shell_runs_in_the_project_with_a_filtered_environment_and_a_timeout() {
    let root = scratch("shell2");
    // SAFETY: set_var is unsafe in Rust 2024 edition; test-only setup.
    unsafe { std::env::set_var("AGON_TOOLS_SECRET", "hunter2") };
    let t = tools(&root);
    let o = run(
        &t,
        "shell_run",
        json!({"command": "pwd -P; echo \"[$AGON_TOOLS_SECRET]\""}),
        &AutoApprove,
    );
    assert!(
        o.content.contains(root.to_str().unwrap())
            && o.content.contains("[]")
            && !o.content.contains("hunter2"),
        "{}",
        o.content
    );
    let o = run(
        &t,
        "shell_run",
        json!({"command": "sleep 30", "timeout_secs": 1}),
        &AutoApprove,
    );
    assert!(
        o.content.contains("killed: timeout after 1s"),
        "{}",
        o.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn long_shell_output_keeps_its_tail() {
    let root = scratch("shell3");
    let t = Tools::new(&root, Permissions::default(), ProcessRunner::new(&root))
        .unwrap()
        .with_limits(Limits {
            max_output_chars: 100,
            ..Limits::default()
        });
    let o = run(
        &t,
        "shell_run",
        json!({"command": "seq 1 5000"}),
        &AutoApprove,
    );
    assert!(
        o.content.contains("characters omitted") && o.content.trim_end().ends_with("5000"),
        "{}",
        o.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_push_is_denied_even_when_everything_else_is_auto_approved() {
    let root = scratch("push");
    let t = tools(&root);
    for cmd in ["git push", "git push origin main", "git -C . push --force"] {
        let o = run(&t, "shell_run", json!({"command": cmd}), &AutoApprove);
        assert!(denied(&o) && o.content.contains("git_push"), "{cmd}: {o:?}");
    }
}

#[test]
fn permissions_can_be_tightened_to_forbid_a_tool_entirely() {
    let root = scratch("tight");
    std::fs::write(root.join("a.txt"), "x").unwrap();
    let perms = Permissions {
        read: Level::Deny,
        ..Permissions::default()
    };
    let t = Tools::new(&root, perms, ProcessRunner::new(&root)).unwrap();
    assert!(denied(&run(
        &t,
        "fs_read",
        json!({"path": "a.txt"}),
        &AutoApprove
    )));
    assert_eq!(t.permissions().level(Action::Read), Level::Deny);
    let _ = std::fs::remove_dir_all(&root);
}

// ── git ────────────────────────────────────────────────────────────────────────────────────

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git_repo(name: &str) -> PathBuf {
    let root = scratch(name);
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@example.com"],
        &["config", "user.name", "Test"],
        &["config", "commit.gpgsign", "false"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
    }
    root
}

#[test]
fn git_status_diff_and_commit_work_and_commit_asks() {
    if !git_available() {
        return;
    }
    let root = git_repo("git");
    std::fs::write(root.join("a.txt"), "v1\n").unwrap();
    let t = tools(&root);

    let o = run(&t, "git_status", json!({}), &DenyAll);
    assert!(o.ok && o.content.contains("a.txt"), "{}", o.content);

    let no = Recording::new(Confirmation::Deny);
    let o = run(
        &t,
        "git_commit",
        json!({"message": "add a", "paths": ["a.txt"]}),
        &no,
    );
    assert!(
        denied(&o) && no.asked()[0].starts_with("git_commit|GitCommit|commit [a.txt]"),
        "{o:?}"
    );

    let o = run(
        &t,
        "git_commit",
        json!({"message": "add a", "paths": ["a.txt"]}),
        &AutoApprove,
    );
    assert!(o.ok, "{}", o.content);
    assert!(
        run(&t, "git_status", json!({}), &DenyAll)
            .content
            .lines()
            .count()
            == 1,
        "clean tree after commit"
    );

    std::fs::write(root.join("a.txt"), "v2\n").unwrap();
    let o = run(&t, "git_diff", json!({"path": "a.txt"}), &DenyAll);
    assert!(
        o.ok && o.content.contains("-v1") && o.content.contains("+v2"),
        "{}",
        o.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_commit_refuses_secrets_missing_paths_and_empty_messages() {
    if !git_available() {
        return;
    }
    let root = git_repo("git2");
    std::fs::write(root.join(".env"), "SECRET=1").unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    let t = tools(&root);
    let y = AutoApprove;
    assert!(
        invalid(&run(
            &t,
            "git_commit",
            json!({"message": "m", "paths": [".env"]}),
            &y
        )),
        "secrets are never staged"
    );
    assert!(
        invalid(&run(
            &t,
            "git_commit",
            json!({"message": "m", "paths": []}),
            &y
        )),
        "nothing is staged implicitly"
    );
    assert!(invalid(&run(
        &t,
        "git_commit",
        json!({"message": "  ", "paths": ["a.txt"]}),
        &y
    )));
    assert!(invalid(&run(
        &t,
        "git_commit",
        json!({"message": "m", "paths": ["../x"]}),
        &y
    )));
    let _ = std::fs::remove_dir_all(&root);
}

// ── contrat avec le modèle ─────────────────────────────────────────────────────────────────

#[test]
fn unknown_tools_and_bad_arguments_are_errors_the_model_can_read() {
    let root = scratch("bad");
    let t = tools(&root);
    let o = run(&t, "rm_rf", json!({}), &AutoApprove);
    assert!(
        invalid(&o) && o.content.contains("unknown tool") && o.content.contains("fs_read"),
        "{}",
        o.content
    );
    let o = t.execute(&ToolCall::new("c", "fs_read", "{not json"), &AutoApprove);
    assert!(
        invalid(&o) && o.content.contains("not valid JSON"),
        "{}",
        o.content
    );
    assert!(
        invalid(&run(&t, "fs_read", json!({}), &AutoApprove)),
        "missing path"
    );
    assert!(invalid(&run(
        &t,
        "fs_read",
        json!({"path": "a", "offset": -1}),
        &AutoApprove
    )));
    assert!(invalid(&run(
        &t,
        "shell_run",
        json!({"command": "  "}),
        &AutoApprove
    )));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn specs_describe_every_tool_with_a_valid_schema_and_can_be_filtered() {
    let root = scratch("specs");
    let t = tools(&root);
    let all = t.specs(None);
    assert_eq!(
        all.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        TOOL_NAMES
    );
    for s in &all {
        assert!(!s.description.is_empty(), "{}", s.name);
        assert_eq!(s.parameters["type"], "object", "{}", s.name);
        for req in s
            .parameters
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            assert!(
                s.parameters["properties"]
                    .get(req.as_str().unwrap())
                    .is_some(),
                "{}: required `{req}` is declared",
                s.name
            );
        }
    }
    let only: std::collections::BTreeSet<String> =
        ["fs_read".to_string(), "shell_run".to_string()].into();
    assert_eq!(
        t.specs(Some(&only))
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["fs_read", "shell_run"]
    );
    let _ = std::fs::remove_dir_all(&root);
}
