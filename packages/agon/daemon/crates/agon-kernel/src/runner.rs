//! Exécution réelle des checks : `sh -c <command>` dans un répertoire donné, avec un
//! environnement filtré (liste blanche), un timeout et une sortie plafonnée (§39.1, V0).

use agon_core::{Check, CheckId};
use agon_verify::{CheckRunner, Observation};
use std::collections::BTreeSet;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Groupes de processus en cours d'exécution : de quoi tout arrêter sur interruption.
static ACTIVE: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn active() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tue un groupe de processus entier (le shell **et** ses enfants, y compris ceux lancés en arrière-plan).
fn kill_group(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill")
        .args(["-s", "KILL", "--"])
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(unix))]
    let _ = pid;
}

/// Tue toutes les commandes en cours (Ctrl-C) : chacune vit dans son propre groupe de processus,
/// donc le terminal ne leur transmet pas l'interruption.
pub fn kill_active_processes() {
    let pids: Vec<u32> = active().iter().copied().collect();
    for pid in pids {
        kill_group(pid);
    }
}

/// Variables d'environnement transmises par défaut aux checks.
pub const DEFAULT_ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "LANG", "TMPDIR"];

#[derive(Clone, Debug)]
pub struct ProcessRunner {
    pub cwd: PathBuf,
    pub env_allowlist: Vec<String>,
    pub timeout: Duration,
    /// Nombre maximal d'octets conservés par flux (stdout, stderr) ; le reste est drainé et ignoré.
    pub max_output: usize,
}

impl ProcessRunner {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        ProcessRunner {
            cwd: cwd.into(),
            env_allowlist: DEFAULT_ENV_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
            timeout: Duration::from_secs(300),
            max_output: 64 * 1024,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_env(mut self, name: impl Into<String>) -> Self {
        self.env_allowlist.push(name.into());
        self
    }

    fn observation(
        check: &CheckId,
        exit_code: Option<i32>,
        out: String,
        err: String,
        timed_out: bool,
    ) -> Observation {
        Observation {
            check: check.clone(),
            exit_code,
            stdout: out,
            stderr: err,
            timed_out,
        }
    }
}

/// Lit un flux dans un thread : conserve au plus `cap` octets — **la fin** du flux, là où se
/// trouvent les erreurs de compilation et les résumés — et draine le reste pour ne jamais bloquer
/// le processus fils sur un pipe plein.
fn capture<R: Read + Send + 'static>(mut stream: R, cap: usize) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut kept, mut chunk) = (Vec::new(), [0u8; 8192]);
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    kept.extend_from_slice(&chunk[..n]);
                    if kept.len() > cap * 2 {
                        kept.drain(..kept.len() - cap);
                    }
                }
            }
        }
        if kept.len() > cap {
            kept.drain(..kept.len() - cap);
        }
        let _ = tx.send(String::from_utf8_lossy(&kept).into_owned());
    });
    rx
}

impl ProcessRunner {
    /// Exécute une commande shell et renvoie son observation, étiquetée par `id`.
    pub fn exec(&self, id: CheckId, command: &str) -> Observation {
        let check_id = id;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Propre groupe de processus : un timeout peut alors tuer tout l'arbre, pas seulement le shell.
        #[cfg(unix)]
        cmd.process_group(0);
        for name in &self.env_allowlist {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Self::observation(
                    &check_id,
                    None,
                    String::new(),
                    format!("agon: failed to spawn: {e}"),
                    false,
                );
            }
        };
        let pid = child.id();
        active().insert(pid);
        let out = capture(child.stdout.take().expect("piped"), self.max_output);
        let err = capture(child.stderr.take().expect("piped"), self.max_output);

