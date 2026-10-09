//! Événements typés (§34) et session append-only (§33).
//!
//! Chaque événement porte l'identifiant de la Form active, ce qui permet de répondre à
//! « quelle était exactement la configuration d'Agon lorsqu'il a effectué cette action ? ».
//! Les Forms elles-mêmes sont enregistrées dans `form.committed`, et les échanges Jev complets
//! dans `decision.completed` : une session suffit à inspecter, exporter et rejouer.

use agon_core::{CheckId, Form, FormId, Mutation, MutationId, Signature, SignatureId};
use agon_model::Exchange;
use agon_verify::Evidence;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// À quel moment du protocole un check a été exécuté.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Première exécution sur la Form courante.
    Initial,
    /// Recheck sur la même Form pour confirmer l'échec (§16).
    Recheck,
    /// Exécution sur la Form candidate.
    Candidate,
    /// Recheck du parent après succès de la candidate (§16.1).
    ParentRecheck,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventKind {
    #[serde(rename = "session.started")]
    SessionStarted { agon: String },
    #[serde(rename = "form.committed")]
    FormCommitted {
        snapshot: Box<Form>,
        parent: Option<FormId>,
    },
    #[serde(rename = "verification.started")]
    VerificationStarted { check: CheckId, phase: Phase },
    #[serde(rename = "verification.passed")]
    VerificationPassed { phase: Phase, evidence: Evidence },
    #[serde(rename = "verification.failed")]
    VerificationFailed { phase: Phase, evidence: Evidence },
    #[serde(rename = "signature.detected")]
    SignatureDetected {
        id: SignatureId,
        signature: Signature,
    },
    #[serde(rename = "decision.requested")]
    DecisionRequested {
        decision_id: String,
        template: agon_core::Digest,
        request: agon_core::Digest,
        candidates: Vec<String>,
    },
    #[serde(rename = "decision.completed")]
    DecisionCompleted {
        decision_id: String,
        jev_model_requested: String,
        jev_model_answered: String,
        /// La version qui a répondu diffère de celle du lockfile (§11.2).
        model_drift: bool,
        exchange: Box<Exchange>,
        outcome: String,
    },
    #[serde(rename = "decision.escalated")]
    DecisionEscalated { decision_id: String, detail: String },
    #[serde(rename = "decision.unavailable")]
    DecisionUnavailable { decision_id: String, detail: String },
    #[serde(rename = "mutation.proposed")]
    MutationProposed { mutation: Box<Mutation> },
    #[serde(rename = "mutation.approved")]
    MutationApproved { mutation: MutationId },
    #[serde(rename = "mutation.applied")]
    MutationApplied {
        mutation: MutationId,
        parent: FormId,
        child: FormId,
    },
    #[serde(rename = "mutation.rejected")]
    MutationRejected {
        mutation: MutationId,
        reasons: Vec<String>,
    },
    #[serde(rename = "mutation.rollback")]
    MutationRollback {
        mutation: MutationId,
        from: FormId,
        to: FormId,
    },
    #[serde(rename = "mutation.validated")]
    MutationValidated { mutation: MutationId },
    #[serde(rename = "mutation.unproven")]
    MutationUnproven { mutation: MutationId },
    #[serde(rename = "memory.created")]
    MemoryCreated {
        signature: SignatureId,
        mutation: MutationId,
    },
    #[serde(rename = "task.completed")]
    TaskCompleted { outcome: crate::Outcome },
    /// Message de l'utilisateur dans une conversation (mode `chat` ou `plan`).
    #[serde(rename = "chat.user")]
    ChatUser { mode: crate::Mode, text: String },
    /// Le modèle a proposé un plan (`# Plan`) : c'est ce que `/go` mettra en œuvre.
    #[serde(rename = "plan.proposed")]
    PlanProposed { plan: String },
    #[serde(rename = "agent.started")]
    AgentStarted { prompt: String },
    /// Niveau de modèle et outils retenus ; `source` : `jev` (décidé) ou `default` (repli).
    #[serde(rename = "agent.configured")]
    AgentConfigured {
        tier: String,
        model: String,
        tools: Vec<String>,
        source: String,
    },
    #[serde(rename = "agent.message")]
    AgentMessage {
        text: String,
        tool_calls: Vec<String>,
        model: String,
        prompt_tokens: u64,
        completion_tokens: u64,
        cost: Option<f64>,
    },
    #[serde(rename = "tool.called")]
    ToolCalled {
        tool: String,
        detail: String,
        authorization: crate::tools::Authorization,
        ok: bool,
        preview: String,
    },
    #[serde(rename = "agent.completed")]
    AgentCompleted { outcome: crate::TaskOutcome },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts_ms: u64,
    /// Form active au moment de l'événement.
    pub form: Option<FormId>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("session line {line}: {source}")]
    Corrupt {
        line: usize,
        source: serde_json::Error,
    },
}

