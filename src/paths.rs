use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub cache_dir: PathBuf,
    pub index_dir: PathBuf,
    pub segments_dir: PathBuf,
    pub manifest_file: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "lastai", "lastai")
            .context("failed to discover OS project directories")?;
        let config_dir = dirs.config_dir().to_path_buf();
        let cache_dir = dirs.cache_dir().to_path_buf();
        let index_dir = cache_dir.join("index");
        let segments_dir = index_dir.join("segments");
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            cache_dir,
            index_dir: index_dir.clone(),
            segments_dir,
            manifest_file: index_dir.join("manifest.json"),
        })
    }
}
