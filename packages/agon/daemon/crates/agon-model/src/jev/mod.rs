//! Client et types de l'API System One (TypeSafe), directe ou via OpenRouter.

mod client;
pub mod gating;
pub mod questions;
mod types;
mod validate;

#[cfg(test)]
pub(crate) mod testing;

pub use client::{JevClient, JevConfig};
pub use types::{Answer, Question, Request, Response, Usage};

#[derive(Debug, thiserror::Error)]
pub enum JevError {
    /// Requête invalide côté Agon : rien n'a été envoyé.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Rien n'a pu être envoyé parce que la configuration est incomplète (clé d'API absente ou vide).
    #[error("{0}")]
    NotConfigured(String),
    /// Erreur non transitoire renvoyée par l'API (400, 401, 402, 403, 404, 413…).
    #[error("jev api error {status}: {message}")]
    Api { status: u16, message: String },
    /// Service injoignable ou en surcharge après épuisement des tentatives (§6.6).
    #[error("jev unavailable after {attempts} attempt(s): {last}")]
    Unavailable { attempts: u32, last: String },
    /// Réponse reçue mais incohérente avec la requête : jamais utilisée.
    #[error("invalid jev response: {0}")]
    InvalidResponse(String),
    #[error("cannot decode jev response: {0}")]
    Decode(String),
    #[error("replay mismatch: {0}")]
    ReplayMismatch(String),
}
