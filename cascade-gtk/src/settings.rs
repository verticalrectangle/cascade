use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_cloud_url")]
    pub cloud_url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default = "default_backend")]
    pub last_backend: String,
}

fn default_cloud_url() -> String {
    "https://wickrunner.com:7700".to_string()
}

fn default_backend() -> String {
    "cloud".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cloud_url: default_cloud_url(),
            token: None,
            last_backend: default_backend(),
        }
    }
}

impl Settings {
    pub fn config_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".config/cascade")
    }

    pub fn path() -> PathBuf {
        Self::config_dir().join("settings.json")
    }

    pub fn registry_path() -> PathBuf {
        Self::config_dir().join("registry.sqlite")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        fs::create_dir_all(Self::config_dir())?;
        fs::write(Self::path(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
