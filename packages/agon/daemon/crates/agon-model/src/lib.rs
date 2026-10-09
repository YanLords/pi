//! Modèles : `DecisionProvider` (Jev, TypeSafe System One) et, plus tard, `ModelProvider`
//! (LLM génératifs). Seul crate avec le Kernel autorisé à faire des E/S réseau (§37.1).

pub mod chat;
pub mod jev;
pub mod provider;
pub mod transport;

pub use chat::{
    ChatClient, ChatConfig, ChatRequest, ChatResponse, Message, ModelError, ModelProvider,
    ToolCall, ToolSpec,
};
pub use jev::{JevClient, JevConfig, JevError};
pub use provider::{DecisionProvider, Exchange, Recorder, ReplayProvider, request_digest};
