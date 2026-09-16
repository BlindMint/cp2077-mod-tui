use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GameInstall {
    pub id: String,
    pub library: PathBuf,
    pub root: PathBuf,
    pub manifest: PathBuf,
    pub build_id: String,
    pub phantom_liberty: bool,
    pub redmod: bool,
    pub writable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModKind {
    Legacy,
    Redmod,
    Framework,
    Mixed,
}

impl ModKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Redmod => "redmod",
            Self::Framework => "framework",
            Self::Mixed => "mixed",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "redmod" => Self::Redmod,
            "framework" => Self::Framework,
            "mixed" => Self::Mixed,
            _ => Self::Legacy,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModRelease {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: ModKind,
    pub archive_sha256: String,
    pub layer_path: PathBuf,
    pub source: String,
    pub installed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dependency {
    pub mod_id: String,
    pub requires_id: String,
    pub inferred: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub game_install_id: String,
    pub game_build_id: String,
    pub runner: String,
    pub launch_args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SaveSet {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadoutEntry {
    pub profile_id: String,
    pub mod_id: String,
    pub priority: i64,
    pub requested_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedEntry {
    pub entry: LoadoutEntry,
    pub effective_enabled: bool,
    pub disabled_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileOwner {
    pub mod_id: String,
    pub relative_path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunRecord {
    pub id: String,
    pub profile_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub command: Vec<String>,
}
