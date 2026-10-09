//! Extracteurs déterministes observation → signature (§18.1). Pas de LLM, pas de Jev.
//!
//! Format TOML :
//! ```toml
//! [extractor.pg_connection_refused]
//! check = "verify.integration"
//! class = "connection_refused"
//! priority = 100
//!
//! [[extractor.pg_connection_refused.match]]
//! source = "exit_code"
//! operator = "not_equals"
//! value = 0
//!
//! [[extractor.pg_connection_refused.match]]
//! source = "stderr"
//! regex = 'ECONNREFUSED [0-9.]+:(?P<port>\d+)'
//!
//! [extractor.pg_connection_refused.fields]
//! port = { from = "port", type = "integer" }
//! service = { lookup = "port", table = { "5432" = "postgres" } }
//! ```

use crate::{Observation, VerifyError};
use agon_core::{CheckId, Digest, Signature, sha256_of};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    ExitCode,
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Equals,
    NotEquals,
    Contains,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchDef {
    pub source: Source,
    #[serde(default)]
    pub operator: Option<Operator>,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub regex: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    #[default]
    String,
    Integer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldDef {
    Lookup {
        lookup: String,
        table: BTreeMap<String, Value>,
    },
    Capture {
        from: String,
        #[serde(default, rename = "type")]
        ty: FieldType,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractorDef {
    pub check: CheckId,
    pub class: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default, rename = "match")]
    pub matches: Vec<MatchDef>,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldDef>,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    extractor: BTreeMap<String, ExtractorDef>,
}

#[derive(Clone)]
enum Matcher {
    ExitCode { negate: bool, code: i64 },
    Regex { source: Source, re: Regex },
    Contains { source: Source, needle: String },
}

#[derive(Clone)]
struct Compiled {
    name: String,
    def: ExtractorDef,
    matchers: Vec<Matcher>,
}

/// Ensemble d'extracteurs compilés, évalués par priorité décroissante puis par nom (déterminisme).
#[derive(Clone, Default)]
pub struct ExtractorSet {
    compiled: Vec<Compiled>,
}

fn invalid(name: &str, reason: impl Into<String>) -> VerifyError {
    VerifyError::InvalidExtractor {
        name: name.to_string(),
        reason: reason.into(),
    }
}

fn compile(name: &str, def: ExtractorDef) -> Result<Compiled, VerifyError> {
    if def.matches.is_empty() {
        return Err(invalid(name, "at least one `match` is required"));
    }
    let mut matchers = Vec::new();
    let mut groups: Vec<String> = Vec::new();
    for m in &def.matches {
        matchers.push(match (m.source, m.operator, &m.regex, &m.value) {
            (
                Source::ExitCode,
                Some(op @ (Operator::Equals | Operator::NotEquals)),
                None,
                Some(v),
            ) => {
                let code = v
                    .as_i64()
                    .ok_or_else(|| invalid(name, "exit_code value must be an integer"))?;
                Matcher::ExitCode {
                    negate: op == Operator::NotEquals,
                    code,
                }
            }
            (Source::Stdout | Source::Stderr, None, Some(pattern), None) => {
                let re = Regex::new(pattern).map_err(|e| invalid(name, format!("regex: {e}")))?;
                groups.extend(re.capture_names().flatten().map(String::from));
                Matcher::Regex {
                    source: m.source,
                    re,
                }
            }
            (
                Source::Stdout | Source::Stderr,
                Some(Operator::Contains),
                None,
                Some(Value::String(s)),
            ) => Matcher::Contains {
                source: m.source,
                needle: s.clone(),
            },
            _ => {
                return Err(invalid(
                    name,
                    format!("unsupported match on {:?}", m.source),
                ));
            }
        });
    }
    for (field, fd) in &def.fields {
        if field == "check" || field == "class" {
            return Err(invalid(name, format!("field `{field}` is reserved")));
        }
        match fd {
            FieldDef::Capture { from, .. } if !groups.contains(from) => {
                return Err(invalid(
                    name,
                    format!("field `{field}`: no capture group `{from}` in any regex"),
                ));
            }
            FieldDef::Lookup { lookup, .. }
                if lookup == field || !def.fields.contains_key(lookup) =>
            {
                return Err(invalid(
                    name,
                    format!("field `{field}`: lookup `{lookup}` must name another field"),
                ));
            }
            _ => {}
        }
    }
    Ok(Compiled {
        name: name.to_string(),
        def,
        matchers,
    })
}

fn text(obs: &Observation, source: Source) -> &str {
    match source {
        Source::Stdout => &obs.stdout,
        Source::Stderr => &obs.stderr,
        Source::ExitCode => "",
    }
}

fn render(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl Compiled {
    /// Champs extraits si l'extracteur correspond, sinon `None`.
    fn extract(&self, obs: &Observation) -> Option<BTreeMap<String, Value>> {
        if obs.check != self.def.check {
            return None;
        }
        let mut captured: BTreeMap<String, String> = BTreeMap::new();
        for m in &self.matchers {
            match m {
                Matcher::ExitCode { negate, code } => {
                    let hit = obs.exit_code.map(i64::from) == Some(*code);
                    if hit == *negate || obs.exit_code.is_none() {
                        return None;
                    }
                }
                Matcher::Contains { source, needle } => {
                    if !text(obs, *source).contains(needle.as_str()) {
                        return None;
                    }
                }
                Matcher::Regex { source, re } => {
                    let caps = re.captures(text(obs, *source))?;
                    for name in re.capture_names().flatten() {
                        if let Some(g) = caps.name(name) {
                            captured
                                .entry(name.to_string())
                                .or_insert_with(|| g.as_str().to_string());
                        }
                    }
                }
            }
        }
        let mut fields = BTreeMap::new();
        for (name, fd) in &self.def.fields {
            if let FieldDef::Capture { from, ty } = fd {
                let raw = captured.get(from)?;
                let value = match ty {
                    FieldType::String => Value::String(raw.clone()),
                    FieldType::Integer => Value::from(raw.parse::<i64>().ok()?),
                };
                fields.insert(name.clone(), value);
            }
        }
        for (name, fd) in &self.def.fields {
            if let FieldDef::Lookup { lookup, table } = fd {
                // Une clé absente de la table n'invalide pas la correspondance : le champ est omis.
                if let Some(v) = fields.get(lookup).and_then(|k| table.get(&render(k))) {
                    fields.insert(name.clone(), v.clone());
                }
            }
        }
        Some(fields)
    }
}

impl ExtractorSet {
    pub fn from_toml(src: &str) -> Result<Self, VerifyError> {
        let file: File = toml::from_str(src)?;
        Self::from_defs(file.extractor)
    }

    pub fn from_defs(defs: BTreeMap<String, ExtractorDef>) -> Result<Self, VerifyError> {
        let mut compiled = defs
            .into_iter()
            .map(|(name, def)| compile(&name, def))
            .collect::<Result<Vec<_>, _>>()?;
        compiled.sort_by(|a, b| {
            b.def
                .priority
                .cmp(&a.def.priority)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(ExtractorSet { compiled })
    }

    /// Fusionne deux ensembles (projet + plugins) ; un nom en double est une erreur.
    pub fn merge(self, other: ExtractorSet) -> Result<Self, VerifyError> {
        let mut defs: BTreeMap<String, ExtractorDef> = BTreeMap::new();
        for c in self.compiled.into_iter().chain(other.compiled) {
            if defs.insert(c.name.clone(), c.def).is_some() {
                return Err(VerifyError::DuplicateExtractor(c.name));
            }
        }
        Self::from_defs(defs)
    }

    pub fn len(&self) -> usize {
        self.compiled.len()
    }

    pub fn is_empty(&self) -> bool {
        self.compiled.is_empty()
    }

    /// Hash de la définition canonique de l'ensemble, pour le lockfile.
    pub fn definition_hash(&self) -> Digest {
        let map: BTreeMap<&String, &ExtractorDef> =
            self.compiled.iter().map(|c| (&c.name, &c.def)).collect();
        sha256_of(&map).expect("extractor definitions are always serializable")
    }

    /// Normalise une observation. Le premier extracteur qui correspond l'emporte ;
    /// sans correspondance, la signature vaut `unknown` (§18.1).
    pub fn normalize(&self, obs: &Observation) -> Signature {
        for c in &self.compiled {
            if let Some(fields) = c.extract(obs) {
                return Signature {
                    check: obs.check.clone(),
                    class: c.def.class.clone(),
                    fields,
                };
            }
        }
        Signature::unknown(obs.check.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PG: &str = r#"
[extractor.pg_connection_refused]
check = "verify.integration"
class = "connection_refused"
priority = 100

[[extractor.pg_connection_refused.match]]
source = "exit_code"
operator = "not_equals"
value = 0

[[extractor.pg_connection_refused.match]]
source = "stderr"
regex = 'ECONNREFUSED [0-9.]+:(?P<port>\d+)'

[extractor.pg_connection_refused.fields]
port = { from = "port", type = "integer" }
service = { lookup = "port", table = { "5432" = "postgres", "6379" = "redis" } }
"#;

    fn obs(check: &str, code: Option<i32>, stderr: &str) -> Observation {
        Observation {
            check: check.into(),
            exit_code: code,
            stdout: String::new(),
            stderr: stderr.into(),
            timed_out: false,
        }
    }

    #[test]
    fn cdc_example_produces_the_documented_signature() {
        let set = ExtractorSet::from_toml(PG).unwrap();
        let sig = set.normalize(&obs(
            "verify.integration",
            Some(1),
            "thread panicked: ECONNREFUSED 127.0.0.1:5432",
        ));
        assert_eq!(sig.class, "connection_refused");
        assert_eq!(sig.fields["port"], json!(5432));
        assert_eq!(sig.fields["service"], json!("postgres"));
    }

    #[test]
    fn superficial_differences_yield_the_same_signature_id() {
        let set = ExtractorSet::from_toml(PG).unwrap();
        let a = set.normalize(&obs(
            "verify.integration",
            Some(1),
            "ECONNREFUSED 127.0.0.1:5432",
        ));
        let b = set.normalize(&obs(
            "verify.integration",
            Some(101),
            "noise\nError: ECONNREFUSED 10.0.0.7:5432\nmore noise",
        ));
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn different_port_yields_a_different_signature() {
        let set = ExtractorSet::from_toml(PG).unwrap();
        let a = set.normalize(&obs(
            "verify.integration",
            Some(1),
            "ECONNREFUSED 127.0.0.1:5432",
        ));
        let b = set.normalize(&obs(
            "verify.integration",
            Some(1),
            "ECONNREFUSED 127.0.0.1:6379",
        ));
        assert_ne!(a.id(), b.id());
        assert_eq!(b.fields["service"], json!("redis"));
    }

    #[test]
    fn no_match_yields_unknown() {
        let set = ExtractorSet::from_toml(PG).unwrap();
        for o in [
            obs("verify.integration", Some(1), "assertion failed"),
            obs("verify.build", Some(1), "ECONNREFUSED 127.0.0.1:5432"), // autre check
            obs("verify.integration", Some(0), "ECONNREFUSED 127.0.0.1:5432"), // exit_code == 0
            obs("verify.integration", None, "ECONNREFUSED 127.0.0.1:5432"), // tué
        ] {
            assert!(set.normalize(&o).is_unknown(), "{o:?}");
        }
    }

    #[test]
    fn unmapped_lookup_omits_the_field_but_still_matches() {
        let set = ExtractorSet::from_toml(PG).unwrap();
        let sig = set.normalize(&obs(
            "verify.integration",
            Some(1),
            "ECONNREFUSED 127.0.0.1:9999",
        ));
        assert_eq!(sig.class, "connection_refused");
        assert!(!sig.fields.contains_key("service"));
    }

    #[test]
    fn highest_priority_wins_then_name_order() {
        let src = r#"
[extractor.generic]
check = "c"
class = "generic"
priority = 1
[[extractor.generic.match]]
source = "stderr"
operator = "contains"
value = "boom"

[extractor.specific]
check = "c"
class = "specific"
priority = 50
[[extractor.specific.match]]
source = "stderr"
operator = "contains"
value = "boom"

[extractor.a_tie]
check = "c"
class = "tie_a"
priority = 50
[[extractor.a_tie.match]]
source = "stderr"
operator = "contains"
value = "boom"
"#;
        let set = ExtractorSet::from_toml(src).unwrap();
        assert_eq!(set.normalize(&obs("c", Some(1), "boom")).class, "tie_a");
    }

    #[test]
    fn invalid_definitions_are_rejected_at_load_time() {
        let cases = [
            ("no match", "[extractor.x]\ncheck='c'\nclass='k'\n"),
            (
                "bad regex",
                "[extractor.x]\ncheck='c'\nclass='k'\n[[extractor.x.match]]\nsource='stderr'\nregex='('\n",
            ),
            (
                "regex on exit_code",
                "[extractor.x]\ncheck='c'\nclass='k'\n[[extractor.x.match]]\nsource='exit_code'\nregex='1'\n",
            ),
            (
                "unknown group",
                "[extractor.x]\ncheck='c'\nclass='k'\n[[extractor.x.match]]\nsource='stderr'\nregex='a'\n[extractor.x.fields]\np={from='nope'}\n",
            ),
            (
                "reserved field",
                "[extractor.x]\ncheck='c'\nclass='k'\n[[extractor.x.match]]\nsource='stderr'\nregex='(?P<p>a)'\n[extractor.x.fields]\nclass={from='p'}\n",
            ),
            (
                "dangling lookup",
                "[extractor.x]\ncheck='c'\nclass='k'\n[[extractor.x.match]]\nsource='stderr'\nregex='a'\n[extractor.x.fields]\ns={lookup='ghost',table={}}\n",
            ),
        ];
        for (label, src) in cases {
            assert!(
                ExtractorSet::from_toml(src).is_err(),
                "{label} must be rejected"
            );
        }
    }

    #[test]
    fn merge_rejects_duplicates_and_hash_is_stable() {
        let a = ExtractorSet::from_toml(PG).unwrap();
        let b = ExtractorSet::from_toml(PG).unwrap();
        assert_eq!(a.definition_hash(), b.definition_hash());
        assert!(matches!(
            a.merge(b),
            Err(VerifyError::DuplicateExtractor(_))
        ));
    }

    #[test]
    fn definition_change_changes_hash() {
        let a = ExtractorSet::from_toml(PG).unwrap();
        let b = ExtractorSet::from_toml(&PG.replace("priority = 100", "priority = 99")).unwrap();
        assert_ne!(a.definition_hash(), b.definition_hash());
    }
}
