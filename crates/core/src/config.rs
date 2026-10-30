use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const APP: &str = "gdsync";

pub fn config_dir() -> PathBuf {
    dirs::config_dir().expect("sin directorio de configuración").join(APP)
}
pub fn data_dir() -> PathBuf {
    dirs::data_dir().expect("sin directorio de datos").join(APP)
}
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().expect("sin directorio de caché").join(APP)
}
pub fn db_path() -> PathBuf {
    data_dir().join("state.db")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_mount")]
    pub mount_point: PathBuf,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Max size of the content cache (pinned files are never evicted).
    #[serde(default = "default_cache_gb")]
    pub cache_max_gb: u64,
    /// Folders selected for sync (defaults to ["root"] for full drive).
    #[serde(default = "default_sync_folders")]
    pub sync_folders: Vec<String>,
}

fn default_mount() -> PathBuf {
    dirs::home_dir().expect("sin HOME").join("GoogleDrive")
}
fn default_poll() -> u64 {
    30
}
fn default_cache_gb() -> u64 {
    10
}
fn default_sync_folders() -> Vec<String> {
    vec!["root".into()]
}

impl Config {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
            mount_point: default_mount(),
            poll_interval_secs: default_poll(),
            cache_max_gb: default_cache_gb(),
            sync_folders: default_sync_folders(),
        }
    }
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }
    pub fn load() -> Result<Self> {
        let s = fs::read_to_string(Self::path()).with_context(|| {
            "no hay configuración; ejecutá `gdsync login --client-id ... --client-secret ...`"
        })?;
        Ok(serde_json::from_str(&s)?)
    }
    pub fn save(&self) -> Result<()> {
        write_private(&Self::path(), &serde_json::to_vec_pretty(self)?)
    }

    pub fn is_syncing_all(&self) -> bool {
        self.sync_folders.is_empty() || self.sync_folders.iter().any(|f| f == "root")
    }

    pub fn add_sync_folder(&mut self, folder_id: String) -> bool {
        if self.sync_folders == vec!["root".to_string()] {
            self.sync_folders.clear();
        }
        if !self.sync_folders.contains(&folder_id) {
            self.sync_folders.push(folder_id);
            true
        } else {
            false
        }
    }

    pub fn remove_sync_folder(&mut self, folder_id: &str) -> bool {
        let prev_len = self.sync_folders.len();
        self.sync_folders.retain(|f| f != folder_id);
        let removed = self.sync_folders.len() != prev_len;
        if self.sync_folders.is_empty() {
            self.sync_folders.push("root".into());
        }
        removed
    }

    pub fn reset_sync_folders(&mut self) {
        self.sync_folders = vec!["root".into()];
    }
}

/// Atomically writes a file readable only by the current user.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_folder_selection() {
        let mut cfg = Config::new("test_id".into(), "test_secret".into());
        assert!(cfg.is_syncing_all());
        assert_eq!(cfg.sync_folders, vec!["root".to_string()]);

        // Adding folder 1 replaces root
        assert!(cfg.add_sync_folder("folder_a".into()));
        assert!(!cfg.is_syncing_all());
        assert_eq!(cfg.sync_folders, vec!["folder_a".to_string()]);

        // Adding second folder
        assert!(cfg.add_sync_folder("folder_b".into()));
        assert_eq!(cfg.sync_folders, vec!["folder_a".to_string(), "folder_b".to_string()]);

        // Adding existing folder returns false
        assert!(!cfg.add_sync_folder("folder_a".into()));

        // Removing folder_a
        assert!(cfg.remove_sync_folder("folder_a"));
        assert_eq!(cfg.sync_folders, vec!["folder_b".to_string()]);

        // Removing last folder restores root
        assert!(cfg.remove_sync_folder("folder_b"));
        assert!(cfg.is_syncing_all());
        assert_eq!(cfg.sync_folders, vec!["root".to_string()]);

        // Manual reset
        cfg.add_sync_folder("folder_c".into());
        assert!(!cfg.is_syncing_all());
        cfg.reset_sync_folders();
        assert!(cfg.is_syncing_all());
    }
}
