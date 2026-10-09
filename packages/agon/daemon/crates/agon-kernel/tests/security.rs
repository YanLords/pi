//! Revue de sécurité : on attaque nos propres garde-fous. Chaque test décrit une attaque réaliste.

use agon_kernel::ProcessRunner;
use agon_kernel::tools::sandbox::{Access, Guard, classify};
use agon_kernel::tools::{Authorization, AutoApprove, DenyAll, ToolOutcome, Tools};
use agon_model::ToolCall;
use agon_policy::Permissions;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agon-sec-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn tools(root: &Path) -> Tools {
    Tools::new(root, Permissions::default(), ProcessRunner::new(root)).unwrap()
}

fn call(t: &Tools, name: &str, args: Value, approve: bool) -> ToolOutcome {
    let c = ToolCall::new("c1", name, args.to_string());
    if approve {
        t.execute(&c, &AutoApprove)
    } else {
        t.execute(&c, &DenyAll)
    }
}

/// Exécute `f` dans un thread : si elle ne rend pas la main en 10 s, le test échoue au lieu de bloquer.
fn within_10s<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the call hung: a special file blocked the tool")
}

// ── A. Contournement par la casse (macOS et Windows ont un système de fichiers insensible) ────

#[test]
fn protected_names_are_matched_case_insensitively() {
    for p in [
        ".GIT/config",
        ".Git/HEAD",
        ".ENV",
        ".Env.Local",
        "Deploy.PEM",
        "ID_RSA",
        "config/.NETRC",
    ] {
        assert_eq!(classify(Path::new(p)), Access::Forbidden, "{p}");
    }
    for p in [".AGON/config.toml", ".Agon/sessions/x.jsonl"] {
        assert_eq!(classify(Path::new(p)), Access::ReadOnly, "{p}");
    }
}

#[test]
fn more_secret_stores_are_forbidden_including_whole_credential_directories() {
    for p in [
        ".git-credentials",
        ".pypirc",
        ".pgpass",
        ".htpasswd",
        "keystore.jks",
        "cert.pfx",
        "prod.tfvars",
        "terraform.tfstate",
        "secrets.yml",
        "secrets.json",
        ".ssh/id_ed25519",
        ".ssh/config",
        ".aws/credentials",
        ".aws/config",
        ".gnupg/pubring.kbx",
        ".kube/config",
        ".docker/config.json",
    ] {
        assert_eq!(classify(Path::new(p)), Access::Forbidden, "{p}");
    }
    for p in [
        "src/secrets_test.rs",
        "docs/ssh.md",
        "aws.rs",
        "keys.rs",
        "tfvars.md",
    ] {
        assert_eq!(
            classify(Path::new(p)),
            Access::Free,
            "{p} must not be a false positive"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn on_a_case_insensitive_filesystem_the_uppercase_alias_of_git_is_still_refused() {
    let root = scratch("case");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(
        root.join(".git/config"),
        "[remote] url = https://token@example.com",
    )
    .unwrap();
    let t = tools(&root);
    for path in [".GIT/config", ".Git/config"] {
        let o = call(&t, "fs_read", json!({"path": path}), true);
        assert!(
            !o.ok && !o.content.contains("token@"),
            "{path} leaked: {}",
            o.content
        );
    }
    std::fs::create_dir_all(root.join(".agon")).unwrap();
    std::fs::write(root.join(".agon/config.toml"), "x").unwrap();
    let o = call(
        &t,
        "fs_write",
        json!({"path": ".AGON/config.toml", "content": "pwned"}),
        true,
    );
    assert!(!o.ok, "{o:?}");
    assert_eq!(
        std::fs::read_to_string(root.join(".agon/config.toml")).unwrap(),
        "x"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── B. Fichiers spéciaux et fichiers énormes ──────────────────────────────────────────────

fn mkfifo(path: &Path) -> bool {
    std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn a_fifo_cannot_hang_reading_editing_or_searching() {
    let root = scratch("fifo");
    if !mkfifo(&root.join("pipe")) {
        return;
    }
    std::fs::write(root.join("normal.txt"), "needle\n").unwrap();
    let r = root.clone();
    let (read, edit, search) = within_10s(move || {
        let t = tools(&r);
        (
            call(&t, "fs_read", json!({"path": "pipe"}), true),
            call(
                &t,
                "fs_edit",
                json!({"path": "pipe", "old": "a", "new": "b"}),
                true,
            ),
            call(&t, "fs_search", json!({"pattern": "needle"}), true),
        )
    });
    assert!(
        !read.ok && read.content.contains("not a regular file"),
        "{}",
        read.content
    );
    assert!(
        !edit.ok && edit.content.contains("not a regular file"),
        "{}",
        edit.content
    );
    assert!(
        search.ok && search.content.contains("normal.txt:1"),
        "the search skips the fifo and still works: {}",
        search.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_huge_file_is_read_in_bounded_memory_and_cannot_be_edited() {
    let root = scratch("huge");
    let f = std::fs::File::create(root.join("huge.txt")).unwrap();
    f.set_len(600 * 1024 * 1024).unwrap(); // fichier creux de 600 Mo
    let r = root.clone();
    let (read, edit) = within_10s(move || {
        let t = tools(&r);
        (
            call(&t, "fs_read", json!({"path": "huge.txt", "limit": 5}), true),
            call(
                &t,
                "fs_edit",
                json!({"path": "huge.txt", "old": "a", "new": "b"}),
                true,
            ),
        )
    });
    assert!(
        read.content.len() < 70_000,
        "bounded output: {} bytes",
        read.content.len()
    );
    assert!(
        !edit.ok && edit.content.contains("too large"),
        "{}",
        edit.content
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── C. Processus orphelins après un timeout ───────────────────────────────────────────────

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn a_timeout_kills_the_whole_process_tree_not_just_the_shell() {
    let root = scratch("orphans");
    let t = tools(&root);
    let o = call(
        &t,
        "shell_run",
        json!({"command": "sleep 60 & echo $! > child.pid; sleep 60", "timeout_secs": 1}),
        true,
    );
    assert!(o.content.contains("killed: timeout"), "{}", o.content);
    let pid = std::fs::read_to_string(root.join("child.pid"))
        .unwrap()
        .trim()
        .to_string();
    let dead = (0..40).any(|_| {
        std::thread::sleep(Duration::from_millis(100));
        !alive(&pid)
    });
    if !dead {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid])
            .status();
    }
    assert!(
        dead,
        "the background child of the timed-out command is still running (pid {pid})"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ── E. Intégrité : le registre de mutations est un état de confiance ──────────────────────

#[test]
fn the_guard_also_covers_the_mutation_registry() {
    let root = scratch("guard-registry");
    std::fs::create_dir_all(root.join(".agon/mutations")).unwrap();
    std::fs::write(root.join(".agon/mutations/registry.toml"), "").unwrap();
    let g = Guard::snapshot(&root);
    std::fs::write(
        root.join(".agon/mutations/registry.toml"),
        "poisoned = true",
    )
    .unwrap();
    assert_eq!(g.violations(), [".agon/mutations/registry.toml"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn authorization_of_a_denied_call_is_never_upgraded_by_the_confirmer() {
    // git push reste refusé même avec un confirmeur qui approuve tout (défense en profondeur).
    let root = scratch("deny");
    let t = tools(&root);
    let o = call(&t, "shell_run", json!({"command": "git push"}), true);
    assert!(matches!(o.authorization, Authorization::Denied { .. }));
    let _ = std::fs::remove_dir_all(&root);
}
