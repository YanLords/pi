//! Modèle génératif : chat completions compatibles OpenAI (OpenRouter, OpenAI, vLLM…) avec appel
//! d'outils. C'est le `ModelProvider` du §30, celui qui *agit* — par opposition à Jev, qui décide.

mod client;
mod stream;
mod types;

pub use client::{ChatClient, ChatConfig};
pub use types::{ChatRequest, ChatResponse, ChatUsage, FunctionCall, Message, ToolCall, ToolSpec};

use std::future::Future;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Rien n'a pu être envoyé parce que la configuration est incomplète (clé d'API absente ou vide).
    #[error("{0}")]
    NotConfigured(String),
    #[error("model api error {status}: {message}")]
    Api { status: u16, message: String },
    #[error("model unavailable after {attempts} attempt(s): {last}")]
    Unavailable { attempts: u32, last: String },
    #[error("invalid model response: {0}")]
    InvalidResponse(String),
    #[error("cannot decode model response: {0}")]
    Decode(String),
}

/// Modèle génératif : reçoit une conversation et des outils, renvoie un message (texte et/ou appels d'outils).
pub trait ModelProvider: Sync {
    fn chat(
        &self,
        request: ChatRequest,
    ) -> impl Future<Output = Result<ChatResponse, ModelError>> + Send;

    /// Comme `chat`, mais transmet le texte au fil de l'eau à `on_delta` quand le fournisseur sait
    /// streamer. Les fragments ne servent qu'à l'affichage ; la réponse rendue est complète et
    /// identique à celle de `chat`. Par défaut : pas de flux, aucun fragment.
    fn chat_stream(
        &self,
        request: ChatRequest,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> impl Future<Output = Result<ChatResponse, ModelError>> + Send {
        let _ = on_delta;
        self.chat(request)
    }
}