        let deadline = Instant::now() + self.timeout;
        let (status, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (Some(status), false),
                Ok(None) if Instant::now() >= deadline => {
                    kill_group(pid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break (None, true);
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break (None, false),
            }
        };

        active().remove(&pid);

        // Un petit-fils qui garde le pipe ouvert ne doit pas bloquer Agon : délai borné.
        let grace = Duration::from_millis(500);
        let stdout = out.recv_timeout(grace).unwrap_or_default();
        let stderr = err.recv_timeout(grace).unwrap_or_default();
        Self::observation(
            &check_id,
            status.and_then(|s| s.code()),
            stdout,
            stderr,
            timed_out,
        )
    }
}

impl CheckRunner for ProcessRunner {
    fn run(&self, check: &Check) -> Observation {
        self.exec(check.id.clone(), &check.command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(cmd: &str) -> Check {
        Check {
            id: "verify.test".into(),
            command: cmd.into(),
            dependencies: vec![],
            success: "exit_code == 0".into(),
            outputs: vec![],
            version: 1,
        }
    }

    fn runner() -> ProcessRunner {
        ProcessRunner::new(std::env::temp_dir())
    }

    #[test]
    fn captures_exit_code_and_both_streams() {
        let o = runner().run(&check("echo out; echo err >&2; exit 3"));
        assert_eq!(o.exit_code, Some(3));
        assert_eq!(o.stdout.trim(), "out");
        assert_eq!(o.stderr.trim(), "err");
        assert!(!o.timed_out);
    }

    #[test]
    fn success_is_exit_zero() {
        assert_eq!(runner().run(&check("true")).exit_code, Some(0));
    }

    #[test]
    fn timeout_kills_the_process() {
        let started = Instant::now();
        let o = runner()
            .with_timeout(Duration::from_millis(200))
            .run(&check("sleep 30"));
        assert!(o.timed_out);
        assert_eq!(o.exit_code, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn environment_is_filtered() {
        // SAFETY: set_var is unsafe in Rust 2024 edition; test-only setup.
        unsafe { std::env::set_var("AGON_TEST_SECRET", "hunter2") };
        let o = runner().run(&check("echo \"[${AGON_TEST_SECRET}]\""));
        assert_eq!(
            o.stdout.trim(),
            "[]",
            "non-allowlisted variables must not leak"
        );
        let o = runner()
            .with_env("AGON_TEST_SECRET")
            .run(&check("echo \"[${AGON_TEST_SECRET}]\""));
        assert_eq!(o.stdout.trim(), "[hunter2]");
    }

    #[test]
    fn runs_in_the_configured_directory() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let o = ProcessRunner::new(&dir).run(&check("pwd -P"));
        assert_eq!(o.stdout.trim(), dir.to_str().unwrap());
    }

    #[test]
    fn output_is_capped_without_blocking() {
        let mut r = runner();
        r.max_output = 1024;
        let o = r.run(&check("yes x | head -c 1000000"));
        assert_eq!(o.exit_code, Some(0));
        assert_eq!(o.stdout.len(), 1024);
    }

    #[test]
    fn a_capped_output_keeps_the_end_of_the_stream() {
        let mut r = runner();
        r.max_output = 64;
        let o = r.run(&check("seq 1 100000"));
        assert_eq!(o.exit_code, Some(0));
        assert!(
            o.stdout.trim_end().ends_with("100000"),
            "the tail must survive: {:?}",
            o.stdout
        );
        assert!(o.stdout.len() <= 64);
    }

    #[test]
    fn interrupting_kills_every_running_command() {
        let started = Instant::now();
        let handle = thread::spawn(|| {
            ProcessRunner::new(std::env::temp_dir())
                .with_timeout(Duration::from_secs(60))
                .run(&check("sleep 30"))
        });
        thread::sleep(Duration::from_millis(400));
        kill_active_processes();
        let o = handle.join().unwrap();
        assert_eq!(o.exit_code, None, "killed by a signal");
        assert!(!o.timed_out && started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn spawn_failure_is_reported_not_panicked() {
        let r = ProcessRunner::new("/definitely/not/a/dir");
        let o = r.run(&check("true"));
        assert_eq!(o.exit_code, None);
        assert!(o.stderr.contains("failed to spawn"));
    }
}