/// Observateur d'événements (voir [`Session::set_sink`]).
pub type Sink = Box<dyn Fn(&Event) + Send>;

/// Journal d'événements en ajout seul : en mémoire, et en JSONL sur disque si un fichier est fourni.
/// Un événement écrit n'est jamais modifié ni supprimé.
pub struct Session {
    id: String,
    events: Vec<Event>,
    file: Option<File>,
    path: Option<PathBuf>,
    /// Observateur appelé après chaque événement enregistré (TUI, streaming). Il ne peut rien modifier.
    sink: Option<Sink>,
}

impl Session {
    pub fn in_memory(id: impl Into<String>) -> Self {
        Session {
            id: id.into(),
            events: Vec::new(),
            file: None,
            path: None,
            sink: None,
        }
    }

    /// Crée `<dir>/<id>.jsonl`. Refuse d'écraser une session existante.
    pub fn create(dir: &Path, id: impl Into<String>) -> Result<Self, SessionError> {
        let id = id.into();
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{id}.jsonl"));
        let mut options = OpenOptions::new();
        options.create_new(true).append(true);
        // Le journal contient le texte des tâches et des sorties de commandes : lisible par le seul utilisateur.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(&path)?;
        Ok(Session {
            id,
            events: Vec::new(),
            file: Some(file),
            path: Some(path),
            sink: None,
        })
    }

    /// Relit une session pour l'inspecter ou la rejouer ; on peut y ajouter à la suite (`resume`).
    pub fn load(path: &Path) -> Result<Self, SessionError> {
        let mut events = Vec::new();
        for (i, line) in BufReader::new(File::open(path)?).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            events.push(
                serde_json::from_str(&line).map_err(|source| SessionError::Corrupt {
                    line: i + 1,
                    source,
                })?,
            );
        }
        let file = OpenOptions::new().append(true).open(path)?;
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        Ok(Session {
            id,
            events,
            file: Some(file),
            path: Some(path.to_path_buf()),
            sink: None,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Branche un observateur : il reçoit chaque événement *après* son écriture (journal d'abord).
    pub fn set_sink(&mut self, sink: impl Fn(&Event) + Send + 'static) {
        self.sink = Some(Box::new(sink));
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn record(
        &mut self,
        form: Option<FormId>,
        kind: EventKind,
    ) -> Result<&Event, SessionError> {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let event = Event {
            seq: self.events.len() as u64,
            ts_ms,
            form,
            kind,
        };
        if let Some(f) = self.file.as_mut() {
            let mut line = serde_json::to_string(&event).expect("event is serializable");
            line.push('\n');
            f.write_all(line.as_bytes())?;
            f.flush()?;
        }
        self.events.push(event);
        let recorded = self.events.last().expect("just pushed");
        if let Some(sink) = &self.sink {
            sink(recorded);
        }
        Ok(recorded)
    }

    /// Échanges Jev enregistrés, dans l'ordre : de quoi construire un `ReplayProvider` (§33).
    pub fn exchanges(&self) -> Vec<Exchange> {
        self.events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::DecisionCompleted { exchange, .. } => Some((**exchange).clone()),
                _ => None,
            })
            .collect()
    }

    /// Historique des Forms de la session (`form.committed`).
    pub fn forms(&self) -> Vec<&Form> {
        self.events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::FormCommitted { snapshot, .. } => Some(snapshot.as_ref()),
                _ => None,
            })
            .collect()
    }

