use std::{fmt, path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ValueEnum,
)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Codex,
    Claude,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            other => Err(format!("unknown provider: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
    Other,
}

impl Role {
    pub fn from_label(label: &str) -> Self {
        match label.to_ascii_lowercase().as_str() {
            "user" | "human" => Self::User,
            "assistant" => Self::Assistant,
            "system" => Self::System,
            "tool" | "tool_result" | "tool-result" | "function" => Self::Tool,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Tool => "tool",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub path: PathBuf,
    pub byte_offset: u64,
    pub line_number: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDoc {
    pub provider: Provider,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub timestamp: Option<DateTime<Utc>>,
    pub role: Role,
    pub text: String,
    pub source: SourceRef,
    pub is_sidechain: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub provider: Provider,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub timestamp: Option<DateTime<Utc>>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    pub role: Role,
    pub timestamp: Option<DateTime<Utc>>,
    pub text: String,
    pub source: SourceRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHit {
    pub provider: Provider,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub timestamp: Option<DateTime<Utc>>,
    pub score: f32,
    pub snippets: Vec<Snippet>,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub limit: usize,
    pub default_sidechain: Option<bool>,
    pub current_cwd: Option<PathBuf>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 50,
            default_sidechain: None,
            current_cwd: std::env::current_dir().ok(),
        }
    }
}
