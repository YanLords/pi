//! Outils de l'agent : fichiers, recherche, shell, git (§41), soumis aux permissions (§32).
//!
//! Chaque appel suit le même chemin : **planification** (arguments et chemins validés, avant toute
//! demande à l'humain) → **décision de permission** (déterministe, jamais influencée par Jev) →
//! **confirmation** si nécessaire → exécution. Le résultat est un texte destiné au modèle ; une erreur
//! est un résultat, pas une exception : le modèle peut s'adapter.

pub mod sandbox;

use crate::runner::ProcessRunner;
use agon_model::{ToolCall, ToolSpec};
use agon_policy::{Action, Permission, Permissions};
use regex::Regex;
use sandbox::{Access, SKIPPED_DIRS, classify, relative, resolve};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub use sandbox::Guard;

// ── confirmation ───────────────────────────────────────────────────────────────────────────

pub struct ConfirmRequest<'a> {
    pub tool: &'a str,
    pub action: Action,
    /// Ce qui va être exécuté : commande, chemin, diff résumé.
    pub detail: &'a str,
    pub reason: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    Allow,
    /// Autorise aussi les appels suivants de cette action pendant la session. Honoré pour `write` et
    /// `edit` seulement : un shell ou un commit se confirme à chaque fois.
    AllowSession,
    Deny,
}

pub trait Confirmer {
    fn confirm(&self, request: &ConfirmRequest<'_>) -> Confirmation;
}

/// Approuve tout ce qui demande confirmation (`--yes`). Ne lève jamais un `Deny` de la politique.
pub struct AutoApprove;
impl Confirmer for AutoApprove {
    fn confirm(&self, _: &ConfirmRequest<'_>) -> Confirmation {
        Confirmation::Allow
    }
}

/// Refuse tout (mode non interactif par défaut).
pub struct DenyAll;
impl Confirmer for DenyAll {
    fn confirm(&self, _: &ConfirmRequest<'_>) -> Confirmation {
        Confirmation::Deny
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authorization", rename_all = "snake_case")]
pub enum Authorization {
    Allowed,
    Confirmed,
    ConfirmedForSession,
    /// Refusé par la politique ou par l'humain.
    Denied {
        reason: String,
    },
    /// Arguments ou chemin invalides : rien n'a été demandé ni exécuté.
    Invalid {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutcome {
    pub tool: String,
    pub detail: String,
    pub authorization: Authorization,
    pub ok: bool,
    /// Texte renvoyé au modèle.
    pub content: String,
}

// ── outils ─────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_read_bytes: usize,
    pub default_read_lines: usize,
    pub max_write_bytes: usize,
    pub max_search_matches: usize,
    pub max_list_entries: usize,
    pub max_shell_secs: u64,
    pub max_output_chars: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_read_bytes: 64 * 1024,
            default_read_lines: 400,
            max_write_bytes: 1024 * 1024,
            max_search_matches: 100,
            max_list_entries: 200,
            max_shell_secs: 600,
            max_output_chars: 12_000,
        }
    }
}

/// Au-delà, un fichier n'est lu qu'en partie (`fs_read`) ou refusé (`fs_edit`).
const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

pub const TOOL_NAMES: &[&str] = &[
    "fs_read",
    "fs_list",
    "fs_search",
    "fs_write",
    "fs_edit",
    "shell_run",
    "git_status",
    "git_diff",
    "git_commit",
];

/// Action de permission d'un outil (§32), pour masquer au modèle ceux que la politique interdit.
pub fn action_of(tool: &str) -> Option<Action> {
    Some(match tool {
        "fs_read" | "fs_list" | "git_status" | "git_diff" => Action::Read,
        "fs_search" => Action::Search,
        "fs_write" => Action::Write,
        "fs_edit" => Action::Edit,
        "shell_run" => Action::Shell,
        "git_commit" => Action::GitCommit,
        _ => return None,
    })
}

/// Description courte d'un outil, pour la question de pertinence posée à Jev.
pub fn describe_tool(tool: &str) -> &'static str {
    match tool {
        "fs_read" => "read a file",
        "fs_list" => "list a directory",
        "fs_search" => "search the code base",
        "fs_write" => "create or overwrite a file",
        "fs_edit" => "edit part of a file",
        "shell_run" => "run a shell command such as a build or a test",
        "git_status" => "inspect the git working tree",
        "git_diff" => "inspect uncommitted changes",
        "git_commit" => "commit changes to git",
        _ => "use a tool",
    }
}

/// Outils toujours proposés au modèle, quoi que Jev décide : sans eux l'agent est aveugle.
pub const ESSENTIAL_TOOLS: &[&str] = &["fs_read", "fs_list", "fs_search"];

enum Req {
    Read {
        path: PathBuf,
        offset: usize,
        limit: usize,
    },
    List {
        path: PathBuf,
    },
    Search {
        re: Regex,
        path: PathBuf,
        glob: Option<String>,
    },
    Write {
        path: PathBuf,
        content: String,
    },
    Edit {
        path: PathBuf,
        old: String,
        new: String,
        all: bool,
    },
    Shell {
        command: String,
        timeout: u64,
    },
    GitStatus,
    GitDiff {
        path: Option<PathBuf>,
        staged: bool,
    },
    GitCommit {
        message: String,
        paths: Vec<PathBuf>,
    },
}

struct Plan {
    tool: &'static str,
    action: Action,
    detail: String,
    req: Req,
}

pub struct Tools {
    root: PathBuf,
    permissions: Permissions,
    shell: ProcessRunner,
    limits: Limits,
    session_allowed: Mutex<BTreeSet<Action>>,
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or non-string argument `{key}`"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn opt_uint(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a non-negative integer")),
    }
}

