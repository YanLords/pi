//! Verification : source de vérité (§15). Ce crate est pur : il définit les observations, la
//! condition de succès, les extracteurs de signatures et le protocole de recheck. L'exécution
//! réelle des commandes est fournie par le Kernel via le trait [`CheckRunner`].

mod condition;
mod extractor;
mod observation;
mod protocol;

pub use condition::{Condition, evaluate};
pub use extractor::{ExtractorDef, ExtractorSet};
pub use observation::{CheckRunner, Observation};
pub use protocol::{Causality, Evidence, Reproduction, causality, reproduction, run_check};

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("unsupported success condition `{0}` (expected `exit_code == N` or `exit_code != N`)")]
    UnsupportedCondition(String),
    #[error("invalid extractor `{name}`: {reason}")]
    InvalidExtractor { name: String, reason: String },
    #[error("extractor `{0}` is defined twice")]
    DuplicateExtractor(String),
    #[error("extractor file: {0}")]
    Toml(#[from] toml::de::Error),
}
