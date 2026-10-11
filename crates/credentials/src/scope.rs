//! Where a secret belongs.
//!
//! The vault holds one flat `KEY=value` space per *scope*, not one for the whole
//! account. An entity that runs with its own Stripe account, its own OpenAI
//! project and its own database gets those under `entity:{id}`, and nothing it
//! runs can see another entity's copy of the same key name.
//!
//! # Resolution
//!
//! Lookups fall back exactly one step, to [`Scope::Global`]:
//!
//! ```text
//! entity:acme  ->  OPENAI_API_KEY  (entity's own key, if set)
//!              ->  OPENAI_API_KEY  (global, otherwise)
//! ```
//!
//! One step, never a chain. A deeper hierarchy would mean that reading a key
//! requires knowing the whole tree to predict which value wins, and the cost of
//! guessing wrong with credentials is charging the wrong customer's card. Shared
//! defaults live in global; anything an entity must not inherit is set on the
//! entity, where it shadows global.
//!
//! # Names
//!
//! Scope strings are `global`, `entity:{id}` or `project:{name}`, and the id half
//! is restricted to `[A-Za-z0-9._-]`. That is not cosmetic: scopes are map keys
//! and appear in export files, so an id containing `:` could otherwise be crafted
//! to read as a different scope than the one it was stored under.

use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// String form of [`Scope::Global`].
pub const GLOBAL: &str = "global";
const ENTITY_PREFIX: &str = "entity:";
const PROJECT_PREFIX: &str = "project:";

/// The scope a secret or item is stored under.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Account-wide defaults, and the CEO's own personal items. Every other
    /// scope falls back here for *environment resolution*; password items do not
    /// fall back — each scope is its own filing cabinet.
    #[default]
    Global,
    /// Belongs to one entity, keyed by entity id.
    Entity(String),
    /// Belongs to one project, keyed by project name.
    Project(String),
}

impl Scope {
    /// Build an entity scope, rejecting an `id` that could be misread as
    /// something other than the scope it names.
    pub fn entity(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        validate_name(&id)?;
        Ok(Self::Entity(id))
    }

    /// Build a project scope, subject to the same name validation as [`Self::entity`].
    pub fn project(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self::Project(name))
    }

    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }

    /// The scope consulted when this one has no value for a key, or `None` for
    /// [`Scope::Global`], which is where the search stops.
    pub fn fallback(&self) -> Option<Scope> {
        match self {
            Self::Global => None,
            _ => Some(Self::Global),
        }
    }

    /// This scope, then everything it falls back to, in lookup order.
    pub fn resolution_order(&self) -> Vec<Scope> {
        match self.fallback() {
            Some(next) => vec![self.clone(), next],
            None => vec![self.clone()],
        }
    }

    /// A short human label, for UI headings.
    pub fn label(&self) -> String {
        match self {
            Self::Global => "Global".to_string(),
            Self::Entity(id) => format!("Entity · {id}"),
            Self::Project(name) => format!("Project · {name}"),
        }
    }
}

/// Scope names are map keys and appear in exports; keep them unambiguous.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("scope name must not be empty");
    }
    if name.len() > 128 {
        bail!("scope name must be 128 characters or fewer");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        bail!("scope name may not contain {bad:?}; use letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => f.write_str(GLOBAL),
            Self::Entity(id) => write!(f, "{ENTITY_PREFIX}{id}"),
            Self::Project(name) => write!(f, "{PROJECT_PREFIX}{name}"),
        }
    }
}

impl FromStr for Scope {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case(GLOBAL) || s.is_empty() {
            return Ok(Self::Global);
        }
        if let Some(id) = s.strip_prefix(ENTITY_PREFIX) {
            return Self::entity(id);
        }
        if let Some(name) = s.strip_prefix(PROJECT_PREFIX) {
            return Self::project(name);
        }
        bail!("unknown scope {s:?}; expected \"global\", \"entity:<id>\" or \"project:<name>\"")
    }
}

/// Stored as its string form so the vault payload stays readable once decrypted,
/// and so an unknown scope in a future file is a parse error rather than silent
/// data loss.
impl Serialize for Scope {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// Environment-variable key names, validated the way a shell would.
///
/// Rejecting `=`, whitespace and a leading digit means an exported `.env` file
/// round-trips, and means a key can never be crafted to inject a second
/// assignment into a generated environment.
pub fn validate_env_key(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("environment variable name must not be empty");
    }
    if key.len() > 256 {
        bail!("environment variable name must be 256 characters or fewer");
    }
    if key.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        bail!("environment variable name must not start with a digit");
    }
    if let Some(bad) = key
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.')))
    {
        bail!("environment variable name may not contain {bad:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_round_trip_through_their_string_form() {
        let cases = [
            (Scope::Global, "global"),
            (Scope::Entity("acme".into()), "entity:acme"),
            (Scope::Project("world-1".into()), "project:world-1"),
        ];
        for (scope, text) in cases {
            assert_eq!(scope.to_string(), text);
            assert_eq!(text.parse::<Scope>().unwrap(), scope);
        }
    }

    #[test]
    fn an_empty_or_odd_cased_scope_is_global() {
        assert_eq!("".parse::<Scope>().unwrap(), Scope::Global);
        assert_eq!("GLOBAL".parse::<Scope>().unwrap(), Scope::Global);
        assert_eq!("  global  ".parse::<Scope>().unwrap(), Scope::Global);
    }

    #[test]
    fn resolution_falls_back_to_global_and_stops() {
        assert_eq!(
            Scope::entity("acme").unwrap().resolution_order(),
            vec![Scope::Entity("acme".into()), Scope::Global]
        );
        // Global is the end of the chain, not a step in it.
        assert_eq!(Scope::Global.resolution_order(), vec![Scope::Global]);
        assert_eq!(Scope::Global.fallback(), None);
    }

    #[test]
    fn a_scope_id_cannot_impersonate_another_scope() {
        // Without validation, an entity literally named "x" plus a crafted
        // suffix could serialize to a string that parses as a different scope.
        for hostile in [
            "a:b",
            "acme:evil",
            "../../etc",
            "has space",
            "entity:nested",
            "",
        ] {
            assert!(
                Scope::entity(hostile).is_err(),
                "{hostile:?} should be refused"
            );
        }
    }

    #[test]
    fn unknown_scope_forms_are_refused_rather_than_guessed() {
        for bad in ["tenant:x", "entity", "wat", "entity:has space"] {
            assert!(bad.parse::<Scope>().is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn env_keys_reject_shell_hostile_names() {
        for good in ["OPENAI_API_KEY", "port", "a1", "my.key", "_leading"] {
            assert!(validate_env_key(good).is_ok(), "{good:?} should be allowed");
        }
        for bad in ["", "1LEADING", "HAS SPACE", "A=B", "A\nB", "A;B", "A$B"] {
            assert!(validate_env_key(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(Scope::Global.label(), "Global");
        assert_eq!(Scope::entity("acme").unwrap().label(), "Entity · acme");
    }
}