fn preview(s: &str, n: usize) -> String {
    let flat = s.replace('\n', "⏎");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}…", flat.chars().take(n).collect::<String>())
    }
}

/// Garde la fin d'un texte (les erreurs de compilation et les résumés sont à la fin).
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    format!(
        "[… {} characters omitted …]\n{}",
        n - max,
        s.chars().skip(n - max).collect::<String>()
    )
}

fn head(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    format!(
        "{}\n[… {} characters omitted …]",
        s.chars().take(max).collect::<String>(),
        n - max
    )
}

/// `*` (n'importe quoi) et `?` (un caractère), sur un nom de fichier.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    go(
        &pattern.chars().collect::<Vec<_>>(),
        &name.chars().collect::<Vec<_>>(),
    )
}

impl Tools {
    pub fn new(
        root: &Path,
        permissions: Permissions,
        mut shell: ProcessRunner,
    ) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("project root {}: {e}", root.display()))?;
        shell.cwd = root.clone();
        Ok(Tools {
            root,
            permissions,
            shell,
            limits: Limits::default(),
            session_allowed: Mutex::new(BTreeSet::new()),
        })
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    /// Description des outils, pour le modèle. `enabled` filtre par nom (`None` = tous).
    pub fn specs(&self, enabled: Option<&BTreeSet<String>>) -> Vec<ToolSpec> {
        let spec = |name: &str, description: &str, parameters: Value| ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
        };
        let s = |d: &str| json!({"type": "string", "description": d});
        let all = vec![
            spec(
                "fs_read",
                "Read a text file with line numbers. Use offset/limit to page through large files.",
                json!({"type": "object", "properties": {"path": s("File path relative to the project root"),
                    "offset": {"type": "integer", "description": "First line to read (1-based)"},
                    "limit": {"type": "integer", "description": "Maximum number of lines"}}, "required": ["path"]}),
            ),
            spec(
                "fs_list",
                "List the entries of a directory (directories end with /).",
                json!({"type": "object", "properties": {"path": s("Directory, default '.'")}}),
            ),
            spec(
                "fs_search",
                "Search files for a regular expression. Skips .git, target, node_modules and secrets.",
                json!({"type": "object", "properties": {"pattern": s("Regular expression (Rust regex syntax)"),
                    "path": s("Directory or file to search, default '.'"),
                    "glob": s("File name filter such as '*.rs'")}, "required": ["pattern"]}),
            ),
            spec(
                "fs_write",
                "Create or overwrite a file with the given content. Requires confirmation.",
                json!({"type": "object", "properties": {"path": s("File path"), "content": s("Full file content")}, "required": ["path", "content"]}),
            ),
            spec(
                "fs_edit",
                "Replace exact text in a file. `old` must match exactly once unless replace_all is true. Requires confirmation.",
                json!({"type": "object", "properties": {"path": s("File path"), "old": s("Exact text to replace"), "new": s("Replacement text"),
                    "replace_all": {"type": "boolean"}}, "required": ["path", "old", "new"]}),
            ),
            spec(
                "shell_run",
                "Run a shell command in the project root and return its exit code and output. Requires confirmation unless allow-listed.",
                json!({"type": "object", "properties": {"command": s("The command line"),
                    "timeout_secs": {"type": "integer", "description": "Timeout in seconds (default 120)"}}, "required": ["command"]}),
            ),
            spec(
                "git_status",
                "Show the working tree status.",
                json!({"type": "object", "properties": {}}),
            ),
            spec(
                "git_diff",
                "Show uncommitted changes, optionally for one path or only staged ones.",
                json!({"type": "object", "properties": {"path": s("Optional path"), "staged": {"type": "boolean"}}}),
            ),
            spec(
                "git_commit",
                "Stage the listed paths and commit them. Requires confirmation. Pushing is not possible.",
                json!({"type": "object", "properties": {"message": s("Commit message"),
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Files to stage and commit"}}, "required": ["message", "paths"]}),
            ),
        ];
        all.into_iter()
            .filter(|t| enabled.is_none_or(|e| e.contains(&t.name)))
            .collect()
    }

    // ── planification ──────────────────────────────────────────────────────────────────────

    fn path(&self, input: &str, write: bool) -> Result<PathBuf, String> {
        let p = resolve(&self.root, input)?;
        match (classify(relative(&self.root, &p)), write) {
            (Access::Forbidden, _) => Err(format!(
                "`{input}` is not accessible (secrets and git internals are off limits)"
            )),
            (Access::ReadOnly, true) => Err(format!(
                "`{input}` is protected: Agon's own state cannot be modified"
            )),
            _ => Ok(p),
        }
    }

    fn rel(&self, p: &Path) -> String {
        let rel = relative(&self.root, p).display().to_string();
        if rel.is_empty() { ".".to_string() } else { rel }
    }

    fn plan(&self, call: &ToolCall) -> Result<Plan, String> {
        let args = call.args()?;
        let name = call.function.name.as_str();
        match name {
            "fs_read" => {
                let path = self.path(arg(&args, "path")?, false)?;
                let offset = opt_uint(&args, "offset")?.unwrap_or(1).max(1) as usize;
                let limit = opt_uint(&args, "limit")?
                    .map(|l| l as usize)
                    .unwrap_or(self.limits.default_read_lines)
                    .max(1);
                Ok(Plan {
                    tool: "fs_read",
                    action: Action::Read,
                    detail: self.rel(&path),
                    req: Req::Read {
                        path,
                        offset,
                        limit,
                    },
                })
            }
            "fs_list" => {
                let path = self.path(opt_str(&args, "path").unwrap_or("."), false)?;
                Ok(Plan {
                    tool: "fs_list",
                    action: Action::Read,
                    detail: self.rel(&path),
                    req: Req::List { path },
                })
            }
            "fs_search" => {
                let pattern = arg(&args, "pattern")?;
                let re =
                    Regex::new(pattern).map_err(|e| format!("invalid regular expression: {e}"))?;
                let path = self.path(opt_str(&args, "path").unwrap_or("."), false)?;
                Ok(Plan {
                    tool: "fs_search",
                    action: Action::Search,
                    detail: format!("{pattern} in {}", self.rel(&path)),
                    req: Req::Search {
                        re,
                        path,
                        glob: opt_str(&args, "glob").map(str::to_owned),
                    },
                })
            }
            "fs_write" => {
                let path = self.path(arg(&args, "path")?, true)?;
                let content = arg(&args, "content")?.to_owned();
                if content.len() > self.limits.max_write_bytes {
                    return Err(format!(
                        "content is {} bytes, the limit is {}",
                        content.len(),
                        self.limits.max_write_bytes
                    ));
                }
                let kind = if path.exists() {
                    "overwrite"
                } else {
                    "new file"
                };
                Ok(Plan {
                    tool: "fs_write",
                    action: Action::Write,
                    detail: format!("{} ({} bytes, {kind})", self.rel(&path), content.len()),
                    req: Req::Write { path, content },
                })
            }
            "fs_edit" => {
                let path = self.path(arg(&args, "path")?, true)?;
                let (old, new) = (arg(&args, "old")?.to_owned(), arg(&args, "new")?.to_owned());
                if old.is_empty() {
                    return Err("`old` must not be empty".into());
                }
                let all = args
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Ok(Plan {
                    tool: "fs_edit",
                    action: Action::Edit,
                    detail: format!(
                        "{}: replace `{}` → `{}`",
                        self.rel(&path),
                        preview(&old, 60),
                        preview(&new, 60)
                    ),
                    req: Req::Edit {
                        path,
                        old,
                        new,
                        all,
                    },
                })
            }
            "shell_run" => {
                let command = arg(&args, "command")?.trim().to_owned();
                if command.is_empty() {
                    return Err("`command` must not be empty".into());
                }
                let timeout = opt_uint(&args, "timeout_secs")?
                    .unwrap_or(120)
                    .clamp(1, self.limits.max_shell_secs);
                Ok(Plan {
                    tool: "shell_run",
                    action: Action::Shell,
                    detail: command.clone(),
                    req: Req::Shell { command, timeout },
                })
            }
            "git_status" => Ok(Plan {
                tool: "git_status",
                action: Action::Read,
                detail: "git status".into(),
                req: Req::GitStatus,
            }),
            "git_diff" => {
                let path = opt_str(&args, "path")
                    .map(|p| self.path(p, false))
                    .transpose()?;
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                Ok(Plan {
                    tool: "git_diff",
                    action: Action::Read,
                    detail: format!("git diff{}", if staged { " --staged" } else { "" }),
                    req: Req::GitDiff { path, staged },
                })
            }
            "git_commit" => {
                let message = arg(&args, "message")?.trim().to_owned();
                if message.is_empty() || message.len() > 2000 {
                    return Err("`message` must be between 1 and 2000 characters".into());
                }
                let list = args
                    .get("paths")
                    .and_then(Value::as_array)
                    .ok_or("`paths` must be a list of file paths")?;
                if list.is_empty() {
                    return Err(
                        "`paths` must list the files to commit (nothing is staged implicitly)"
                            .into(),
                    );
                }
                let paths = list
                    .iter()
                    .map(|p| {
                        p.as_str()
                            .ok_or_else(|| "`paths` must contain strings".to_string())
                            .and_then(|p| self.path(p, true))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let names: Vec<String> = paths.iter().map(|p| self.rel(p)).collect();
                Ok(Plan {
                    tool: "git_commit",
                    action: Action::GitCommit,
                    detail: format!("commit [{}]: {}", names.join(", "), preview(&message, 80)),
                    req: Req::GitCommit { message, paths },
                })
            }
            other => Err(format!(
                "unknown tool `{other}`; available: {}",
                TOOL_NAMES.join(", ")
            )),
        }
    }

    // ── exécution d'un appel ───────────────────────────────────────────────────────────────

    pub fn execute(&self, call: &ToolCall, confirmer: &dyn Confirmer) -> ToolOutcome {
        let tool = call.function.name.clone();
        let plan = match self.plan(call) {
            Ok(p) => p,
            Err(reason) => {
                return ToolOutcome {
                    tool,
                    detail: String::new(),
                    authorization: Authorization::Invalid {
                        reason: reason.clone(),
                    },
                    ok: false,
                    content: format!("error: {reason}"),
                };
            }
        };
        let detail = plan.detail.clone();
        let denied = |reason: String| ToolOutcome {
            tool: plan.tool.into(),
            detail: detail.clone(),
            authorization: Authorization::Denied {
                reason: reason.clone(),
            },
            ok: false,
            content: format!(
                "permission denied: {reason}. Do not retry the same action; choose another approach or explain what you need."
            ),
        };

        let authorization = match self.permissions.decide(plan.action, &plan.detail) {
            Permission::Allow => Authorization::Allowed,
            Permission::Deny { reason } => return denied(reason),
            Permission::Confirm { reason } => {
                let remembered = matches!(plan.action, Action::Write | Action::Edit)
                    && self.session_allowed.lock().unwrap().contains(&plan.action);
                if remembered {
                    Authorization::ConfirmedForSession
                } else {
                    let answer = confirmer.confirm(&ConfirmRequest {
                        tool: plan.tool,
                        action: plan.action,
                        detail: &plan.detail,
                        reason: &reason,
                    });
                    match answer {
                        Confirmation::Deny => return denied("declined by the user".into()),
                        Confirmation::Allow => Authorization::Confirmed,
                        Confirmation::AllowSession
                            if matches!(plan.action, Action::Write | Action::Edit) =>
                        {
                            self.session_allowed.lock().unwrap().insert(plan.action);
                            Authorization::ConfirmedForSession
                        }
                        Confirmation::AllowSession => Authorization::Confirmed,
                    }
                }
            }
        };

        let (ok, content) = match self.run(plan.req) {
            Ok(text) => (true, text),
            Err(e) => (false, format!("error: {e}")),
        };
        ToolOutcome {
            tool: plan.tool.into(),
            detail,
            authorization,
            ok,
            content,
        }
    }

    fn run(&self, req: Req) -> Result<String, String> {
        match req {
            Req::Read {
                path,
                offset,
                limit,
            } => self.read(&path, offset, limit),
            Req::List { path } => self.list(&path),
            Req::Search { re, path, glob } => self.search(&re, &path, glob.as_deref()),
            Req::Write { path, content } => self.write(&path, &content),
            Req::Edit {
                path,
                old,
                new,
                all,
            } => self.edit(&path, &old, &new, all),
            Req::Shell { command, timeout } => Ok(self.shell(&command, timeout)),
            Req::GitStatus => self.git(&["status", "--short", "--branch"]),
            Req::GitDiff { path, staged } => {
                let mut args: Vec<String> = vec!["diff".into(), "--no-color".into()];
                if staged {
                    args.push("--staged".into());
                }
                if let Some(p) = path {
                    args.extend(["--".into(), self.rel(&p)]);
                }
                self.git(&args.iter().map(String::as_str).collect::<Vec<_>>())
                    .map(|d| head(&d, self.limits.max_output_chars * 2))
            }
            Req::GitCommit { message, paths } => {
                let mut add: Vec<String> = vec!["add".into(), "--".into()];
                add.extend(paths.iter().map(|p| self.rel(p)));
                self.git(&add.iter().map(String::as_str).collect::<Vec<_>>())?;
                self.git(&["commit", "-m", &message])
            }
        }
    }

    /// Lit au plus `cap` octets d'un **fichier ordinaire**. Un FIFO, un socket ou un périphérique
    /// feraient bloquer la lecture indéfiniment : le type est vérifié avant l'ouverture. La mémoire
    /// reste bornée quelle que soit la taille du fichier.
    fn read_capped(&self, path: &Path, cap: usize) -> Result<(Vec<u8>, bool), String> {
        use std::io::Read;
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", self.rel(path)))?;
        if !meta.is_file() {
            return Err(format!("{} is not a regular file", self.rel(path)));
        }
        let mut buf = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(cap as u64 + 1).read_to_end(&mut buf))
            .map_err(|e| format!("{}: {e}", self.rel(path)))?;
        let truncated = buf.len() > cap;
        buf.truncate(cap);
        Ok((buf, truncated))
    }

    fn read(&self, path: &Path, offset: usize, limit: usize) -> Result<String, String> {
        let (bytes, file_truncated) = self.read_capped(path, MAX_FILE_BYTES)?;
        if bytes.iter().take(8000).any(|b| *b == 0) {
            return Err(format!("{} looks like a binary file", self.rel(path)));
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return Ok("(empty file)".into());
        }
        if offset > lines.len() {
            return Err(format!(
                "offset {offset} is past the end of the file ({} lines)",
                lines.len()
            ));
        }
        let (mut out, mut used, mut last) = (String::new(), 0usize, offset - 1);
        for (i, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            let numbered = format!("{:>5}\t{line}\n", i + 1);
            if used + numbered.len() > self.limits.max_read_bytes && used > 0 {
                break;
            }
            used += numbered.len();
            out.push_str(&numbered);
            last = i + 1;
        }
        if last < lines.len() {
            out.push_str(&format!(
                "[showing lines {offset}-{last} of {}; use offset={} to continue]\n",
                lines.len(),
                last + 1
            ));
        }
        if file_truncated {
            out.push_str(&format!(
                "[the file is larger than {} MiB: only the beginning can be read]\n",
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }
        Ok(out)
    }

    fn list(&self, path: &Path) -> Result<String, String> {
        let mut entries: Vec<String> = std::fs::read_dir(path)
            .map_err(|e| format!("{}: {e}", self.rel(path)))?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != ".git")
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        entries.sort();
        let total = entries.len();
        entries.truncate(self.limits.max_list_entries);
        let mut out = entries.join("\n");
        if total > entries.len() {
            out.push_str(&format!("\n[+{} more entries]", total - entries.len()));
        }
        Ok(if out.is_empty() {
            "(empty directory)".into()
        } else {
            out
        })
    }

    fn search(&self, re: &Regex, start: &Path, glob: Option<&str>) -> Result<String, String> {
        let (mut matches, mut scanned, mut stack, mut truncated) =
            (Vec::new(), 0usize, vec![start.to_path_buf()], false);
        'walk: while let Some(p) = stack.pop() {
            let meta = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue; // ni boucles ni évasions
            }
            let rel = relative(&self.root, &p).to_path_buf();
            if classify(&rel) == Access::Forbidden {
                continue;
            }
            if meta.is_dir() {
                if p != start
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| SKIPPED_DIRS.contains(&n))
                {
                    continue;
                }
                if let Ok(rd) = std::fs::read_dir(&p) {
                    let mut kids: Vec<PathBuf> =
                        rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                    kids.sort();
                    stack.extend(kids.into_iter().rev());
                }
                continue;
            }
            // Ni FIFO, ni socket, ni périphérique : leur lecture pourrait bloquer.
            if !meta.is_file()
                || meta.len() > 1024 * 1024
                || glob.is_some_and(|g| {
                    !glob_match(g, &p.file_name().unwrap_or_default().to_string_lossy())
                })
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            if bytes.iter().take(1024).any(|b| *b == 0) {
                continue;
            }
            scanned += 1;
            for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                if re.is_match(line) {
                    if matches.len() >= self.limits.max_search_matches {
                        truncated = true;
                        break 'walk;
                    }
                    matches.push(format!(
                        "{}:{}: {}",
                        rel.display(),
                        i + 1,
                        preview(line.trim(), 200)
                    ));
                }
            }
        }
        let mut out = matches.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "[{} match(es){} in {scanned} file(s) scanned]",
            matches.len(),
            if truncated { ", truncated" } else { "" }
        ));
        Ok(out)
    }

    fn write(&self, path: &Path, content: &str) -> Result<String, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", self.rel(parent)))?;
        }
        let existed = path.exists();
        std::fs::write(path, content).map_err(|e| format!("{}: {e}", self.rel(path)))?;
        Ok(format!(
            "{} {} ({} bytes)",
            if existed { "overwrote" } else { "created" },
            self.rel(path),
            content.len()
        ))
    }

    fn edit(&self, path: &Path, old: &str, new: &str, all: bool) -> Result<String, String> {
        let (bytes, truncated) = self.read_capped(path, MAX_FILE_BYTES)?;
        if truncated {
            return Err(format!(
                "{} is too large to edit (over {} MiB)",
                self.rel(path),
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not valid UTF-8 text", self.rel(path)))?;
        let count = text.matches(old).count();
        match (count, all) {
            (0, _) => Err(
                "`old` was not found in the file; read the file again and copy the exact text"
                    .into(),
            ),
            (n, false) if n > 1 => Err(format!(
                "`old` matches {n} times; add surrounding context to make it unique, or set replace_all"
            )),
            _ => {
                let updated = if all {
                    text.replace(old, new)
                } else {
                    text.replacen(old, new, 1)
                };
                std::fs::write(path, updated).map_err(|e| format!("{}: {e}", self.rel(path)))?;
                Ok(format!(
                    "edited {} ({count} replacement{})",
                    self.rel(path),
                    if count == 1 { "" } else { "s" }
                ))
            }
        }
    }

    fn shell(&self, command: &str, timeout: u64) -> String {
        let runner = self
            .shell
            .clone()
            .with_timeout(Duration::from_secs(timeout));
        let o = runner.exec("shell_run".into(), command);
        let status = match (o.timed_out, o.exit_code) {
            (true, _) => format!("killed: timeout after {timeout}s"),
            (_, Some(c)) => format!("exit code: {c}"),
            (_, None) => "killed by a signal".into(),
        };
        let cap = self.limits.max_output_chars;
        let mut out = status;
        if !o.stdout.trim().is_empty() {
            out.push_str(&format!(
                "\n--- stdout ---\n{}",
                tail(o.stdout.trim_end(), cap)
            ));
        }
        if !o.stderr.trim().is_empty() {
            out.push_str(&format!(
                "\n--- stderr ---\n{}",
                tail(o.stderr.trim_end(), cap)
            ));
        }
        out
    }

    fn git(&self, args: &[&str]) -> Result<String, String> {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args)
            .current_dir(&self.root)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat");
        for k in ["PATH", "HOME", "LANG"] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        let o = cmd.output().map_err(|e| format!("cannot run git: {e}"))?;
        let (out, err) = (
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr),
        );
        if o.status.success() {
            Ok(if out.trim().is_empty() {
                if err.trim().is_empty() {
                    "(no output)".into()
                } else {
                    err.trim().to_string()
                }
            } else {
                out.trim_end().to_string()
            })
        } else {
            Err(format!(
                "git {} failed ({}): {}",
                args.first().unwrap_or(&""),
                o.status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                tail(err.trim(), 2000)
            ))
        }
    }
}
