use std::{collections::BTreeMap, fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{paths::AppPaths, types::Provider};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_max_indexed_bytes")]
    pub max_indexed_bytes_per_message: usize,
    #[serde(default = "default_index_recent_days")]
    pub index_recent_days: Option<i64>,
    #[serde(default = "default_index_max_files")]
    pub index_max_files: usize,
    #[serde(default)]
    pub providers: ProviderConfigs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfigs {
    pub codex: ProviderConfig,
    pub claude: ProviderConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub command: String,
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Vec<String>>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            max_indexed_bytes_per_message: default_max_indexed_bytes(),
            index_recent_days: default_index_recent_days(),
            index_max_files: default_index_max_files(),
            providers: ProviderConfigs::default(),
        }
    }
}

impl Default for ProviderConfigs {
    fn default() -> Self {
        let mut codex_profiles = BTreeMap::new();
        codex_profiles.insert("default".to_string(), vec![]);
        codex_profiles.insert(
            "safe".to_string(),
            vec![
                "-s".to_string(),
                "workspace-write".to_string(),
                "-a".to_string(),
                "on-request".to_string(),
            ],
        );
        codex_profiles.insert("yolo".to_string(), vec!["--yolo".to_string()]);

        let mut claude_profiles = BTreeMap::new();
        claude_profiles.insert("default".to_string(), vec![]);
        claude_profiles.insert("safe".to_string(), vec![]);
        claude_profiles.insert(
            "yolo".to_string(),
            vec!["--dangerously-skip-permissions".to_string()],
        );

        Self {
            codex: ProviderConfig {
                command: "codex".to_string(),
                resume_args: vec![
                    "resume".to_string(),
                    "{profile_args}".to_string(),
                    "{session_id}".to_string(),
                    "{prompt}".to_string(),
                ],
                profiles: codex_profiles,
            },
            claude: ProviderConfig {
                command: "claude".to_string(),
                resume_args: vec![
                    "{profile_args}".to_string(),
                    "--resume".to_string(),
                    "{session_id}".to_string(),
                    "{prompt}".to_string(),
                ],
                profiles: claude_profiles,
            },
        }
    }
}

impl AppConfig {
    pub fn load(paths: &AppPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("failed to read {}", paths.config_file.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", paths.config_file.display()))
    }

    pub fn provider(&self, provider: Provider) -> &ProviderConfig {
        match provider {
            Provider::Codex => &self.providers.codex,
            Provider::Claude => &self.providers.claude,
        }
    }

    pub fn config_path(paths: &AppPaths) -> PathBuf {
        paths.config_file.clone()
    }
}

fn default_max_indexed_bytes() -> usize {
    256 * 1024
}

fn default_index_recent_days() -> Option<i64> {
    Some(90)
}

fn default_index_max_files() -> usize {
    600
}
