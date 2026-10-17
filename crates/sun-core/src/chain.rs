//! [`ChainEntry`]: one runtime of an agent's or workflow's failover chain
//! (`runtime.chain`), as users write it in the API and in workflow YAML.
//!
//! **This shape is a one-way door** (TRD BLO-117 §7): it lands in YAML users
//! commit. A bare string is a runtime name, as it always was. An object
//! names the tool and, optionally, whose account pays (`credential`), the
//! host it must run on (`host`), the model and the effort level. No other
//! field is accepted.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{EffortCatalog, EffortLevel};

/// One runtime of a failover chain: `"claude-code"` or
/// `{tool, credential?, host?, model?, effort?}`. Serializes back in the
/// shape it was written in, so a chain round-trips unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ChainEntry {
    /// A runtime name: a tool id (`claude-code`, which then runs on the
    /// CLI's own login on any host), the name of one of the workspace's
    /// key-backed runtimes (`ollama-qwen`), `market:<adapter>` or
    /// `<extension>.<adapter>`.
    Name(String),
    /// A tool with its settings.
    Spec(ChainSpec),
}

/// A string or a [`ChainSpec`] map. Not `untagged`: a bad map reports its
/// own field error (`unknown field `x`, expected one of tool, ...`).
impl<'de> Deserialize<'de> for ChainEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ChainEntry;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a runtime name or {tool, credential, host, model, effort}")
            }

            fn visit_str<E: serde::de::Error>(self, name: &str) -> Result<ChainEntry, E> {
                Ok(ChainEntry::Name(name.to_owned()))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<ChainEntry, A::Error> {
                let spec =
                    ChainSpec::deserialize(serde::de::value::MapAccessDeserializer::new(map));
                spec.map(ChainEntry::Spec)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// The object form of a [`ChainEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainSpec {
    /// The tool (`RuntimeAdapter::id`), e.g. `codex`.
    pub tool: String,
    /// The name of the workspace's key-backed runtime that pays for it
    /// (today's `byo_key`/`credits` runtime rows; they keep their names
    /// when they become credentials). `None`: the CLI's own login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The id of the host it must run on. `None`: any eligible host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The model id passed to the CLI. `None`: the CLI's (or key's) default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// One of the effort tokens the host reported for the tool and model
    /// ([`EffortCatalog`]). `None`: the CLI's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl ChainEntry {
    /// The entry's runtime name: what failover, caps, cooldowns and the
    /// journal call it. The credential's name when it has one, else the
    /// tool's. Unique within a chain.
    pub fn name(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Spec(spec) => spec.credential.as_deref().unwrap_or(&spec.tool),
        }
    }

    /// The object form, when written as one.
    pub fn spec(&self) -> Option<&ChainSpec> {
        match self {
            Self::Name(_) => None,
            Self::Spec(spec) => Some(spec),
        }
    }

    /// The host it is pinned to, if any.
    pub fn host(&self) -> Option<&str> {
        self.spec().and_then(|s| s.host.as_deref())
    }

    /// The model it asks for, if any.
    pub fn model(&self) -> Option<&str> {
        self.spec().and_then(|s| s.model.as_deref())
    }

    /// The effort level it asks for, if any.
    pub fn effort(&self) -> Option<&str> {
        self.spec().and_then(|s| s.effort.as_deref())
    }
}

impl From<&str> for ChainEntry {
    fn from(name: &str) -> Self {
        Self::Name(name.to_owned())
    }
}

impl From<String> for ChainEntry {
    fn from(name: String) -> Self {
        Self::Name(name)
    }
}

/// A bare name equals its string (`chain == ["claude-code", "codex"]`).
impl PartialEq<&str> for ChainEntry {
    fn eq(&self, other: &&str) -> bool {
        matches!(self, Self::Name(name) if name == other)
    }
}

impl fmt::Display for ChainEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The runtime names of `chain`, in order ([`ChainEntry::name`]).
pub fn chain_names(chain: &[ChainEntry]) -> Vec<String> {
    chain.iter().map(|e| e.name().to_owned()).collect()
}

impl EffortCatalog {
    /// The levels a session of `model` may use: that model's own list when
    /// the CLI scopes effort per model and lists it, else the tool-wide one.
    pub fn options_for(&self, model: Option<&str>) -> &[EffortLevel] {
        model
            .and_then(|m| self.models.iter().find(|e| e.id == m))
            .map_or(&self.effort_options, |m| &m.effort_options)
    }

    /// `effort` is one of the levels [`options_for`](Self::options_for)
    /// `model` lists.
    pub fn allows(&self, model: Option<&str>, effort: &str) -> bool {
        self.options_for(model).iter().any(|l| l.value == effort)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelEffort;

    #[test]
    fn entries_round_trip_in_the_shape_they_were_written() {
        let json = r#"["claude-code",{"tool":"opencode","credential":"ollama-qwen","model":"qwen2.5-coder:7b"},{"tool":"codex","host":"h1","effort":"high"}]"#;
        let chain: Vec<ChainEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(chain_names(&chain), ["claude-code", "ollama-qwen", "codex"]);
        assert_eq!(chain[2].host(), Some("h1"));
        assert_eq!(chain[2].effort(), Some("high"));
        assert_eq!(chain[1].model(), Some("qwen2.5-coder:7b"));
        assert_eq!(serde_json::to_string(&chain).unwrap(), json);
    }

    #[test]
    fn an_entry_takes_no_field_beyond_the_five() {
        let extra = r#"{"tool":"codex","permission_mode":"plan"}"#;
        assert!(serde_json::from_str::<ChainEntry>(extra).is_err());
        assert!(serde_json::from_str::<ChainEntry>(r#"{"model":"x"}"#).is_err());
        assert!(serde_json::from_str::<ChainEntry>("3").is_err());
    }

    #[test]
    fn effort_is_checked_per_model_where_the_tool_scopes_it() {
        let level = |v: &str| EffortLevel {
            value: v.into(),
            label: v.into(),
            description: None,
        };
        let catalog = EffortCatalog {
            effort_options: vec![level("low"), level("high"), level("xhigh")],
            models: vec![ModelEffort {
                id: "gpt-5-mini".into(),
                label: "mini".into(),
                effort_options: vec![level("low")],
                default_effort: None,
            }],
        };
        assert!(catalog.allows(None, "xhigh"));
        assert!(catalog.allows(Some("gpt-5-codex"), "xhigh"));
        assert!(!catalog.allows(Some("gpt-5-mini"), "high"));
        assert!(!EffortCatalog::default().allows(None, "low"));
    }
}
