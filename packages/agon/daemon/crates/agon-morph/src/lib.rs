//! Morph : moteur d'adaptation déterministe (§8). Fonction pure, sans E/S, sans LLM.
//! `Morph(parent Form, mutation) → candidate Form` ; l'activation relève du Kernel (§8.3).

mod apply;
mod history;
mod registry;

pub use apply::apply;
pub use history::History;
pub use registry::{Registry, Status};

use agon_core::{CheckId, FormId};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum MorphError {
    #[error("mutation targets parent {expected} but the form is {actual}")]
    ParentMismatch { expected: String, actual: String },
    #[error("malformed operation on {0}")]
    MalformedOp(String),
    #[error("check `{0}` is not defined in the catalog")]
    UnknownCheck(CheckId),
    #[error("check `{0}` is already active with a different definition")]
    CheckRedefinition(CheckId),
    #[error("checks are append-only: only `add` is allowed")]
    ChecksAreAppendOnly,
    #[error("form {0} is not an ancestor of the current form")]
    NotAnAncestor(String),
    #[error("unknown form {0}")]
    UnknownForm(String),
}

pub(crate) fn display(id: &FormId) -> String {
    id.display()
}
