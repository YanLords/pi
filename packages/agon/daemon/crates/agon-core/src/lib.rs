//! Types fondamentaux d'Agon : sérialisation canonique, Form, lockfile,
//! checks, signatures, mutations. Aucune E/S, aucune dépendance vers un autre crate Agon.

pub mod canonical;
pub mod check;
#[cfg(any(test, feature = "test-util"))]
pub mod fixtures;
pub mod form;
pub mod lock;
pub mod mutation;
pub mod signature;

pub use canonical::{Digest, canonical_json, sha256_of};
pub use check::{Check, CheckCatalog, CheckId};
pub use form::{Form, FormId};
pub use lock::{Lockfile, Precision};
pub use mutation::{Mutation, MutationId, Op, Target};
pub use signature::{Signature, SignatureId};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("serialization: {0}")]
    Serialize(#[from] serde_json::Error),
}
