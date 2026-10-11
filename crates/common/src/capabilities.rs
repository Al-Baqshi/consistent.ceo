//! Identifiers used by the permission system: what an agent can *do*
//! ([`CapabilityId`]) and how far it's allowed to act without a human in the
//! loop ([`ExecutionLevel`]). Both round-trip through a string form so they
//! can be stored in config/DB and sent over the wire.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// A single permission an agent/tool may require, e.g. reading the filesystem
/// or publishing something publicly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityId {
    FsRead,
    FsWrite,
    ShellExecSafe,
    ShellExecPrivileged,
    HttpFetch,
    MessageInternal,
    MessageExternal,
    PublishPublic,
}

impl CapabilityId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::ShellExecSafe => "shell.exec.safe",
            Self::ShellExecPrivileged => "shell.exec.privileged",
            Self::HttpFetch => "http.fetch",
            Self::MessageInternal => "message.internal",
            Self::MessageExternal => "message.external",
            Self::PublishPublic => "publish.public",
        }
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

impl FromStr for CapabilityId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "fs.read" => Ok(Self::FsRead),
            "fs.write" => Ok(Self::FsWrite),
            "shell.exec.safe" => Ok(Self::ShellExecSafe),
            "shell.exec.privileged" => Ok(Self::ShellExecPrivileged),
            "http.fetch" => Ok(Self::HttpFetch),
            "message.internal" => Ok(Self::MessageInternal),
            "message.external" => Ok(Self::MessageExternal),
            "publish.public" => Ok(Self::PublishPublic),
            other => Err(format!("unknown capability: {}", other)),
        }
    }
}

impl Serialize for CapabilityId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CapabilityId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// How autonomously an agent may act, ordered roughly from passive to
/// highest-risk: observing, drafting, acting internally, notifying, and so on
/// up to publishing externally or running sensitive commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionLevel {
    Observe,
    Draft,
    InternalAct,
    Notify,
    Schedule,
    Coordinate,
    ExternalPublish,
    SensitiveExec,
}

impl ExecutionLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Draft => "draft",
            Self::InternalAct => "internal_act",
            Self::Notify => "notify",
            Self::Schedule => "schedule",
            Self::Coordinate => "coordinate",
            Self::ExternalPublish => "external_publish",
            Self::SensitiveExec => "sensitive_exec",
        }
    }
}

impl fmt::Display for ExecutionLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

impl FromStr for ExecutionLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "observe" => Ok(Self::Observe),
            "draft" => Ok(Self::Draft),
            "internal_act" => Ok(Self::InternalAct),
            "notify" => Ok(Self::Notify),
            "schedule" => Ok(Self::Schedule),
            "coordinate" => Ok(Self::Coordinate),
            "external_publish" => Ok(Self::ExternalPublish),
            "sensitive_exec" => Ok(Self::SensitiveExec),
            other => Err(format!("unknown execution level: {}", other)),
        }
    }
}

impl Serialize for ExecutionLevel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ExecutionLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}
