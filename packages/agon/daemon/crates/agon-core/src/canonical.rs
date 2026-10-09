//! Sérialisation canonique (clés triées, UTF-8, sans espaces) et hachage SHA-256 (§10).

use crate::CoreError;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use std::fmt;

/// Empreinte SHA-256 affichée sous la forme `sha256:<hex>`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Digest(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Préfixe court lisible (ex. `7a3c91be`), pour l'affichage.
    pub fn short(&self) -> &str {
        let hex = &self.0["sha256:".len()..];
        &hex[..8.min(hex.len())]
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// JSON canonique : `serde_json::Value` trie les clés (BTreeMap) tant que la
/// feature `preserve_order` n'est pas activée ; ne jamais l'activer dans ce workspace.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<String, CoreError> {
    let v = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&v)?)
}

pub fn sha256_of<T: Serialize>(value: &T) -> Result<Digest, CoreError> {
    Ok(Digest::of_bytes(canonical_json(value)?.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_change_hash() {
        let a = json!({"b": 1, "a": {"y": 2, "x": 3}});
        let b = json!({"a": {"x": 3, "y": 2}, "b": 1});
        assert_eq!(sha256_of(&a).unwrap(), sha256_of(&b).unwrap());
    }

    #[test]
    fn canonical_has_no_whitespace() {
        assert_eq!(
            canonical_json(&json!({"b": 1, "a": [1, 2]})).unwrap(),
            r#"{"a":[1,2],"b":1}"#
        );
    }

    #[test]
    fn short_is_eight_hex() {
        let d = Digest::of_bytes(b"agon");
        assert_eq!(d.short().len(), 8);
        assert!(d.as_str().starts_with("sha256:"));
    }
}
