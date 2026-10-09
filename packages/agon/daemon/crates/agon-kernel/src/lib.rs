//! Kernel : boucle agent, sessions, activation, exécution des outils (§4).
//! Seul crate (avec `agon-model`) autorisé à effectuer des E/S système ou réseau.

pub mod activation;
pub mod candidates;
pub mod events;
mod kernel;
pub mod runner;
pub mod state;
pub mod tools;

pub use activation::{ActivationError, Activator, CapabilitySpec, CommandActivator, NoopActivator};
pub use candidates::{CandidateCatalog, CandidateRule};
pub use events::{Event, EventKind, Phase, Session, SessionError};
pub use kernel::looks_like_plan;
pub use kernel::{
    Agent, AgentConfig, CancelToken, ChatOutcome, Conversation, HumanReason, Kernel, KernelConfig,
    KernelError, KernelParts, Mode, Outcome, Proof, TaskOutcome, TaskReason, Tier, Tiers,
};
pub use runner::{ProcessRunner, kill_active_processes};