    /// Noms d'événements dans l'ordre (`"verification.failed"`…), pratique pour les tests et `agon inspect`.
    pub fn kinds(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| {
                serde_json::to_value(&e.kind)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(String::from))
                    .unwrap_or_default()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agon-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn events_are_numbered_and_carry_the_active_form() {
        let mut s = Session::in_memory("s1");
        let f = agon_core::fixtures::base_form().id();
        s.record(
            None,
            EventKind::SessionStarted {
                agon: "0.1.0".into(),
            },
        )
        .unwrap();
        s.record(
            Some(f.clone()),
            EventKind::MutationApproved {
                mutation: MutationId("m-001".into()),
            },
        )
        .unwrap();
        assert_eq!(s.events()[0].seq, 0);
        assert_eq!(s.events()[1].seq, 1);
        assert_eq!(s.events()[1].form, Some(f));
        assert_eq!(s.kinds(), ["session.started", "mutation.approved"]);
    }

    #[test]
    fn session_roundtrips_through_disk_and_can_be_resumed() {
        let dir = scratch("roundtrip");
        let mut s = Session::create(&dir, "s1").unwrap();
        s.record(
            None,
            EventKind::SessionStarted {
                agon: "0.1.0".into(),
            },
        )
        .unwrap();
        s.record(
            None,
            EventKind::FormCommitted {
                snapshot: Box::new(agon_core::fixtures::base_form()),
                parent: None,
            },
        )
        .unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let mut loaded = Session::load(&path).unwrap();
        assert_eq!(loaded.events().len(), 2);
        assert_eq!(loaded.forms().len(), 1);
        loaded
            .record(
                None,
                EventKind::MutationValidated {
                    mutation: MutationId("m-001".into()),
                },
            )
            .unwrap();
        drop(loaded);

        let again = Session::load(&path).unwrap();
        assert_eq!(
            again.kinds(),
            ["session.started", "form.committed", "mutation.validated"]
        );
        assert_eq!(again.id(), "s1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_event_payload_field_collides_with_the_envelope() {
        // L'enveloppe est aplatie avec le payload : un champ `seq`, `ts_ms` ou `form` dans une
        // variante corromprait la sérialisation. On sérialise un exemple de chaque variante à champs.
        let f = agon_core::fixtures::base_form();
        let sample = EventKind::FormCommitted {
            snapshot: Box::new(f.clone()),
            parent: Some(f.id()),
        };
        let v = serde_json::to_value(&sample).unwrap();
        for reserved in ["seq", "ts_ms", "form"] {
            assert!(
                v.get(reserved).is_none(),
                "payload field `{reserved}` collides with the event envelope"
            );
        }
    }

    #[test]
    fn the_sink_sees_every_event_after_it_is_journaled() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = scratch("sink");
        let mut s = Session::create(&dir, "s1").unwrap();
        let (seen2, path) = (seen.clone(), s.path().unwrap().to_path_buf());
        s.set_sink(move |e| {
            // Au moment où l'observateur est appelé, la ligne est déjà sur disque.
            let on_disk = std::fs::read_to_string(&path).unwrap().lines().count();
            seen2.lock().unwrap().push((e.seq, on_disk));
        });
        s.record(None, EventKind::SessionStarted { agon: "x".into() })
            .unwrap();
        s.record(
            None,
            EventKind::MutationApproved {
                mutation: MutationId("m-001".into()),
            },
        )
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![(0, 1), (1, 2)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn session_journals_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("mode");
        let s = Session::create(&dir, "s1").unwrap();
        let mode = std::fs::metadata(s.path().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creating_an_existing_session_never_overwrites_it() {
        let dir = scratch("nooverwrite");
        Session::create(&dir, "s1").unwrap();
        assert!(Session::create(&dir, "s1").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_line_is_reported_with_its_number() {
        let dir = scratch("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.jsonl");
        std::fs::write(&p, "{\"seq\":0,\"ts_ms\":0,\"form\":null,\"type\":\"session.started\",\"agon\":\"x\"}\nnot json\n").unwrap();
        assert!(matches!(
            Session::load(&p),
            Err(SessionError::Corrupt { line: 2, .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
